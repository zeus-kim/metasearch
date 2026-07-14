//! Network-gated selector-drift harness.
//!
//! HTML-scraping engines (Brave, Mojeek, Startpage, DuckDuckGo Lite, …) break
//! silently when an upstream changes its markup. These tests hit the real
//! upstreams and assert that each engine still yields results — catching
//! selector drift that fixture tests cannot.
//!
//! They are SKIPPED OFFLINE in two ways so the default `cargo test` stays green
//! with no network:
//!   1. every test is `#[ignore]`d (only runs with `--ignored`), and
//!   2. each test early-returns unless `METASEARCH_LIVE=1` is set.
//!
//! Run them deliberately with:
//!   METASEARCH_LIVE=1 cargo test --test live_drift -- --ignored --nocapture

use std::time::Duration;

use metasearch::config::Settings;
use metasearch::search::{search_all, Runtime, SearchParams};

fn live_enabled() -> bool {
    std::env::var("METASEARCH_LIVE")
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// Run a single-engine search (via a `!bang`) and return the result count.
async fn count_for(bang_query: &str) -> usize {
    let mut settings = Settings::default();
    // Be patient with live upstreams.
    settings.server.request_timeout_secs = 15;
    settings.server.engine_min_interval_ms = 0;
    let rt = Runtime::new(&settings);
    let params = SearchParams::new(bang_query);
    let resp = search_all(&params, &settings, &rt).await;
    resp.results.len()
}

macro_rules! drift_test {
    ($name:ident, $engine:literal, $query:literal) => {
        #[tokio::test]
        #[ignore = "network-gated; run with METASEARCH_LIVE=1 --ignored"]
        async fn $name() {
            if !live_enabled() {
                eprintln!("skipping {}: set METASEARCH_LIVE=1 to run", $engine);
                return;
            }
            let n = tokio::time::timeout(Duration::from_secs(30), count_for($query))
                .await
                .unwrap_or(0);
            assert!(
                n > 0,
                "engine `{}` returned 0 results — possible selector drift / block",
                $engine
            );
            eprintln!("OK {}: {} results", $engine, n);
        }
    };
}

drift_test!(brave_live, "brave", "!br rust programming language");
drift_test!(mojeek_live, "mojeek", "!mjk rust programming language");
// Qwant is keyless but opt-in (disabled by default); enable it in config before
// running this live. The key-based major engines (brave_api, yandex, bing,
// bingnews, google) are intentionally omitted here — they need API keys and are
// covered by fixture tests instead.
drift_test!(qwant_live, "qwant", "!qw rust programming language");
drift_test!(
    startpage_live,
    "startpage",
    "!startpage rust programming language"
);
drift_test!(
    ddg_lite_live,
    "duckduckgo_lite",
    "!ddl rust programming language"
);
drift_test!(wikipedia_live, "wikipedia", "!w albert einstein");
drift_test!(duckduckgo_live, "duckduckgo", "!ddg rust");
