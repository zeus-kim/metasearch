//! Lightweight observability.
//!
//! A dependency-free, leveled logger writing structured single-line records to
//! stderr. We deliberately avoid pulling in `tracing` + `tracing-subscriber`
//! (not vendored in this environment); the API here mirrors the parts we need.
//!
//! PRIVACY: this module never accepts or emits the user's query text. Callers
//! log engine names, counts, durations and statuses only.

use std::sync::atomic::{AtomicU8, Ordering};

const ERROR: u8 = 1;
const WARN: u8 = 2;
const INFO: u8 = 3;
const DEBUG: u8 = 4;

static LEVEL: AtomicU8 = AtomicU8::new(INFO);

/// Initialise the log level from `METASEARCH_LOG` (error|warn|info|debug).
pub fn init() {
    let level = match std::env::var("METASEARCH_LOG").as_deref() {
        Ok("error") => ERROR,
        Ok("warn") => WARN,
        Ok("debug") | Ok("trace") => DEBUG,
        _ => INFO,
    };
    LEVEL.store(level, Ordering::Relaxed);
}

fn enabled(level: u8) -> bool {
    LEVEL.load(Ordering::Relaxed) >= level
}

fn emit(tag: &str, msg: &str) {
    eprintln!("[metasearch] {tag} {msg}");
}

pub fn error(msg: impl AsRef<str>) {
    if enabled(ERROR) {
        emit("ERROR", msg.as_ref());
    }
}

pub fn warn(msg: impl AsRef<str>) {
    if enabled(WARN) {
        emit("WARN ", msg.as_ref());
    }
}

pub fn info(msg: impl AsRef<str>) {
    if enabled(INFO) {
        emit("INFO ", msg.as_ref());
    }
}

pub fn debug(msg: impl AsRef<str>) {
    if enabled(DEBUG) {
        emit("DEBUG", msg.as_ref());
    }
}

/// Log the outcome of one engine call (never includes query text).
pub fn engine_result(engine: &str, status: &str, count: usize, ms: u128) {
    debug(format!(
        "engine={engine} status={status} results={count} ms={ms}"
    ));
}

/// Log that an engine has been automatically cooled down (skipped in fan-out)
/// after repeated hard failures. Never includes query text.
pub fn engine_cooldown(engine: &str, consecutive: u32, class: &str, secs: u64) {
    warn(format!(
        "engine={engine} cooled-down consecutive={consecutive} class={class} for={secs}s"
    ));
}

/// Log that a previously cooled-down engine has recovered. Never includes query text.
pub fn engine_recovered(engine: &str) {
    info(format!("engine={engine} recovered from cool-down"));
}
