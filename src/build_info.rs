//! Compile-time build identity for stack fingerprinting (UI footer, HTTP headers).

pub const STACK: &str = "metasearch";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const GIT_SHA: &str = env!("METASEARCH_GIT_SHA");

pub fn version_label() -> String {
    format!("{VERSION}+{GIT_SHA}")
}
