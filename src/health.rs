//! Per-engine health tracking + automatic cool-down / fallback.
//!
//! Upstream engines flake: some bot-block under fan-out (HTTP 403), some time
//! out, some go down entirely. Retrying a *persistently* failing engine on
//! every request wastes a fan-out slot and slows the whole search down to that
//! engine's timeout. This module tracks consecutive **hard** failures per
//! engine and, once a configurable threshold is crossed, "cools the engine
//! down": the orchestrator skips it in fan-out for a configurable backoff
//! window, then probes it once more (probe-recover). A single successful
//! response resets the counter and clears the cool-down.
//!
//! PRIVACY: like the rest of the crate, nothing here touches query text — only
//! engine names, failure classes, counts and timers.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;

/// Classification of an engine failure. Used both to surface an accurate
/// last-error class in `/stats` and metrics, and to decide whether a failure is
/// "hard" (worth counting toward an automatic cool-down).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// Bot-block / forbidden (HTTP 403, captcha challenge).
    BotBlock,
    /// Rate limited (HTTP 429). Handled primarily by [`crate::ratelimit`], so it
    /// is intentionally *not* counted as a hard failure here (avoids
    /// double-penalizing an engine the limiter is already backing off).
    RateLimit,
    /// Request or response-body read timed out.
    Timeout,
    /// Transport/connection failure (connect refused, reset, DNS, 5xx).
    Transport,
    /// Response arrived but failed to parse (bad JSON / unexpected shape).
    Parse,
    /// HTTP 200 but zero results on a general web scraper (selector drift /
    /// silent bot-block). Counts toward cool-down when
    /// [`crate::config::ServerSettings::empty_result_tracking`] is on.
    EmptyResults,
    /// Anything else (unknown engine, non-classified messages).
    Other,
}

impl FailureClass {
    /// Classify an engine error string into a [`FailureClass`]. The engine
    /// error vocabulary is small and stable (see `engines::*` and
    /// [`crate::engines::body_error`]); we match on the substrings those paths
    /// emit so the class is accurate without changing every engine signature.
    pub fn classify(reason: &str) -> FailureClass {
        let r = reason.to_ascii_lowercase();
        if r.contains("403") || r.contains("forbidden") || r.contains("captcha") {
            FailureClass::BotBlock
        } else if r.contains("429") || r.contains("too many requests") {
            FailureClass::RateLimit
        } else if r.contains("timeout") || r.contains("timed out") {
            FailureClass::Timeout
        } else if r.contains("request failed")
            || r.contains("connection")
            || r.contains("reset")
            || r.contains("502")
            || r.contains("503")
            || r.contains("504")
            || r.contains("dns")
        {
            FailureClass::Transport
        } else if r.contains("bad json")
            || r.contains("parse")
            || r.contains("decode")
            || r.contains("bad body")
            || r.contains("unexpected")
        {
            FailureClass::Parse
        } else if r.contains("empty results") {
            FailureClass::EmptyResults
        } else {
            FailureClass::Other
        }
    }

    /// Stable, lowercase label for diagnostics / `/stats`.
    pub fn label(&self) -> &'static str {
        match self {
            FailureClass::BotBlock => "bot-block",
            FailureClass::RateLimit => "rate-limit",
            FailureClass::Timeout => "timeout",
            FailureClass::Transport => "transport",
            FailureClass::Parse => "parse",
            FailureClass::EmptyResults => "empty-results",
            FailureClass::Other => "other",
        }
    }

    /// Whether this failure counts toward the consecutive-failure tally that
    /// trips an automatic cool-down. Bot-blocks, timeouts, and transport errors
    /// mean the engine is effectively down *for us*; rate limits are left to the
    /// [`crate::ratelimit`] backoff, parse failures are usually transient selector
    /// drift, and empty results are often just query-specific (especially for
    /// non-English queries) — none of these should trip the cooldown alone.
    pub fn counts_toward_cooldown(&self) -> bool {
        matches!(
            self,
            FailureClass::BotBlock
                | FailureClass::Timeout
                | FailureClass::Transport
        )
    }

    /// Back-compat alias used by older tests/diagnostics.
    pub fn is_hard(&self) -> bool {
        self.counts_toward_cooldown()
    }
}

/// Internal per-engine health record.
#[derive(Debug, Clone, Default)]
struct EngineHealth {
    /// Consecutive hard failures since the last success.
    consecutive_failures: u32,
    /// Most recent failure class (any class, hard or soft).
    last_class: Option<FailureClass>,
    /// When set and in the future, the engine is cooling down (skipped).
    cooling_until: Option<Instant>,
    /// How many times this engine has been cooled down (cumulative).
    cooldowns_total: u64,
}

impl EngineHealth {
    fn to_info(&self, now: Instant) -> HealthInfo {
        let cooling_down = self.cooling_until.map(|u| u > now).unwrap_or(false);
        let cooldown_remaining_secs = self
            .cooling_until
            .and_then(|u| u.checked_duration_since(now))
            .map(|d| d.as_secs())
            .unwrap_or(0);
        HealthInfo {
            healthy: !cooling_down && self.consecutive_failures == 0,
            cooling_down,
            consecutive_failures: self.consecutive_failures,
            last_error_class: self.last_class.map(|c| c.label().to_string()),
            cooldown_remaining_secs,
            cooldowns_total: self.cooldowns_total,
        }
    }
}

/// Public, serializable snapshot of one engine's health (for `/stats`, metrics
/// and the desktop app).
#[derive(Debug, Clone, Serialize)]
pub struct HealthInfo {
    /// No recent failures and not cooling down.
    pub healthy: bool,
    /// Currently inside a cool-down window (being skipped in fan-out).
    pub cooling_down: bool,
    /// Consecutive hard failures since the last success.
    pub consecutive_failures: u32,
    /// Label of the most recent failure class, if any.
    pub last_error_class: Option<String>,
    /// Approximate seconds left in the current cool-down (0 if not cooling).
    pub cooldown_remaining_secs: u64,
    /// Cumulative number of times this engine has been cooled down.
    pub cooldowns_total: u64,
}

impl Default for HealthInfo {
    fn default() -> Self {
        HealthInfo {
            healthy: true,
            cooling_down: false,
            consecutive_failures: 0,
            last_error_class: None,
            cooldown_remaining_secs: 0,
            cooldowns_total: 0,
        }
    }
}

/// Tracks health/cool-down state for every engine. Cheaply shared inside
/// [`crate::search::Runtime`] (interior mutability via a `Mutex`).
pub struct HealthTracker {
    /// Consecutive hard failures before an engine is cooled down. `0` disables
    /// the whole mechanism (no engine is ever skipped).
    threshold: u32,
    /// How long a cooled-down engine is skipped before it is probed again.
    cooldown: Duration,
    inner: Mutex<HashMap<String, EngineHealth>>,
}

impl HealthTracker {
    /// Build a tracker. `threshold == 0` disables cool-downs entirely.
    pub fn new(threshold: u32, cooldown_secs: u64) -> Self {
        HealthTracker {
            threshold,
            cooldown: Duration::from_secs(cooldown_secs.max(1)),
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Whether the health/cool-down mechanism is active.
    pub fn enabled(&self) -> bool {
        self.threshold > 0
    }

    /// Whether `engine` should be skipped right now (still inside its cool-down
    /// window). Returns `false` when disabled, unknown, or when the window has
    /// elapsed — so the very next call probes the engine (probe-recover).
    pub fn should_skip(&self, engine: &str) -> bool {
        self.should_skip_at(engine, Instant::now())
    }

    fn should_skip_at(&self, engine: &str, now: Instant) -> bool {
        if !self.enabled() {
            return false;
        }
        let map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        map.get(engine)
            .and_then(|h| h.cooling_until)
            .map(|until| until > now)
            .unwrap_or(false)
    }

    /// Record a successful response: resets the failure counter and clears any
    /// cool-down. Logs a recovery the first time an engine comes back.
    pub fn record_success(&self, engine: &str) {
        if !self.enabled() {
            return;
        }
        let was_cooling = {
            let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            let h = map.entry(engine.to_string()).or_default();
            let was_cooling = h.cooling_until.is_some();
            h.consecutive_failures = 0;
            h.last_class = None;
            h.cooling_until = None;
            was_cooling
        };
        if was_cooling {
            crate::obs::engine_recovered(engine);
        }
    }

    /// Record a failure for `engine`, classified by `class`. Returns `true` if
    /// this failure just (re-)armed a cool-down window.
    pub fn record_failure(&self, engine: &str, class: FailureClass) -> bool {
        self.record_failure_at(engine, class, Instant::now())
    }

    fn record_failure_at(&self, engine: &str, class: FailureClass, now: Instant) -> bool {
        if !self.enabled() {
            return false;
        }
        let armed_with: Option<u32> = {
            let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            let h = map.entry(engine.to_string()).or_default();
            h.last_class = Some(class);
            if !class.counts_toward_cooldown() {
                // Soft failures (rate-limit / parse / other) are recorded for
                // visibility but do not, on their own, trip the cool-down.
                None
            } else {
                h.consecutive_failures = h.consecutive_failures.saturating_add(1);
                // Re-arm on every hard failure once tripped, so a still-broken
                // engine that fails its probe is cooled down again immediately
                // instead of being hammered `threshold` more times.
                if h.consecutive_failures >= self.threshold {
                    h.cooling_until = Some(now + self.cooldown);
                    h.cooldowns_total = h.cooldowns_total.saturating_add(1);
                    Some(h.consecutive_failures)
                } else {
                    None
                }
            }
        };
        if let Some(consecutive) = armed_with {
            crate::obs::engine_cooldown(
                engine,
                consecutive,
                class.label(),
                self.cooldown.as_secs(),
            );
            true
        } else {
            false
        }
    }

    /// Health snapshot for a single engine (if it has any recorded history).
    pub fn info(&self, engine: &str) -> Option<HealthInfo> {
        let now = Instant::now();
        let map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        map.get(engine).map(|h| h.to_info(now))
    }

    /// Health snapshot for every engine with recorded history.
    pub fn snapshot(&self) -> HashMap<String, HealthInfo> {
        let now = Instant::now();
        let map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        map.iter()
            .map(|(k, h)| (k.clone(), h.to_info(now)))
            .collect()
    }

    /// A human-readable reason an engine is currently being skipped, for the
    /// `unresponsive_engines` list. `None` if it is not cooling down.
    pub fn cooldown_reason(&self, engine: &str) -> Option<String> {
        let info = self.info(engine)?;
        if !info.cooling_down {
            return None;
        }
        let cls = info.last_error_class.as_deref().unwrap_or("hard");
        Some(format!(
            "cooling down ~{}s after {} consecutive {} failures",
            info.cooldown_remaining_secs, info.consecutive_failures, cls
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_maps_real_engine_reasons() {
        // Strings these come from the actual engine error paths.
        assert_eq!(
            FailureClass::classify("HTTP 403 Forbidden"),
            FailureClass::BotBlock
        );
        assert_eq!(FailureClass::classify("HTTP 429"), FailureClass::RateLimit);
        assert_eq!(FailureClass::classify("timeout"), FailureClass::Timeout);
        assert_eq!(
            FailureClass::classify("timeout reading response body: ..."),
            FailureClass::Timeout
        );
        assert_eq!(
            FailureClass::classify("request failed: connection reset"),
            FailureClass::Transport
        );
        assert_eq!(FailureClass::classify("HTTP 503"), FailureClass::Transport);
        assert_eq!(
            FailureClass::classify("bad json: expected value"),
            FailureClass::Parse
        );
        assert_eq!(FailureClass::classify("no results"), FailureClass::Other);
    }

    #[test]
    fn hard_vs_soft_classes() {
        assert!(FailureClass::BotBlock.is_hard());
        assert!(FailureClass::Timeout.is_hard());
        assert!(FailureClass::Transport.is_hard());
        assert!(!FailureClass::RateLimit.is_hard());
        assert!(!FailureClass::Parse.is_hard());
        assert!(!FailureClass::EmptyResults.is_hard()); // Empty results don't trip cooldown
        assert!(!FailureClass::Other.is_hard());
    }

    #[test]
    fn threshold_zero_disables_everything() {
        let h = HealthTracker::new(0, 60);
        assert!(!h.enabled());
        for _ in 0..100 {
            assert!(!h.record_failure("mojeek", FailureClass::BotBlock));
        }
        assert!(!h.should_skip("mojeek"));
        assert!(h.info("mojeek").is_none());
    }

    #[test]
    fn cools_down_after_threshold_hard_failures() {
        let h = HealthTracker::new(3, 60);
        let now = Instant::now();
        // Two failures: not yet cooled.
        assert!(!h.record_failure_at("mojeek", FailureClass::BotBlock, now));
        assert!(!h.record_failure_at("mojeek", FailureClass::BotBlock, now));
        assert!(!h.should_skip_at("mojeek", now));
        // Third trips the cool-down.
        assert!(h.record_failure_at("mojeek", FailureClass::BotBlock, now));
        assert!(h.should_skip_at("mojeek", now));

        let info = h.info("mojeek").unwrap();
        assert!(info.cooling_down);
        assert!(!info.healthy);
        assert_eq!(info.consecutive_failures, 3);
        assert_eq!(info.last_error_class.as_deref(), Some("bot-block"));
        assert_eq!(info.cooldowns_total, 1);
    }

    #[test]
    fn soft_failures_do_not_trip_cooldown() {
        let h = HealthTracker::new(2, 60);
        let now = Instant::now();
        for _ in 0..10 {
            assert!(!h.record_failure_at("semanticscholar", FailureClass::RateLimit, now));
            assert!(!h.record_failure_at("codeberg", FailureClass::Parse, now));
        }
        assert!(!h.should_skip_at("semanticscholar", now));
        assert!(!h.should_skip_at("codeberg", now));
        // But the last error class is still recorded for visibility.
        assert_eq!(
            h.info("semanticscholar")
                .unwrap()
                .last_error_class
                .as_deref(),
            Some("rate-limit")
        );
    }

    #[test]
    fn probe_recovers_after_window_then_success_resets() {
        let h = HealthTracker::new(2, 60);
        let t0 = Instant::now();
        assert!(!h.record_failure_at("arxiv", FailureClass::Timeout, t0));
        assert!(h.record_failure_at("arxiv", FailureClass::Timeout, t0));
        assert!(h.should_skip_at("arxiv", t0));

        // Still cooling halfway through the window.
        let mid = t0 + Duration::from_secs(30);
        assert!(h.should_skip_at("arxiv", mid));

        // After the window elapses, the engine is probed (not skipped).
        let after = t0 + Duration::from_secs(61);
        assert!(!h.should_skip_at("arxiv", after));

        // A successful probe resets everything.
        h.record_success("arxiv");
        let info = h.info("arxiv").unwrap();
        assert!(info.healthy);
        assert!(!info.cooling_down);
        assert_eq!(info.consecutive_failures, 0);
        assert!(info.last_error_class.is_none());
    }

    #[test]
    fn failed_probe_rearms_cooldown_immediately() {
        let h = HealthTracker::new(2, 60);
        let t0 = Instant::now();
        h.record_failure_at("mojeek", FailureClass::BotBlock, t0);
        assert!(h.record_failure_at("mojeek", FailureClass::BotBlock, t0));
        assert_eq!(h.info("mojeek").unwrap().cooldowns_total, 1);

        // Window elapses; probe fails again → re-armed immediately (not after
        // another full `threshold` failures).
        let after = t0 + Duration::from_secs(61);
        assert!(!h.should_skip_at("mojeek", after));
        assert!(h.record_failure_at("mojeek", FailureClass::BotBlock, after));
        assert!(h.should_skip_at("mojeek", after));
        assert_eq!(h.info("mojeek").unwrap().cooldowns_total, 2);
    }
}
