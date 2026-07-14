//! Standalone metasearch HTTP server.
//!
//! Config resolution order:
//!   1. `$METASEARCH_SETTINGS` if set
//!   2. `./settings.yml` next to the working directory
//!   3. built-in defaults (no file required)
//!
//! Environment overlays (see `Settings::apply_env`):
//!   * `UPSTREAM_SEARCH_URL`    — enable + point an upstream search engine
//!   * `METASEARCH_AI_BASE_URL` — Ollama-compatible base URL for AI features
//!   * `METASEARCH_BIND`        — bind address (e.g. 0.0.0.0 in a container)
//!   * `METASEARCH_PORT`        — listen port
//!   * `GOOGLE_API_KEY` + `GOOGLE_CSE_ID`, `BING_API_KEY` — key-based engines
//!   * `METASEARCH_PROXIES`     — comma/space-separated proxy URLs (rotation)
//!   * `METASEARCH_LOG`         — error|warn|info|debug
//!
//! Flags:
//!   * `--healthcheck` — probe the configured `/healthz` and exit 0/1 (used by
//!     the Docker HEALTHCHECK; needs no extra tools in the image).
//!   * `--open` — on macOS, open `http://127.0.0.1:<port>/` after bind (also
//!     `METASEARCH_OPEN=1`). Prefer `./scripts/run.sh` for build + wait + open.

use std::path::PathBuf;

use metasearch::Settings;

#[tokio::main]
async fn main() {
    // Only open browser if explicitly requested with --open flag
    // METASEARCH_OPEN=0 to force disable
    let open_browser = std::env::var("METASEARCH_OPEN").ok().as_deref() != Some("0")
        && std::env::args().any(|a| a == "--open");

    eprintln!("Starting metasearch (first compile can take 1–2 min)…");

    let path = std::env::var("METASEARCH_SETTINGS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("settings.yml"));

    let settings = match Settings::load(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("config error: {e}");
            std::process::exit(1);
        }
    };

    if std::env::args().any(|a| a == "--healthcheck") {
        std::process::exit(healthcheck(&settings).await);
    }

    if let Err(e) = metasearch::serve(settings, open_browser).await {
        eprintln!("server error: {e}");
        std::process::exit(1);
    }
}

/// Probe `/healthz` on the configured bind address/port. Returns a process exit
/// code (0 = healthy, 1 = unhealthy).
async fn healthcheck(settings: &Settings) -> i32 {
    let host = match settings.server.bind_address.as_str() {
        "0.0.0.0" | "" => "127.0.0.1",
        h => h,
    };
    let url = format!("http://{host}:{}/healthz", settings.server.port);
    let client = metasearch::build_client();
    match client
        .get(&url)
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => 0,
        _ => 1,
    }
}
