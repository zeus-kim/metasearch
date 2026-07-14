//! Shared retry / backoff helpers for bot-block resilience.
//!
//! Engine fetches occasionally fail transiently (rate limits, blocks, network
//! blips). The orchestrator retries a failed engine a configurable number of
//! times with exponential backoff, optionally rotating through configured
//! proxies between attempts (see [`crate::search`]). These pure helpers keep
//! the backoff schedule testable.

use std::time::Duration;

/// Exponential backoff delay for retry `attempt` (1-based), capped at 2s with a
/// small fixed base so retries stay polite but quick.
pub fn backoff(attempt: u32) -> Duration {
    let base_ms: u64 = 150;
    let factor = 1u64 << attempt.min(4); // 2,4,8,16,16,...
    Duration::from_millis((base_ms * factor).min(2000))
}

/// Whether an engine error string looks like a transient bot-block / rate
/// limit / network condition worth retrying (vs. a permanent "no results").
pub fn is_retryable(reason: &str) -> bool {
    let r = reason.to_ascii_lowercase();
    r.contains("429")
        || r.contains("403")
        || r.contains("503")
        || r.contains("timeout")
        || r.contains("request failed")
        || r.contains("connection")
        || r.contains("reset")
        || r.contains("captcha")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        assert!(backoff(1) < backoff(2));
        assert!(backoff(2) < backoff(3));
        assert_eq!(backoff(10), Duration::from_millis(2000));
    }

    #[test]
    fn classifies_transient_errors() {
        assert!(is_retryable("HTTP 429"));
        assert!(is_retryable("request failed: connection reset"));
        assert!(is_retryable("timeout"));
        assert!(!is_retryable("bad json: x"));
        assert!(!is_retryable("HTTP 404"));
    }
}
