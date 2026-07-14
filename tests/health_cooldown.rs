//! Engine health / auto-fallback integration test.
//!
//! Drives the *real* `search_all` pipeline against a local fixture engine that
//! returns HTTP 403 (a bot-block — a hard failure class), and proves that after
//! the configured threshold of consecutive hard failures the engine is cooled
//! down (skipped in fan-out, surfaced in `unresponsive_engines`), then probes
//! and recovers once the upstream starts succeeding again.
//!
//! Fully offline: the only socket opened is a loopback fixture server in this
//! test; all native engines are disabled so nothing touches the network.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use metasearch::config::{CustomEngine, Settings};
use metasearch::search::{search_all, Runtime, SearchParams};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Fixture upstream modes.
const MODE_FORBIDDEN: u8 = 0; // always 403
const MODE_OK: u8 = 1; // 200 + a valid JSON result

/// Boot a tiny loopback HTTP server whose response is controlled by `mode`.
/// Returns the bound port.
async fn spawn_fixture(mode: Arc<AtomicU8>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let mode = mode.clone();
            tokio::spawn(async move {
                // Drain the request headers (enough to be a well-formed server).
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf).await;
                let resp = match mode.load(Ordering::Relaxed) {
                    MODE_OK => {
                        let body = r#"{"items":[{"url":"http://example.com/1","title":"Fixture One","content":"hello world"}]}"#;
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        )
                    }
                    _ => {
                        let body = "forbidden";
                        format!(
                            "HTTP/1.1 403 Forbidden\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        )
                    }
                };
                let _ = stream.write_all(resp.as_bytes()).await;
                let _ = stream.flush().await;
            });
        }
    });
    port
}

/// Build settings with *only* a single config-driven JSON engine pointed at the
/// fixture, a low cool-down threshold and a short window, caching disabled.
fn fixture_settings(port: u16, threshold: u32, cooldown_secs: u64) -> Settings {
    let mut s = Settings::default();
    // Disable every native engine so the fan-out only runs the fixture.
    for e in s.engines.iter_mut() {
        e.enabled = false;
    }
    // No caching (otherwise repeated identical queries never re-hit the engine).
    s.server.cache_ttl_secs = 0;
    // Single attempt per call keeps the test fast and the failure count crisp.
    s.server.max_retries = 0;
    s.server.engine_min_interval_ms = 0;
    s.server.engine_failure_threshold = threshold;
    s.server.engine_cooldown_secs = cooldown_secs;
    s.server.allow_private_urls = true;

    s.custom_engines = vec![CustomEngine {
        name: "fixture".into(),
        kind: "json".into(),
        enabled: true,
        weight: 1.0,
        categories: vec!["general".into()],
        url_template: Some(format!("http://127.0.0.1:{port}/?q={{query}}")),
        description_url: None,
        result_path: Some("items".into()),
        url_field: Some("url".into()),
        title_field: Some("title".into()),
        content_field: Some("content".into()),
        thumbnail_field: None,
        published_field: None,
        timeout_secs: Some(2),
        api_key: None,
    }];
    s
}

fn is_cooling(resp: &metasearch::SearchResponse) -> bool {
    resp.unresponsive_engines
        .iter()
        .any(|(name, reason)| name == "fixture" && reason.contains("cooling down"))
}

#[tokio::test]
async fn engine_cools_down_after_repeated_403_then_recovers() {
    let mode = Arc::new(AtomicU8::new(MODE_FORBIDDEN));
    let port = spawn_fixture(mode.clone()).await;

    let threshold = 3;
    let cooldown_secs = 1;
    let settings = fixture_settings(port, threshold, cooldown_secs);
    let rt = Runtime::new(&settings);

    // Each search hits the fixture once and gets a 403 (a hard, bot-block
    // failure). The first `threshold` searches actually call the engine.
    for i in 0..threshold {
        let resp = search_all(&SearchParams::new("rust language"), &settings, &rt).await;
        // While still being attempted, the engine shows up as unresponsive with
        // an HTTP 403 reason (not yet "cooling down").
        assert!(
            resp.unresponsive_engines
                .iter()
                .any(|(n, r)| n == "fixture" && r.contains("403")),
            "iter {i}: expected a 403 from the fixture engine, got {:?}",
            resp.unresponsive_engines
        );
        assert!(!is_cooling(&resp), "iter {i}: should not be cooling yet");
        assert!(resp.results.is_empty());
    }

    // The threshold has now been crossed → the engine is cooled down and is
    // skipped in fan-out, surfaced with a clear "cooling down" reason.
    let info = rt.health.info("fixture").expect("health recorded");
    assert!(info.cooling_down, "engine should be cooling down: {info:?}");
    assert_eq!(info.last_error_class.as_deref(), Some("bot-block"));
    assert_eq!(info.cooldowns_total, 1);

    let resp = search_all(&SearchParams::new("rust language"), &settings, &rt).await;
    assert!(
        is_cooling(&resp),
        "skipped engine should be reported as cooling down: {:?}",
        resp.unresponsive_engines
    );

    // Flip the upstream healthy and wait out the cool-down window so the next
    // search probes the engine again (probe-recover).
    mode.store(MODE_OK, Ordering::Relaxed);
    tokio::time::sleep(Duration::from_millis(cooldown_secs * 1000 + 300)).await;

    let resp = search_all(&SearchParams::new("rust language"), &settings, &rt).await;
    assert!(
        !is_cooling(&resp),
        "engine should have been probed after the window: {:?}",
        resp.unresponsive_engines
    );
    assert!(
        resp.results.iter().any(|r| r.url.contains("example.com")),
        "recovered engine should contribute results again: {} results",
        resp.results.len()
    );

    // And the tracker reflects a full recovery.
    let info = rt.health.info("fixture").expect("health recorded");
    assert!(
        info.healthy,
        "engine should be healthy after a success: {info:?}"
    );
    assert_eq!(info.consecutive_failures, 0);
    assert!(!info.cooling_down);
}

#[tokio::test]
async fn timeout_failures_also_trip_cooldown() {
    // A fixture that accepts the connection but never replies → the per-engine
    // timeout fires, which is a hard (timeout) failure.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            // Hold the connection open without responding.
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(30)).await;
                drop(stream);
            });
        }
    });

    let threshold = 2;
    let settings = fixture_settings(port, threshold, 5);
    let rt = Runtime::new(&settings);

    for _ in 0..threshold {
        let resp = search_all(&SearchParams::new("anything"), &settings, &rt).await;
        assert!(resp
            .unresponsive_engines
            .iter()
            .any(|(n, _)| n == "fixture"));
    }

    let info = rt.health.info("fixture").expect("health recorded");
    assert!(
        info.cooling_down,
        "timeouts should trip the cool-down: {info:?}"
    );
    assert_eq!(info.last_error_class.as_deref(), Some("timeout"));
}
