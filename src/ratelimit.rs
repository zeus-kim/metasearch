//! Per-engine politeness / rate limiting with adaptive 429 backoff.
//!
//! Each engine has a minimum interval between requests. When an engine replies
//! `429 Too Many Requests`, its next-allowed time is pushed out exponentially.
//! This keeps us a well-behaved client of the free upstreams we depend on.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

struct State {
    next_allowed: Instant,
    /// Current backoff penalty applied after consecutive 429s.
    penalty: Duration,
}

/// Hard ceiling on how far into the future the limiter will ever push a slot's
/// `next_allowed`, and therefore the longest a single [`RateLimiter::acquire`]
/// can block. This is the key guard against the consecutive-search hang: under
/// concurrent fan-out, every `acquire` reserves the next slot, so rapidly
/// overlapping searches on a *shared* key (e.g. the `"wikimedia"` slot) used to
/// pile reservations arbitrarily far into the future — and a single `429`
/// penalty could push it 120s out. Because the per-engine timeout did not cover
/// the `acquire` wait, that stalled the whole `join_all`. Capping the horizon
/// keeps the backlog bounded and self-healing: a backed-up or penalized slot
/// simply yields shortly; persistently-failing engines are handled separately
/// by the health tracker (cool-down), not by ever-growing waits.
const MAX_WAIT: Duration = Duration::from_secs(3);

pub struct RateLimiter {
    min_interval: Duration,
    map: Mutex<HashMap<String, State>>,
}

impl RateLimiter {
    pub fn new(min_interval_ms: u64) -> Self {
        RateLimiter {
            min_interval: Duration::from_millis(min_interval_ms),
            map: Mutex::new(HashMap::new()),
        }
    }

    fn enabled(&self) -> bool {
        !self.min_interval.is_zero()
    }

    /// Block until this engine is allowed to make a request, then reserve the
    /// next slot. No-op when rate limiting is disabled.
    ///
    /// The returned wait is hard-capped at [`MAX_WAIT`] and the reserved
    /// `next_allowed` is clamped to `now + MAX_WAIT`, so a shared/penalized slot
    /// can never accumulate an unbounded backlog or block the fan-out for long.
    pub async fn acquire(&self, engine: &str) {
        if !self.enabled() {
            return;
        }
        let wait = {
            let mut map = match self.map.lock() {
                Ok(m) => m,
                Err(_) => return,
            };
            let now = Instant::now();
            let horizon = now + MAX_WAIT;
            let entry = map.entry(engine.to_string()).or_insert(State {
                next_allowed: now,
                penalty: Duration::ZERO,
            });
            // How long the caller actually waits, capped so a far-future
            // `next_allowed` (from backlog or a 429 penalty) can't stall us.
            let wait = entry
                .next_allowed
                .saturating_duration_since(now)
                .min(MAX_WAIT);
            // Reserve the next slot from whichever is later (but never beyond the
            // horizon), so reservations self-heal instead of growing unbounded
            // across rapid consecutive searches.
            let base = entry.next_allowed.max(now).min(horizon);
            entry.next_allowed = base + self.min_interval;
            wait
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }

    /// Record a 429 from an engine, applying exponential backoff (capped).
    pub fn penalize(&self, engine: &str) {
        if let Ok(mut map) = self.map.lock() {
            let now = Instant::now();
            let entry = map.entry(engine.to_string()).or_insert(State {
                next_allowed: now,
                penalty: Duration::ZERO,
            });
            let next = if entry.penalty.is_zero() {
                Duration::from_secs(2)
            } else {
                (entry.penalty * 2).min(Duration::from_secs(120))
            };
            entry.penalty = next;
            entry.next_allowed = now + next;
        }
    }

    /// Clear the penalty after a successful request.
    pub fn reward(&self, engine: &str) {
        if let Ok(mut map) = self.map.lock() {
            if let Some(entry) = map.get_mut(engine) {
                entry.penalty = Duration::ZERO;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A penalized slot (which sets `next_allowed` up to 120s out) must never
    /// make `acquire` block longer than `MAX_WAIT` — this is the guard against
    /// the consecutive-search hang.
    #[tokio::test(start_paused = true)]
    async fn penalized_acquire_is_capped() {
        let rl = RateLimiter::new(200);
        // Simulate the worst case: many consecutive 429s push the penalty to its
        // cap, putting `next_allowed` ~120s into the future.
        for _ in 0..10 {
            rl.penalize("wikimedia");
        }
        let start = Instant::now();
        rl.acquire("wikimedia").await;
        assert!(
            start.elapsed() <= MAX_WAIT + Duration::from_millis(50),
            "acquire waited {:?}, expected <= {:?}",
            start.elapsed(),
            MAX_WAIT
        );
    }

    /// Rapid reservations on a shared slot must not accumulate unboundedly:
    /// every individual `acquire` stays within `MAX_WAIT`.
    #[tokio::test(start_paused = true)]
    async fn reservations_do_not_accumulate_past_horizon() {
        let rl = RateLimiter::new(1000);
        for _ in 0..50 {
            let start = Instant::now();
            rl.acquire("wikimedia").await;
            assert!(start.elapsed() <= MAX_WAIT + Duration::from_millis(50));
        }
    }
}
