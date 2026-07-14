//! Basic load / smoke test for the standalone HTTP server.
//!
//! Boots the server on a loopback port and fires a burst of concurrent
//! requests at local, no-network endpoints (`/healthz`, `/config`). Verifies
//! the connection back-pressure and routing hold up under concurrency. Fully
//! offline — never touches an upstream engine.

use std::time::{Duration, Instant};

use metasearch::config::Settings;

/// Find a free loopback port by binding to :0 and releasing it.
fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

#[tokio::test]
async fn server_handles_concurrent_load() {
    let port = free_port();
    let mut settings = Settings::default();
    settings.server.bind_address = "127.0.0.1".into();
    settings.server.port = port;
    settings.server.max_connections = 32;

    // Boot the server in the background; the test runtime aborts it on exit.
    tokio::spawn(async move {
        let _ = metasearch::serve(settings, false).await;
    });

    let client = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{port}");

    // Wait for readiness (up to ~3s).
    let mut ready = false;
    for _ in 0..30 {
        if let Ok(r) = client.get(format!("{base}/healthz")).send().await {
            if r.status().is_success() {
                ready = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(ready, "server did not become ready on port {port}");

    // Fire N concurrent requests across two no-network endpoints.
    const N: usize = 200;
    let started = Instant::now();
    let mut handles = Vec::with_capacity(N);
    for i in 0..N {
        let client = client.clone();
        let base = base.clone();
        handles.push(tokio::spawn(async move {
            let path = if i % 2 == 0 { "/healthz" } else { "/config" };
            client
                .get(format!("{base}{path}"))
                .timeout(Duration::from_secs(5))
                .send()
                .await
                .map(|r| r.status().is_success())
                .unwrap_or(false)
        }));
    }

    let mut ok = 0usize;
    for h in handles {
        if h.await.unwrap_or(false) {
            ok += 1;
        }
    }
    let elapsed = started.elapsed();
    eprintln!(
        "load: {ok}/{N} succeeded in {:?} ({:.0} req/s)",
        elapsed,
        N as f64 / elapsed.as_secs_f64()
    );
    // Allow a tiny slack for scheduling jitter but expect near-100% success.
    assert!(ok >= N - 2, "only {ok}/{N} requests succeeded under load");
}
