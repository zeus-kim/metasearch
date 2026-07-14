//! Structured JSON logging for Orgos search engine.
//!
//! A lightweight, dependency-free JSON logger that writes structured records to
//! stderr. Uses the `LOG_LEVEL` environment variable (default: info).
//!
//! Log format: `{"ts":"2024-01-01T12:00:00Z","level":"info","msg":"...","fields":{...}}`

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

// Log levels as u8 for atomic operations
const ERROR: u8 = 1;
const WARN: u8 = 2;
const INFO: u8 = 3;
const DEBUG: u8 = 4;

static LEVEL: AtomicU8 = AtomicU8::new(INFO);

/// Initialize the log level from `LOG_LEVEL` environment variable.
/// Valid values: error, warn, info, debug (default: info)
pub fn init() {
    let level = match std::env::var("LOG_LEVEL")
        .or_else(|_| std::env::var("METASEARCH_LOG"))
        .as_deref()
    {
        Ok("error") => ERROR,
        Ok("warn") | Ok("warning") => WARN,
        Ok("debug") | Ok("trace") => DEBUG,
        _ => INFO,
    };
    LEVEL.store(level, Ordering::Relaxed);
}

fn enabled(level: u8) -> bool {
    LEVEL.load(Ordering::Relaxed) >= level
}

/// Generate ISO 8601 timestamp (UTC).
fn timestamp() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();

    // Calculate date/time components from Unix timestamp
    let days = secs / 86400;
    let time_secs = secs % 86400;
    let hours = time_secs / 3600;
    let minutes = (time_secs % 3600) / 60;
    let seconds = time_secs % 60;

    // Calculate year, month, day from days since epoch (1970-01-01)
    let (year, month, day) = days_to_ymd(days as i64);

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, day, hours, minutes, seconds
    )
}

/// Convert days since Unix epoch to (year, month, day).
fn days_to_ymd(days: i64) -> (i32, u32, u32) {
    // Algorithm from http://howardhinnant.github.io/date_algorithms.html
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

/// Escape a string for JSON output.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

/// A structured log entry with arbitrary fields.
pub struct LogEntry {
    level: &'static str,
    level_num: u8,
    msg: String,
    fields: HashMap<String, String>,
}

impl LogEntry {
    fn new(level: &'static str, level_num: u8, msg: impl Into<String>) -> Self {
        LogEntry {
            level,
            level_num,
            msg: msg.into(),
            fields: HashMap::new(),
        }
    }

    /// Add a string field to the log entry.
    pub fn field(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.fields.insert(key.into(), value.into());
        self
    }

    /// Add a numeric field to the log entry.
    pub fn field_num(mut self, key: impl Into<String>, value: impl std::fmt::Display) -> Self {
        self.fields.insert(key.into(), format!("__NUM__{}", value));
        self
    }

    /// Emit the log entry to stderr.
    pub fn emit(self) {
        if !enabled(self.level_num) {
            return;
        }

        let mut fields_json = String::new();
        let mut first = true;
        for (k, v) in &self.fields {
            if !first {
                fields_json.push(',');
            }
            first = false;
            // Check if it's a numeric value (marked with __NUM__ prefix)
            if let Some(num) = v.strip_prefix("__NUM__") {
                fields_json.push_str(&format!("\"{}\":{}", json_escape(k), num));
            } else {
                fields_json.push_str(&format!("\"{}\":\"{}\"", json_escape(k), json_escape(v)));
            }
        }

        let json = format!(
            r#"{{"ts":"{}","level":"{}","msg":"{}","fields":{{{}}}}}"#,
            timestamp(),
            self.level,
            json_escape(&self.msg),
            fields_json
        );
        eprintln!("{}", json);
    }
}

// ----------------------------------------------------------------- Log macros

/// Create a debug log entry.
pub fn debug(msg: impl Into<String>) -> LogEntry {
    LogEntry::new("debug", DEBUG, msg)
}

/// Create an info log entry.
pub fn info(msg: impl Into<String>) -> LogEntry {
    LogEntry::new("info", INFO, msg)
}

/// Create a warn log entry.
pub fn warn(msg: impl Into<String>) -> LogEntry {
    LogEntry::new("warn", WARN, msg)
}

/// Create an error log entry.
pub fn error(msg: impl Into<String>) -> LogEntry {
    LogEntry::new("error", ERROR, msg)
}

// ----------------------------------------------------------------- Convenience functions

/// Log server start event.
pub fn server_start(port: u16, version: &str, bind_address: &str) {
    info("server started")
        .field("port", port.to_string())
        .field("version", version)
        .field("bind_address", bind_address)
        .emit();
}

/// Log server shutdown event.
pub fn server_shutdown() {
    info("server shutdown").emit();
}

/// Log incoming request.
pub fn request_received(method: &str, path: &str, ip: Option<&str>) {
    let mut entry = debug("request received")
        .field("method", method)
        .field("path", path);
    if let Some(ip) = ip {
        entry = entry.field("ip", ip);
    }
    entry.emit();
}

/// Log completed request.
pub fn request_completed(method: &str, path: &str, status: u16, duration_ms: u128) {
    info("request completed")
        .field("method", method)
        .field("path", path)
        .field_num("status", status)
        .field_num("duration_ms", duration_ms)
        .emit();
}

/// Log search query.
pub fn search_query(query: &str, engines: &[&str], results_count: usize, duration_ms: u128) {
    info("search query")
        .field("query", query)
        .field("engines", engines.join(","))
        .field_num("results_count", results_count)
        .field_num("duration_ms", duration_ms)
        .emit();
}

/// Log an error with context.
pub fn error_occurred(message: &str, context: &str) {
    error("error occurred")
        .field("error", message)
        .field("context", context)
        .emit();
}

/// Log rate limit event.
pub fn rate_limited(ip: &str, endpoint: &str) {
    warn("rate limited")
        .field("ip", ip)
        .field("endpoint", endpoint)
        .emit();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_json_escape() {
        assert_eq!(json_escape("hello"), "hello");
        assert_eq!(json_escape("hello\"world"), "hello\\\"world");
        assert_eq!(json_escape("line1\nline2"), "line1\\nline2");
    }

    #[test]
    fn test_days_to_ymd() {
        // 1970-01-01 is day 0
        assert_eq!(days_to_ymd(0), (1970, 1, 1));
        // 2024-01-01 is 19723 days from epoch (leap years accounted for)
        assert_eq!(days_to_ymd(19723), (2024, 1, 1));
    }
}
