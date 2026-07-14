//! A small, dependency-light HTTP/1.1 server exposing the metasearch API.
//!
//! This module is split into submodules for maintainability:
//! - `response` - HTTP response types and builders
//! - `auth` - Admin authentication
//!
//! Routes:
//! * `GET /`              — Answer UI (SSE streaming + sources)
//! * `GET /classic`       — classic standard HTML search UI
//! * `GET /search`        — `?q=&format=json|html|rss|csv&categories=&pageno=&language=&time_range=&safesearch=&ai=`
//! * `GET /autocompleter` — `?q=` → OpenSearch-style `[query, [suggestions]]`
//! * `GET /config`        — enabled engines, categories, formats, AI flags (JSON)
//! * `GET /stats`         — cumulative per-engine timing/error stats (JSON)
//! * `GET /preferences`   — HTML preferences page (client-side persisted)
//! * `GET /opensearch.xml`— OpenSearch description (add as a browser search engine)
//! * `GET /image_proxy`   — `?url=` privacy-proxied image thumbnails
//! * `GET /llms.txt`      — AI-readable project handoff guide
//! * `GET /.well-known/ai-handoff.json` — structured agent handoff metadata
//! * `GET /api/v1/discover_snapshot` — cached daily Discover snapshot
//! * `GET /api/v1/trending` — real-time Google Trends (geo=KR|US|JP|DE|FR|CN|BR etc)
//! * `GET|POST /api/v1/news_images` — cached async image hydration for news cards
//! * `GET /healthz`       — liveness probe
//! * `GET /health`        — comprehensive health check (status, version, uptime, engines, cache, memory)
//!
//! Only GET/HEAD are handled and connections close after each response. Anything
//! heavier (keep-alive, TLS) belongs behind a reverse proxy, standard itself.

pub mod auth;
pub mod response;

use auth::{check_admin_auth, handle_login, ADMIN_SESSION_COOKIE};
use response::{Body, Response, SECURITY_HEADERS, escape};

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

use crate::config::Settings;
use crate::search::{autocomplete, search_all, Runtime, SearchParams, SearchResponse};

use std::sync::OnceLock;
static GLOBAL_FEED_CACHE: OnceLock<std::sync::Arc<crate::feeds::FeedCache>> = OnceLock::new();

fn get_global_feed_cache(settings: &Settings) -> std::sync::Arc<crate::feeds::FeedCache> {
    GLOBAL_FEED_CACHE.get_or_init(|| {
        let config = crate::feeds::PollerConfig {
            retention_days: settings.feeds.retention_days,
            poll_interval_mins: settings.feeds.poll_interval_mins,
            timeout_secs: 30,
            user_agent: "Metasearch/1.0".into(),
        };
        std::sync::Arc::new(crate::feeds::FeedCache::new(config).expect("Failed to init feed cache"))
    }).clone()
}

const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_REQUEST_BYTES: usize = 256 * 1024;

/// Real-time metrics for lightweight performance monitoring.
/// Uses atomic counters for lock-free thread safety.
struct Metrics {
    /// Total requests received since startup.
    total_requests: AtomicU64,
    /// Total requests that resulted in errors (4xx/5xx).
    total_errors: AtomicU64,
    /// Sum of all response times in microseconds (for avg calculation).
    total_response_time_us: AtomicU64,
    /// Cache hits.
    cache_hits: AtomicU64,
    /// Cache misses.
    cache_misses: AtomicU64,
    /// Rolling window for requests-per-minute calculation.
    /// Stores (timestamp_secs, count) pairs for the last 60 seconds.
    minute_buckets: Mutex<Vec<(u64, u64)>>,
}

impl Metrics {
    fn new() -> Self {
        Metrics {
            total_requests: AtomicU64::new(0),
            total_errors: AtomicU64::new(0),
            total_response_time_us: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
            minute_buckets: Mutex::new(Vec::with_capacity(60)),
        }
    }

    /// Record a completed request.
    fn record_request(&self, response_time_us: u64, is_error: bool) {
        self.total_requests.fetch_add(1, Ordering::Relaxed);
        self.total_response_time_us.fetch_add(response_time_us, Ordering::Relaxed);
        if is_error {
            self.total_errors.fetch_add(1, Ordering::Relaxed);
        }

        // Update per-second bucket for RPM calculation
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if let Ok(mut buckets) = self.minute_buckets.lock() {
            // Prune old buckets (older than 60 seconds)
            let cutoff = now_secs.saturating_sub(60);
            buckets.retain(|(ts, _)| *ts > cutoff);
            // Increment or add bucket for current second
            if let Some((ts, count)) = buckets.last_mut() {
                if *ts == now_secs {
                    *count += 1;
                } else {
                    buckets.push((now_secs, 1));
                }
            } else {
                buckets.push((now_secs, 1));
            }
        }
    }

    /// Record a cache hit.
    fn record_cache_hit(&self) {
        self.cache_hits.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a cache miss.
    fn record_cache_miss(&self) {
        self.cache_misses.fetch_add(1, Ordering::Relaxed);
    }

    /// Get total requests count.
    fn total_requests(&self) -> u64 {
        self.total_requests.load(Ordering::Relaxed)
    }

    /// Get total errors count.
    fn total_errors(&self) -> u64 {
        self.total_errors.load(Ordering::Relaxed)
    }

    /// Get requests per minute (rolling 60-second window).
    fn requests_per_minute(&self) -> f64 {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let cutoff = now_secs.saturating_sub(60);
        if let Ok(buckets) = self.minute_buckets.lock() {
            let count: u64 = buckets.iter()
                .filter(|(ts, _)| *ts > cutoff)
                .map(|(_, c)| *c)
                .sum();
            count as f64
        } else {
            0.0
        }
    }

    /// Get cache hit ratio (0.0 to 1.0).
    fn cache_hit_ratio(&self) -> f64 {
        let hits = self.cache_hits.load(Ordering::Relaxed);
        let misses = self.cache_misses.load(Ordering::Relaxed);
        let total = hits + misses;
        if total == 0 {
            0.0
        } else {
            hits as f64 / total as f64
        }
    }

    /// Get average response time in milliseconds.
    fn avg_response_time_ms(&self) -> f64 {
        let total_us = self.total_response_time_us.load(Ordering::Relaxed);
        let count = self.total_requests.load(Ordering::Relaxed);
        if count == 0 {
            0.0
        } else {
            (total_us as f64 / count as f64) / 1000.0
        }
    }

    /// Get error rate (0.0 to 1.0).
    fn error_rate(&self) -> f64 {
        let errors = self.total_errors.load(Ordering::Relaxed);
        let total = self.total_requests.load(Ordering::Relaxed);
        if total == 0 {
            0.0
        } else {
            errors as f64 / total as f64
        }
    }

    /// Get cache stats (hits, misses).
    fn cache_stats(&self) -> (u64, u64) {
        (
            self.cache_hits.load(Ordering::Relaxed),
            self.cache_misses.load(Ordering::Relaxed),
        )
    }
}

struct Ctx {
    /// Live settings, swappable at runtime by the `/preferences` editor.
    settings: RwLock<Arc<Settings>>,
    /// Where settings are persisted (resolved from `$METASEARCH_SETTINGS` or
    /// `./settings.yml`); writes from `/preferences` land here.
    settings_path: PathBuf,
    rt: Arc<Runtime>,
    started: Instant,
    /// Per-IP rate limiter for /search and /preferences endpoints.
    rate_limiter: RateLimiter,
    /// Real-time performance metrics.
    metrics: Metrics,
}

impl Ctx {
    /// A cheap snapshot of the current settings (clones the inner `Arc`, never
    /// holding the lock across an `.await`).
    fn settings(&self) -> Arc<Settings> {
        self.settings
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|p| p.into_inner().clone())
    }
}

// ----------------------------------------------------------------- Rate Limiting

/// Per-IP rate limit tracker entry.
#[derive(Debug, Clone)]
struct RateLimitEntry {
    /// Timestamps of requests within the current window.
    timestamps: Vec<Instant>,
}

/// In-memory rate limiter keyed by IP address.
struct RateLimiter {
    /// Map of IP -> (endpoint -> entry). Endpoint is "search" or "preferences".
    entries: Mutex<HashMap<IpAddr, HashMap<String, RateLimitEntry>>>,
}

impl RateLimiter {
    fn new() -> Self {
        RateLimiter {
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Check if the request is allowed under rate limits. Returns (allowed, remaining).
    /// If allowed, the request is recorded.
    fn check_and_record(
        &self,
        ip: IpAddr,
        endpoint: &str,
        limit: u32,
        window_secs: u64,
    ) -> (bool, u32) {
        let now = Instant::now();
        let window = Duration::from_secs(window_secs);

        let mut guard = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        let ip_entries = guard.entry(ip).or_insert_with(HashMap::new);
        let entry = ip_entries
            .entry(endpoint.to_string())
            .or_insert_with(|| RateLimitEntry {
                timestamps: Vec::new(),
            });

        // Prune old timestamps outside the window
        entry.timestamps.retain(|&t| now.duration_since(t) < window);

        let count = entry.timestamps.len() as u32;
        if count >= limit {
            // Rate limit exceeded
            (false, 0)
        } else {
            // Allow and record
            entry.timestamps.push(now);
            let remaining = limit.saturating_sub(count + 1);
            (true, remaining)
        }
    }

    /// Periodically clean up stale entries (IPs with no recent requests).
    fn cleanup(&self, window_secs: u64) {
        let now = Instant::now();
        let window = Duration::from_secs(window_secs);

        let mut guard = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        guard.retain(|_, endpoints| {
            endpoints.retain(|_, entry| {
                entry.timestamps.retain(|&t| now.duration_since(t) < window);
                !entry.timestamps.is_empty()
            });
            !endpoints.is_empty()
        });
    }
}


/// Hostname suitable for browser/curl URLs (maps `0.0.0.0` → loopback).
fn browser_host(bind_address: &str) -> &str {
    match bind_address {
        "0.0.0.0" | "" => "127.0.0.1",
        host => host,
    }
}

/// Bind a TCP listener, printing actionable errors on failure.
async fn bind_listener(addr: &str, _bind_address: &str, port: u16) -> std::io::Result<TcpListener> {
    match TcpListener::bind(addr).await {
        Ok(l) => Ok(l),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            eprintln!("error: cannot bind to {addr}: port {port} already in use");
            eprintln!("  Find what is listening:  lsof -i :{port}");
            eprintln!("  Stop it, or change server.port in settings.yml (or set METASEARCH_PORT).");
            eprintln!("  Quick start:  ./scripts/run.sh");
            Err(e)
        }
        Err(e) => {
            eprintln!("error: cannot bind to {addr}: {e}");
            eprintln!(
                "  Check server.bind_address / server.port in settings.yml                  (or METASEARCH_BIND / METASEARCH_PORT)."
            );
            Err(e)
        }
    }
}

#[cfg(target_os = "macos")]
fn launch_browser(url: &str) {
    use std::process::Command;
    if let Err(e) = Command::new("open").arg(url).status() {
        eprintln!("note: could not open browser ({e}); visit {url}/ manually");
    }
}

#[cfg(not(target_os = "macos"))]
fn launch_browser(url: &str) {
    let _ = url;
}

async fn accept_connection(
    stream: TcpStream,
    peer_addr: Option<IpAddr>,
    ctx: Arc<Ctx>,
    limiter: Arc<Semaphore>,
    read_timeout: Duration,
) {
    tokio::spawn(async move {
        let _permit = match limiter.try_acquire() {
            Ok(p) => p,
            Err(_) => match limiter.acquire().await {
                Ok(p) => p,
                Err(_) => return,
            },
        };
        if let Err(e) = handle_connection(stream, peer_addr, &ctx, read_timeout).await {
            crate::obs::warn(format!("connection error: {e}"));
            crate::logging::error_occurred(&e.to_string(), "handle_connection");
        }
    });
}

/// Bind and serve until Ctrl-C. When `open_browser` is true, opens the UI on macOS.
pub async fn serve(settings: Settings, open_browser: bool) -> std::io::Result<()> {
    crate::obs::init();
    crate::logging::init();
    let bind_address = settings.server.bind_address.clone();
    let port = settings.server.port;
    let addr = format!("{bind_address}:{port}");
    let listener_v4 = bind_listener(&addr, &bind_address, port).await?;
    // macOS resolves `localhost` to ::1 first; also listen on IPv6 loopback when bound to 127.0.0.1.
    let listener_v6 = if bind_address == "127.0.0.1" {
        let v6_addr = format!("[::1]:{port}");
        bind_listener(&v6_addr, "::1", port).await.ok()
    } else {
        None
    };

    let max_conns = settings.server.max_connections.max(1);
    let read_timeout = Duration::from_secs(settings.server.read_timeout_secs.max(1));

    let settings_path = std::env::var("METASEARCH_SETTINGS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("settings.yml"));

    let ctx = Arc::new(Ctx {
        rt: Arc::new(Runtime::new(&settings)),
        settings: RwLock::new(Arc::new(settings)),
        settings_path,
        started: Instant::now(),
        rate_limiter: RateLimiter::new(),
        metrics: Metrics::new(),
    });

    // Load discover cache from disk on startup
    const DISCOVER_CACHE_PATH: &str = "data/discover_cache.json";
    ctx.rt.discover_snapshot_cache.load_from_disk(DISCOVER_CACHE_PATH);

    let limiter = Arc::new(Semaphore::new(max_conns));

    // Background: periodic discover cache save (every 5 minutes)
    {
        let save_ctx = ctx.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(300)).await;
                save_ctx.rt.discover_snapshot_cache.save_to_disk(DISCOVER_CACHE_PATH);
            }
        });
    }

    // Background: periodic rate limiter cleanup (every 5 minutes)
    {
        let cleanup_ctx = ctx.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(300)).await;
                let window = cleanup_ctx.settings().server.rate_limit.window_secs;
                cleanup_ctx.rate_limiter.cleanup(window);
            }
        });
    }

    // Background: RSS feed poller (standalone mode)
    {
        let settings = ctx.settings();
        if settings.feeds.enabled {
            // Share the feed cache with local_feeds engine
            let cache = get_global_feed_cache(&settings);
            crate::engines::local_feeds::set_feed_cache(cache.clone());
        }

        let poller_ctx = ctx.clone();
        tokio::spawn(async move {
            let settings = poller_ctx.settings();
            if !settings.feeds.enabled || settings.feeds.poll_interval_mins == 0 {
                eprintln!("[RSS] Polling disabled (enabled={}, poll_interval={})",
                    settings.feeds.enabled, settings.feeds.poll_interval_mins);
                return;
            }
            let cache = get_global_feed_cache(&settings);
            let languages: Vec<String> = if settings.feeds.languages.is_empty() {
                cache.languages().iter().map(|s| s.to_string()).collect()
            } else {
                settings.feeds.languages.clone()
            };
            eprintln!("Starting RSS poller for {} languages", languages.len());
            let poller = crate::feeds::FeedPoller::new(cache, languages);
            poller.run().await;
        });

        // Start embedding worker (background, separate from indexing)
        let embed_ctx = Arc::clone(&ctx);
        tokio::spawn(async move {
            let settings = embed_ctx.settings();
            let ollama_url = "http://localhost:11434".to_string();

            // Check if Ollama is available before starting embedding worker
            let ollama_available = reqwest::Client::new()
                .get(format!("{}/api/tags", &ollama_url))
                .timeout(std::time::Duration::from_secs(2))
                .send()
                .await
                .map(|r| r.status().is_success())
                .unwrap_or(false);

            if !ollama_available {
                eprintln!("[Embedding] Ollama not available at {} - embedding worker disabled", ollama_url);
                return;
            }

            let db_path = std::env::var("METASEARCH_DATA_DIR")
                .unwrap_or_else(|_| "data".to_string());
            let db_path = format!("{}/articles.db", db_path);

            match crate::feeds::embeddings::EmbeddingStore::open(&db_path, &ollama_url) {
                Ok(store) => {
                    let store = std::sync::Arc::new(store);
                    // Initialize category embeddings on startup
                    match store.init_category_embeddings().await {
                        Ok(n) if n > 0 => eprintln!("[Embedding] Initialized {} category embeddings", n),
                        Ok(_) => eprintln!("[Embedding] Category embeddings ready"),
                        Err(e) => eprintln!("[Embedding] Category init error: {}", e),
                    }
                    let worker = crate::feeds::embeddings::EmbeddingWorker::new(store);
                    worker.run().await;
                }
                Err(e) => {
                    eprintln!("[Embedding] Failed to start worker: {}", e);
                }
            }
        });
    }

    let display_host = browser_host(&bind_address);
    let base_url = format!("http://{display_host}:{port}");

    // Log server start in JSON format
    crate::logging::server_start(port, crate::build_info::VERSION, &bind_address);

    eprintln!("Listening on {base_url}/");
    if listener_v6.is_some() {
        eprintln!("  Also reachable at http://localhost:{port}/ (IPv6 loopback)");
    }
    eprintln!("  Open in browser:  {base_url}/  (classic search: {base_url}/classic)");
    eprintln!("  Use http:// (not https). Press Ctrl+C to stop.");
    eprintln!(
        "  JSON API:  curl '{base_url}/search?q=hello&format=json'\n  enabled engines: {}",
        ctx.settings()
            .engines
            .iter()
            .filter(|e| e.enabled)
            .map(|e| e.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );

    // Background: warm Discover cache for common categories (initial + periodic refresh)
    // DISABLED: warming competes with user requests for DB - use disk cache instead
    if false && ctx.settings().search.news.discover_cache_ttl_hours > 0 {
        let warm_ctx = ctx.clone();
        tokio::spawn(async move {
            // Initial warm
            warm_discover_cache(&warm_ctx).await;

            // Periodic refresh every 3 minutes
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(180));
            interval.tick().await; // skip first immediate tick
            loop {
                interval.tick().await;
                eprintln!("[metasearch] Refreshing Discover cache...");
                warm_discover_cache(&warm_ctx).await;
            }
        });
    }

    if open_browser {
        launch_browser(&format!("{base_url}/"));
    }

    match listener_v6 {
        Some(listener_v6) => loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    crate::logging::server_shutdown();
                    eprintln!("\nmetasearch shutting down");
                    break;
                }
                accepted = listener_v4.accept() => {
                    let (stream, addr) = accepted?;
                    let peer_ip = addr.ip();
                    accept_connection(stream, Some(peer_ip), ctx.clone(), limiter.clone(), read_timeout).await;
                }
                accepted = listener_v6.accept() => {
                    let (stream, addr) = accepted?;
                    let peer_ip = addr.ip();
                    accept_connection(stream, Some(peer_ip), ctx.clone(), limiter.clone(), read_timeout).await;
                }
            }
        },
        None => loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    crate::logging::server_shutdown();
                    eprintln!("\nmetasearch shutting down");
                    break;
                }
                accepted = listener_v4.accept() => {
                    let (stream, addr) = accepted?;
                    let peer_ip = addr.ip();
                    accept_connection(stream, Some(peer_ip), ctx.clone(), limiter.clone(), read_timeout).await;
                }
            }
        },
    }
    Ok(())
}

async fn handle_connection(
    mut stream: TcpStream,
    peer_addr: Option<IpAddr>,
    ctx: &Ctx,
    read_timeout: Duration,
) -> std::io::Result<()> {
    let request_start = Instant::now();
    let req = match read_request(&mut stream, read_timeout).await {
        Ok(Some(req)) => req,
        _ => return Ok(()), // timeout, EOF or read error: drop silently
    };
    let RequestHeaders {
        request_line,
        cookies,
        authorization,
        body,
    } = req;
    let theme = Theme::from_cookies(&cookies);

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/");

    // Log request received
    crate::logging::request_received(
        method,
        target,
        peer_addr.as_ref().map(|ip| ip.to_string()).as_deref(),
    );

    if method != "GET" && method != "HEAD" && method != "POST" {
        return write_response(
            &mut stream,
            &Response::text(
                405,
                "text/plain; charset=utf-8",
                "method not allowed".into(),
            ),
            true,
        )
        .await;
    }

    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };

    // Rate limiting check for /search and /preferences endpoints
    let rate_limit_result = check_rate_limit(path, peer_addr, ctx);
    if let Some((false, _)) = rate_limit_result {
        if let Some(ip) = peer_addr {
            crate::logging::rate_limited(&ip.to_string(), path);
        }
        let duration_ms = request_start.elapsed().as_millis();
        crate::logging::request_completed(method, path, 429, duration_ms);
        return write_response(&mut stream, &Response::rate_limited(), true).await;
    }

    // Admin authentication for /preferences endpoints (disabled for personal use)
    // To enable: set ORGOS_ADMIN_AUTH=true
    let auth_enabled = std::env::var("ORGOS_ADMIN_AUTH").map(|v| v == "true" || v == "1").unwrap_or(false);
    if auth_enabled && (path == "/preferences" || path == "/preferences/logout" || path == "/login") {
        match check_admin_auth(&cookies, authorization.as_deref()) {
            Ok(Some(token)) => {
                // Authenticated via Basic Auth, set session cookie and redirect
                let mut resp = Response::redirect("/preferences".to_string());
                resp.set_cookie = Some(format!(
                    "{}={}; Path=/; Max-Age=86400; SameSite=Lax; HttpOnly",
                    ADMIN_SESSION_COOKIE, token
                ));
                return write_response(&mut stream, &resp, true).await;
            }
            Ok(None) => {
                // Already authenticated via session cookie, proceed
                if path == "/preferences/logout" {
                    // Clear session and redirect to home
                    let mut resp = Response::redirect("/".to_string());
                    resp.set_cookie = Some(format!(
                        "{}=; Path=/; Max-Age=0; SameSite=Lax; HttpOnly",
                        ADMIN_SESSION_COOKIE
                    ));
                    return write_response(&mut stream, &resp, true).await;
                }
            }
            Err(resp) => {
                // Not authenticated, show login page
                return write_response(&mut stream, &resp, true).await;
            }
        }
    }

    // Streaming endpoints write directly to the socket.
    if path == "/api/v1/research" && (method == "GET" || method == "POST") {
        let src = if method == "POST" && !body.is_empty() {
            body.as_str()
        } else {
            query
        };
        let is_json = method == "POST" && body.trim_start().starts_with('{');
        if let Ok(req) = crate::api::parse_research_request(src, is_json) {
            if req.stream {
                return stream_research_sse(&mut stream, &req, ctx).await;
            }
        }
    }
    if path == "/answer" && (method == "GET" || method == "POST") {
        return stream_answer_sse(&mut stream, method, query, &body, ctx).await;
    }
    if path == "/api/v1/news_article" && (method == "GET" || method == "POST") {
        let src = if method == "POST" && !body.is_empty() {
            body.as_str()
        } else {
            query
        };
        if matches!(param(src, "stream").as_str(), "1" | "true" | "on" | "yes") {
            return stream_news_article_sse(&mut stream, src, ctx).await;
        }
    }

    let mut response = route(method, path, query, &body, theme, ctx).await;
    // Add rate limit remaining header if applicable
    if let Some((true, remaining)) = rate_limit_result {
        response = response.with_rate_limit_remaining(remaining);
    }
    let body_omitted = method == "HEAD";

    // Log request completion
    let duration_us = request_start.elapsed().as_micros() as u64;
    let duration_ms = duration_us / 1000;
    let is_error = response.status >= 400;
    ctx.metrics.record_request(duration_us, is_error);
    crate::logging::request_completed(method, path, response.status, duration_ms as u128);

    write_response(&mut stream, &response, !body_omitted).await
}

/// Check rate limit for the given path and IP. Returns None if rate limiting
/// doesn't apply to this path, or Some((allowed, remaining)) if it does.
fn check_rate_limit(path: &str, peer_addr: Option<IpAddr>, ctx: &Ctx) -> Option<(bool, u32)> {
    let settings = ctx.settings();
    if !settings.server.rate_limit.enabled {
        return None;
    }

    let peer_ip = peer_addr?;
    let window = settings.server.rate_limit.window_secs;

    let (endpoint, limit) = if path == "/search" || path.starts_with("/search?") {
        ("search", settings.server.rate_limit.search_requests_per_minute)
    } else if path == "/preferences" || path.starts_with("/preferences?") {
        (
            "preferences",
            settings.server.rate_limit.preferences_requests_per_minute,
        )
    } else {
        return None;
    };

    Some(ctx.rate_limiter.check_and_record(peer_ip, endpoint, limit, window))
}

/// UI colour theme, persisted in the `theme` cookie on the standalone server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Theme {
    Auto,
    Light,
    Dark,
}

impl Theme {
    fn from_cookies(cookies: &str) -> Self {
        for part in cookies.split(';') {
            if let Some((k, v)) = part.split_once('=') {
                if k.trim() == "theme" {
                    return match v.trim() {
                        "light" => Theme::Light,
                        "dark" => Theme::Dark,
                        _ => Theme::Auto,
                    };
                }
            }
        }
        Theme::Auto
    }

    fn as_str(&self) -> &'static str {
        match self {
            Theme::Auto => "auto",
            Theme::Light => "light",
            Theme::Dark => "dark",
        }
    }

    /// `data-theme` attribute value for `<html>` (empty for auto = follow OS).
    fn attr(&self) -> &'static str {
        match self {
            Theme::Auto => "",
            Theme::Light => " data-theme=\"light\"",
            Theme::Dark => " data-theme=\"dark\"",
        }
    }
}

async fn route(
    method: &str,
    path: &str,
    query: &str,
    body: &str,
    theme: Theme,
    ctx: &Ctx,
) -> Response {
    // Login form submission
    if path == "/login" && method == "POST" {
        return handle_login(body);
    }
    // The only settings-write endpoint: persist edited preferences.
    if path == "/preferences" && method == "POST" {
        return save_preferences(body, theme, ctx).await;
    }
    // `/followups` accepts POST (so a prior answer/context can ride in the body)
    // as well as GET; `/api/v1/research` accepts POST JSON.
    if method == "POST" && path == "/followups" {
        return followups_json(body, ctx).await;
    }
    if method == "POST" && path == "/api/v1/research" {
        return research_json(body, true, ctx).await;
    }
    if method == "POST" && path == "/api/v1/cache/clear" {
        return cache_clear_json(ctx);
    }
    if method == "POST" && path == "/api/v1/news_images" {
        return news_images_json(method, query, body, ctx).await;
    }
    if method == "POST" && (path == "/api/v1/answer" || path == "/api/v1/followups") {
        return match path {
            "/api/v1/answer" => agent_answer_json(body, ctx).await,
            _ => agent_followups_json(body, ctx).await,
        };
    }
    if method == "POST" && path == "/api/v1/feed_subscribe" {
        return feed_subscribe_proxy(body, ctx).await;
    }
    if method == "POST" && path == "/api/v1/translate" {
        return translate_query_json(body, ctx).await;
    }
    if method == "POST" {
        return Response::text(
            405,
            "text/plain; charset=utf-8",
            "method not allowed".into(),
        );
    }
    match path {
        "/healthz" => Response::text(200, "text/plain; charset=utf-8", "ok".into()),
        "/health" => health_json(ctx),
        "/config" => Response::json(config_json(&ctx.settings())),
        "/stats" => {
            if param(query, "format") == "json" {
                Response::json(stats_json(ctx))
            } else {
                Response::html(stats_page(ctx, theme))
            }
        }
        "/stats.json" => Response::json(stats_json(ctx)),
        "/theme" => set_theme(query),
        "/opensearch.xml" => Response::text(
            200,
            "application/opensearchdescription+xml; charset=utf-8",
            opensearch_xml(&ctx.settings()),
        ),
        "/favicon.svg" => Response::text(
            200,
            "image/svg+xml",
            r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><circle cx="32" cy="32" r="28" stroke="#0d9488" stroke-width="4" fill="none"/><circle cx="32" cy="32" r="12" fill="#0d9488"/></svg>"##.into(),
        ),
        "/favicon.ico" => Response::text(
            200,
            "image/svg+xml",
            r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><circle cx="32" cy="32" r="28" stroke="#0d9488" stroke-width="4" fill="none"/><circle cx="32" cy="32" r="12" fill="#0d9488"/></svg>"##.into(),
        ),
        "/manifest.json" => Response::json(include_str!("../../static/manifest.json").to_string()),
        "/sw.js" => Response::text(200, "application/javascript; charset=utf-8", include_str!("../../static/sw.js").into()),
        "/hls.min.js" => Response::text(200, "application/javascript; charset=utf-8", include_str!("../../static/hls.min.js").into()),
        "/icon-192.png" | "/icon-512.png" => Response::text(
            200,
            "image/svg+xml",
            r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 512 512"><rect width="512" height="512" rx="96" fill="#0d9488"/><circle cx="256" cy="256" r="120" stroke="white" stroke-width="24" fill="none"/><circle cx="256" cy="256" r="48" fill="white"/></svg>"##.into(),
        ),
        "/autocompleter" => {
            let q = param(query, "q");
            let sugg = autocomplete(&q, &ctx.settings(), &ctx.rt).await;
            Response::json(serde_json::json!([q, sugg]).to_string())
        }
        "/image_proxy" => image_proxy(query, ctx).await,
        "/preferences" => Response::html(preferences_page(&ctx.settings(), false, theme)),
        "/classic" => {
            if param(query, "q").is_empty() {
                Response::html(home_page(&ctx.settings(), theme))
            } else {
                run_search(query, theme, ctx).await
            }
        }
        "/" => Response::html(answer_ui_page(theme, &ctx.settings())),
        "/search" => run_search(query, theme, ctx).await,
        "/followups" => followups_json(query, ctx).await,
        "/openapi.json" => Response::json(crate::api::openapi_spec(&ctx.settings()).to_string()),
        "/llms.txt" => Response::text(200, "text/plain; charset=utf-8", llms_txt()),
        "/.well-known/ai-handoff.json" => Response::json(ai_handoff_json()),
        "/ai-handoff" | "/docs/project-status" => Response::html(ai_handoff_page()),
        "/api/v1/research" => research_json(query, false, ctx).await,
        "/api/v1/search" => agent_search_json(query, ctx).await,
        "/api/v1/answer" => agent_answer_json(query, ctx).await,
        "/api/v1/followups" => agent_followups_json(query, ctx).await,
        "/api/v1/health" => Response::json(agent_health_json(ctx)),
        "/api/v1/models" => models_json(ctx).await,
        "/api/v1/engines" => {
            Response::json(crate::api::engines_matrix(&ctx.settings()).to_string())
        }
        "/api/v1/discover_snapshot" => discover_snapshot_json(query, ctx).await,
        "/api/v1/trending" => trending_json(query, ctx).await,
        "/api/v1/telegram" => telegram_json(query, ctx).await,
        "/api/v1/news_digest" => news_digest_json(query, ctx).await,
        "/api/v1/news_images" => news_images_json(method, query, body, ctx).await,
        "/api/v1/news_article" => news_article_json(query, ctx).await,
        "/api/v1/cache/clear" => cache_clear_json(ctx),
        "/api/v1/feed_recommend" => feed_recommend_proxy(query, ctx).await,
        "/api/v1/global_feed" => global_feed_proxy(query, ctx).await,
        "/api/v1/standalone_feed" => standalone_feed_json(query, ctx).await,
        "/api/v1/feed_pool" => feed_pool_json(ctx).await,
        "/api/v1/feed_manager/stats" => feed_manager_stats_json(ctx).await,
        // "/api/v1/local_trending" => local_trending_json(query, ctx).await, // disabled: hardcoded filters don't scale
        "/api/v1/briefing" => briefing_proxy(query).await,
        "/api/v1/briefing/audio" => briefing_audio_proxy(query).await,
        "/api/v1/radio/search" => radio_search(query),
        "/api/v1/radio/stations" => radio_recommend(query),
        "/api/v1/radio/recommend" => radio_recommend(query),
        "/api/v1/radio/genre" => radio_by_genre(query),
        "/api/v1/radio/country" => radio_by_country(query),
        "/api/v1/radio/stream" => radio_stream_proxy(query).await,
        "/api/v1/lens" => lens_analyze_json(query, ctx).await,
        "/api/v1/followup" => followup_answer_json(query, ctx).await,
        "/lang/en.json" => Response::json(include_str!("../../static/lang/en.json").to_string()),
        "/lang/ko.json" => Response::json(include_str!("../../static/lang/ko.json").to_string()),
        "/lang/ja.json" => Response::json(include_str!("../../static/lang/ja.json").to_string()),
        "/lang/zh.json" => Response::json(include_str!("../../static/lang/zh.json").to_string()),
        "/lang/es.json" => Response::json(include_str!("../../static/lang/es.json").to_string()),
        "/lang/fr.json" => Response::json(include_str!("../../static/lang/fr.json").to_string()),
        "/lang/de.json" => Response::json(include_str!("../../static/lang/de.json").to_string()),
        "/lang/pt.json" => Response::json(include_str!("../../static/lang/pt.json").to_string()),
        "/lang/it.json" => Response::json(include_str!("../../static/lang/it.json").to_string()),
        "/lang/ru.json" => Response::json(include_str!("../../static/lang/ru.json").to_string()),
        "/lang/nl.json" => Response::json(include_str!("../../static/lang/nl.json").to_string()),
        "/lang/ar.json" => Response::json(include_str!("../../static/lang/ar.json").to_string()),
        "/lang/th.json" => Response::json(include_str!("../../static/lang/th.json").to_string()),
        "/lang/vi.json" => Response::json(include_str!("../../static/lang/vi.json").to_string()),
        "/lang/id.json" => Response::json(include_str!("../../static/lang/id.json").to_string()),
        "/lang/tr.json" => Response::json(include_str!("../../static/lang/tr.json").to_string()),
        "/lang/pl.json" => Response::json(include_str!("../../static/lang/pl.json").to_string()),
        "/lang/sv.json" => Response::json(include_str!("../../static/lang/sv.json").to_string()),
        "/lang/no.json" => Response::json(include_str!("../../static/lang/no.json").to_string()),
        "/lang/da.json" => Response::json(include_str!("../../static/lang/da.json").to_string()),
        "/lang/fi.json" => Response::json(include_str!("../../static/lang/fi.json").to_string()),
        "/lang/hi.json" => Response::json(include_str!("../../static/lang/hi.json").to_string()),
        "/lang/he.json" => Response::json(include_str!("../../static/lang/he.json").to_string()),
        "/lang/el.json" => Response::json(include_str!("../../static/lang/el.json").to_string()),
        "/lang/cs.json" => Response::json(include_str!("../../static/lang/cs.json").to_string()),
        "/lang/hu.json" => Response::json(include_str!("../../static/lang/hu.json").to_string()),
        "/lang/ro.json" => Response::json(include_str!("../../static/lang/ro.json").to_string()),
        "/lang/uk.json" => Response::json(include_str!("../../static/lang/uk.json").to_string()),
        "/lang/ms.json" => Response::json(include_str!("../../static/lang/ms.json").to_string()),
        "/lang/tl.json" => Response::json(include_str!("../../static/lang/tl.json").to_string()),
        "/lang/bn.json" => Response::json(include_str!("../../static/lang/bn.json").to_string()),
        "/lang/af.json" => Response::json(include_str!("../../static/lang/af.json").to_string()),
        "/lang/bg.json" => Response::json(include_str!("../../static/lang/bg.json").to_string()),
        "/lang/ca.json" => Response::json(include_str!("../../static/lang/ca.json").to_string()),
        "/lang/et.json" => Response::json(include_str!("../../static/lang/et.json").to_string()),
        "/lang/eu.json" => Response::json(include_str!("../../static/lang/eu.json").to_string()),
        "/lang/fa.json" => Response::json(include_str!("../../static/lang/fa.json").to_string()),
        "/lang/gl.json" => Response::json(include_str!("../../static/lang/gl.json").to_string()),
        "/lang/hr.json" => Response::json(include_str!("../../static/lang/hr.json").to_string()),
        "/lang/hy.json" => Response::json(include_str!("../../static/lang/hy.json").to_string()),
        "/lang/is.json" => Response::json(include_str!("../../static/lang/is.json").to_string()),
        "/lang/ka.json" => Response::json(include_str!("../../static/lang/ka.json").to_string()),
        "/lang/kk.json" => Response::json(include_str!("../../static/lang/kk.json").to_string()),
        "/lang/km.json" => Response::json(include_str!("../../static/lang/km.json").to_string()),
        "/lang/lt.json" => Response::json(include_str!("../../static/lang/lt.json").to_string()),
        "/lang/lv.json" => Response::json(include_str!("../../static/lang/lv.json").to_string()),
        "/lang/mk.json" => Response::json(include_str!("../../static/lang/mk.json").to_string()),
        "/lang/ml.json" => Response::json(include_str!("../../static/lang/ml.json").to_string()),
        "/lang/mn.json" => Response::json(include_str!("../../static/lang/mn.json").to_string()),
        "/lang/mr.json" => Response::json(include_str!("../../static/lang/mr.json").to_string()),
        "/lang/my.json" => Response::json(include_str!("../../static/lang/my.json").to_string()),
        "/lang/ne.json" => Response::json(include_str!("../../static/lang/ne.json").to_string()),
        "/lang/pa.json" => Response::json(include_str!("../../static/lang/pa.json").to_string()),
        "/lang/si.json" => Response::json(include_str!("../../static/lang/si.json").to_string()),
        "/lang/sk.json" => Response::json(include_str!("../../static/lang/sk.json").to_string()),
        "/lang/sl.json" => Response::json(include_str!("../../static/lang/sl.json").to_string()),
        "/lang/sq.json" => Response::json(include_str!("../../static/lang/sq.json").to_string()),
        "/lang/sr.json" => Response::json(include_str!("../../static/lang/sr.json").to_string()),
        "/lang/sw.json" => Response::json(include_str!("../../static/lang/sw.json").to_string()),
        "/lang/ta.json" => Response::json(include_str!("../../static/lang/ta.json").to_string()),
        "/lang/te.json" => Response::json(include_str!("../../static/lang/te.json").to_string()),
        "/lang/ur.json" => Response::json(include_str!("../../static/lang/ur.json").to_string()),
        "/lang/uz.json" => Response::json(include_str!("../../static/lang/uz.json").to_string()),
        _ => Response::text(404, "text/plain; charset=utf-8", "not found".into()),
    }
}

/// `GET /theme?set=dark&to=/search?q=x` — persist the theme cookie and redirect
/// back to `to` (defaults to `/`). The cookie is read on every subsequent
/// render, so the choice survives across the standalone server.
fn set_theme(query: &str) -> Response {
    let set = param(query, "set");
    let theme = match set.as_str() {
        "light" => "light",
        "dark" => "dark",
        _ => "auto",
    };
    let to = {
        let t = param(query, "to");
        if t.starts_with('/') {
            t
        } else {
            "/".to_string()
        }
    };
    let mut resp = Response::redirect(to);
    // 1 year, site-wide, lax.
    resp.set_cookie = Some(format!(
        "theme={theme}; Path=/; Max-Age=31536000; SameSite=Lax"
    ));
    resp
}

/// Form fields: `default_language`, `default_lang`, `safe_search` (0/1/2), `results_per_page`, and
/// per-engine `en_<name>` (checkbox = enabled) + `wt_<name>` (weight). On
/// success the new settings are validated, written to disk, swapped into the
/// live `Ctx`, and the result cache is cleared so subsequent searches reflect
/// the change immediately.
async fn save_preferences(body: &str, theme: Theme, ctx: &Ctx) -> Response {
    let fields: std::collections::HashMap<String, String> =
        url::form_urlencoded::parse(body.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();

    let mut settings = (*ctx.settings()).clone();

    // Update built-in engines
    for e in settings.engines.iter_mut() {
        // Unchecked checkboxes are simply absent from the POST body.
        e.enabled = fields.contains_key(&format!("en_{}", e.name));
        if let Some(w) = fields.get(&format!("wt_{}", e.name)) {
            if let Ok(parsed) = w.trim().parse::<f64>() {
                e.weight = parsed;
            }
        }
    }
    // Update custom engines
    for e in settings.custom_engines.iter_mut() {
        e.enabled = fields.contains_key(&format!("en_{}", e.name));
        if let Some(w) = fields.get(&format!("wt_{}", e.name)) {
            if let Ok(parsed) = w.trim().parse::<f64>() {
                e.weight = parsed;
            }
        }
    }
    if let Some(lang) = fields.get("default_lang") {
        let lang = lang.trim();
        if !lang.is_empty() {
            settings.search.default_lang = lang.to_string();
        }
    }
    if let Some(lang) = fields.get("default_language") {
        let lang = lang.trim();
        if !lang.is_empty() {
            settings.search.default_language = lang.to_string();
        }
    }
    if let Some(ss) = fields
        .get("safe_search")
        .and_then(|s| s.trim().parse::<u8>().ok())
    {
        settings.search.safe_search = ss.min(2);
    }
    if let Some(rpp) = fields
        .get("results_per_page")
        .and_then(|s| s.trim().parse::<usize>().ok())
    {
        settings.server.max_results_per_engine = rpp.clamp(1, 50);
    }

    // Branding fields
    if let Some(name) = fields.get("app_name") {
        let name = name.trim();
        if !name.is_empty() {
            settings.branding.app_name = name.to_string();
        }
    }
    if let Some(url) = fields.get("logo_url") {
        let url = url.trim();
        settings.branding.logo_url = if url.is_empty() {
            None
        } else {
            Some(url.to_string())
        };
    }
    if let Some(url) = fields.get("favicon_url") {
        let url = url.trim();
        settings.branding.favicon_url = if url.is_empty() {
            None
        } else {
            Some(url.to_string())
        };
    }

    // AI settings
    settings.ai.enabled = fields.contains_key("ai_enabled");
    if let Some(url) = fields.get("ai_base_url") {
        let url = url.trim();
        if !url.is_empty() {
            settings.ai.base_url = url.to_string();
        }
    }
    if let Some(model) = fields.get("ai_model") {
        let model = model.trim();
        if !model.is_empty() {
            settings.ai.model = model.to_string();
        }
    }
    if let Some(model) = fields.get("ai_article_model") {
        let model = model.trim();
        if !model.is_empty() {
            settings.ai.article_model = model.to_string();
        }
    }
    if let Some(model) = fields.get("ai_embedding_model") {
        let model = model.trim();
        if !model.is_empty() {
            settings.ai.embedding_model = model.to_string();
        }
    }
    if let Some(model) = fields.get("ai_vision_model") {
        let model = model.trim();
        if !model.is_empty() {
            settings.ai.vision_model = model.to_string();
        }
    }
    if let Some(n) = fields.get("ai_answer_top_n").and_then(|s| s.trim().parse::<usize>().ok()) {
        settings.ai.answer_top_n = n.clamp(1, 20);
    }
    if let Some(timeout) = fields.get("ai_timeout_secs").and_then(|s| s.trim().parse::<u64>().ok()) {
        settings.ai.timeout_secs = timeout.max(1);
    }
    settings.ai.answer = fields.contains_key("ai_answer");
    settings.ai.expand = fields.contains_key("ai_expand");
    settings.ai.rerank = fields.contains_key("ai_rerank");
    settings.ai.cluster = fields.contains_key("ai_cluster");
    settings.ai.vision = fields.contains_key("ai_vision");
    if let Some(prompt) = fields.get("ai_news_prompt_ko") {
        settings.ai.news_prompt_ko = prompt.trim().to_string();
    }
    if let Some(prompt) = fields.get("ai_news_prompt_en") {
        settings.ai.news_prompt_en = prompt.trim().to_string();
    }
    if let Some(lang) = fields.get("ai_answer_language") {
        settings.ai.answer_language = lang.trim().to_string();
    }
    // API key (only update if not masked placeholder)
    if let Some(key) = fields.get("ai_api_key") {
        let key = key.trim();
        if !key.is_empty() && !key.starts_with("••") {
            settings.ai.api_key = Some(key.to_string());
        } else if key.is_empty() {
            settings.ai.api_key = None;
        }
    }
    // Cost tracking
    settings.ai.track_usage = fields.contains_key("ai_track_usage");
    if let Some(cost) = fields.get("ai_input_cost").and_then(|s| s.trim().parse::<f64>().ok()) {
        settings.ai.input_cost_per_million = cost.max(0.0);
    }
    if let Some(cost) = fields.get("ai_output_cost").and_then(|s| s.trim().parse::<f64>().ok()) {
        settings.ai.output_cost_per_million = cost.max(0.0);
    }
    if let Some(days) = fields.get("ai_chat_retention_days").and_then(|s| s.trim().parse::<u32>().ok()) {
        settings.ai.chat_retention_days = days;
    }

    // News settings
    if let Some(v) = fields.get("news_per_source_cap").and_then(|s| s.trim().parse::<usize>().ok()) {
        settings.search.news.per_source_cap = v;
    }
    if let Some(v) = fields.get("news_freshness_half_life").and_then(|s| s.trim().parse::<f64>().ok()) {
        settings.search.news.freshness_half_life_hours = v.max(1.0);
    }
    if let Some(v) = fields.get("news_freshness_weight").and_then(|s| s.trim().parse::<f64>().ok()) {
        settings.search.news.freshness_weight = v.clamp(0.0, 1.0);
    }
    if let Some(v) = fields.get("news_dedup_similarity").and_then(|s| s.trim().parse::<f64>().ok()) {
        settings.search.news.dedup_title_similarity = v.clamp(0.0, 1.0);
    }
    if let Some(v) = fields.get("news_max_age_days").and_then(|s| s.trim().parse::<u64>().ok()) {
        settings.search.news.max_age_days = v;
    }
    if let Some(v) = fields.get("news_cache_ttl").and_then(|s| s.trim().parse::<u64>().ok()) {
        settings.search.news.cache_ttl_secs = v;
    }
    if let Some(v) = fields.get("news_enrich_max").and_then(|s| s.trim().parse::<usize>().ok()) {
        settings.search.news.enrich_max = v;
    }
    // Discover settings
    if let Some(v) = fields.get("discover_articles_per_category").and_then(|s| s.trim().parse::<usize>().ok()) {
        settings.search.news.discover_articles_per_category = v.max(1).min(50);
    }
    // Collect selected discover categories
    let all_cats = ["news", "politics", "business", "finance", "tech", "world",
                    "sports", "entertainment", "health", "science", "culture", "opinion", "lifestyle", "society"];
    let selected_cats: Vec<String> = all_cats.iter()
        .filter(|cat| fields.contains_key(&format!("discover_cat_{}", cat)))
        .map(|s| s.to_string())
        .collect();
    settings.search.news.discover_categories = selected_cats;

    // Server settings
    if let Some(addr) = fields.get("bind_address") {
        let addr = addr.trim();
        if !addr.is_empty() {
            settings.server.bind_address = addr.to_string();
        }
    }
    if let Some(p) = fields.get("port").and_then(|s| s.trim().parse::<u16>().ok()) {
        settings.server.port = p.max(1);
    }
    if let Some(v) = fields.get("max_connections").and_then(|s| s.trim().parse::<usize>().ok()) {
        settings.server.max_connections = v.max(1);
    }
    settings.server.image_proxy = fields.contains_key("image_proxy");
    if let Some(backend) = fields.get("cache_backend") {
        let backend = backend.trim();
        if matches!(backend, "memory" | "disk" | "redis") {
            settings.server.cache_backend = backend.to_string();
        }
    }
    if let Some(dir) = fields.get("cache_dir") {
        settings.server.cache_dir = dir.trim().to_string();
    }
    if let Some(url) = fields.get("redis_url") {
        settings.server.redis_url = url.trim().to_string();
    }
    if let Some(v) = fields.get("engine_failure_threshold").and_then(|s| s.trim().parse::<u32>().ok()) {
        settings.server.engine_failure_threshold = v;
    }
    if let Some(v) = fields.get("engine_cooldown_secs").and_then(|s| s.trim().parse::<u64>().ok()) {
        settings.server.engine_cooldown_secs = v;
    }

    // Advanced server settings
    if let Some(ttl) = fields
        .get("cache_ttl_secs")
        .and_then(|s| s.trim().parse::<u64>().ok())
    {
        settings.server.cache_ttl_secs = ttl;
    }
    if let Some(timeout) = fields
        .get("request_timeout_secs")
        .and_then(|s| s.trim().parse::<u64>().ok())
    {
        settings.server.request_timeout_secs = timeout.max(1);
    }

    if let Err(e) = settings.save(&ctx.settings_path) {
        return Response::text(
            400,
            "text/plain; charset=utf-8",
            format!("invalid settings: {e}"),
        );
    }

    let new = Arc::new(settings);
    if let Ok(mut guard) = ctx.settings.write() {
        *guard = new.clone();
    }
    ctx.rt.cache.clear();
    ctx.rt.digest_cache.clear();
    ctx.rt.news_image_cache.clear();
    ctx.rt.article_cache.clear();
    ctx.rt.article_rewrite_cache.clear();

    Response::html(preferences_page(&new, true, theme))
}

fn parse_params(query: &str) -> SearchParams {
    let pageno = param(query, "pageno").parse().unwrap_or(1usize).max(1);
    let language = {
        let l = param(query, "language");
        if l.is_empty() {
            None
        } else {
            Some(l)
        }
    };
    let time_range = {
        let t = param(query, "time_range");
        if matches!(t.as_str(), "day" | "week" | "month" | "year") {
            Some(t)
        } else {
            None
        }
    };
    let safe_search = {
        let s = param(query, "safesearch");
        s.parse::<u8>().ok().filter(|v| *v <= 2)
    };
    let ai_answer = match param(query, "ai").as_str() {
        "" => None,
        "0" | "false" | "off" => Some(false),
        _ => Some(true),
    };
    let categories: Vec<String> = param(query, "categories")
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let context = {
        let c = param(query, "context");
        if c.is_empty() {
            None
        } else {
            Some(c)
        }
    };
    let rerank = match param(query, "rerank").as_str() {
        "" => None,
        "0" | "false" | "off" => Some(false),
        _ => Some(true),
    };
    let deep = match param(query, "deep").as_str() {
        "" => None,
        "0" | "false" | "off" => Some(false),
        _ => Some(true),
    };

    SearchParams {
        query: param(query, "q"),
        categories,
        pageno,
        language,
        time_range,
        safe_search,
        ai_answer,
        context,
        rerank: rerank.or_else(|| deep.filter(|&d| d).map(|_| true)),
        deep,
        ..Default::default()
    }
}

async fn run_search(query: &str, theme: Theme, ctx: &Ctx) -> Response {
    // Input validation: limit query length to prevent abuse
    let q_param = param(query, "q");
    if q_param.len() > 2000 {
        return Response::json(r#"{"error":"Query too long (max 2000 chars)"}"#.into());
    }

    let search_start = Instant::now();
    let format = {
        let f = param(query, "format");
        if f.is_empty() {
            "html".to_string()
        } else {
            f
        }
    };
    let params = parse_params(query);

    // standard `!!` external redirect: bounce the browser straight to the
    // chosen engine (HTML browsing only; JSON/RSS/CSV clients still get data).
    if format == "html" || format == "redirect" {
        if let Some(url) = crate::query::external_redirect(&params.query) {
            return Response::redirect(url);
        }
    }

    let settings = ctx.settings();
    let response = search_all(&params, &settings, &ctx.rt).await;

    // Record cache hit/miss metrics
    if response.cache_hit {
        ctx.metrics.record_cache_hit();
    } else {
        ctx.metrics.record_cache_miss();
    }

    // Log search query
    let engines_used: Vec<&str> = response
        .results
        .iter()
        .flat_map(|r| r.engines.iter().map(|s| s.as_str()))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let search_duration_ms = search_start.elapsed().as_millis();
    crate::logging::search_query(
        &params.query,
        &engines_used,
        response.number_of_results,
        search_duration_ms,
    );

    match format.as_str() {
        "json" => Response::json(serde_json::to_string(&response).unwrap_or_else(|_| "{}".into())),
        "rss" => Response::text(200, "application/rss+xml; charset=utf-8", rss(&response)),
        "csv" => Response {
            status: 200,
            content_type: "text/csv; charset=utf-8".into(),
            body: Body::Text(csv(&response)),
            cache: "no-store",
            location: None,
            set_cookie: None,
            rate_limit_remaining: None,
            www_authenticate: None,
        },
        _ => Response::html(results_page(&response, &settings, theme)),
    }
}

async fn image_proxy(query: &str, ctx: &Ctx) -> Response {
    if !ctx.settings().server.image_proxy {
        return Response::text(
            403,
            "text/plain; charset=utf-8",
            "image proxy disabled".into(),
        );
    }
    let url = param(query, "url");

    // Filter out URLs that are definitely not images
    let url_lower = url.to_lowercase();
    if url_lower.contains("youtube.com/v/")
        || url_lower.contains("youtube.com/watch")
        || url_lower.contains("youtu.be/")
        || url_lower.contains("vimeo.com/")
        || url_lower.contains("dailymotion.com/")
        || url_lower.contains(".html")
        || url_lower.contains(".htm")
        || url_lower.contains(".php")
        || url_lower.contains(".asp")
    {
        return Response::text(415, "text/plain; charset=utf-8", "not an image url".into());
    }

    if !crate::url_safety::is_safe_public_url(&url) {
        return Response::text(400, "text/plain; charset=utf-8", "blocked url".into());
    }
    let client = crate::url_safety::safe_fetch_client();
    let resp = match client
        .get(&url)
        .header("User-Agent", crate::engines::USER_AGENT)
        .timeout(Duration::from_secs(10))
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return Response::text(502, "text/plain; charset=utf-8", "fetch failed".into()),
    };
    if !resp.status().is_success() {
        return Response::text(502, "text/plain; charset=utf-8", "upstream error".into());
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    if !content_type.starts_with("image/") {
        return Response::text(415, "text/plain; charset=utf-8", "not an image".into());
    }
    let bytes = match resp.bytes().await {
        Ok(b) if b.len() <= MAX_IMAGE_BYTES => b,
        _ => return Response::text(502, "text/plain; charset=utf-8", "image too large".into()),
    };
    Response {
        status: 200,
        content_type,
        body: Body::Bytes(bytes.to_vec()),
        cache: "public, max-age=86400",
        location: None,
        set_cookie: None,
        rate_limit_remaining: None,
        www_authenticate: None,
    }
}

/// Decode a single query parameter (handles `+` and `%XX`).
fn param(query: &str, key: &str) -> String {
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

/// Read a full HTTP request: the request line plus the body (for POST). Reads
/// until the end of the headers (`\r\n\r\n`), parses `Content-Length`, then
/// drains exactly that many body bytes. Returns `(request_line, body)`.
/// Read one chunk from `stream` into `tmp`, but never past the overall request
/// `deadline`. Returns `Some(n)` (bytes read; `0` = EOF) or `None` on timeout /
/// deadline / read error. Bounding against a shared deadline (rather than a
/// fresh per-read timeout) is what makes the whole request read slow-loris
/// resistant.
async fn read_until_deadline(
    stream: &mut TcpStream,
    tmp: &mut [u8],
    deadline: Instant,
) -> Option<usize> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return None;
    }
    match tokio::time::timeout(remaining, stream.read(tmp)).await {
        Ok(Ok(n)) => Some(n),
        _ => None,
    }
}

/// Parsed HTTP request headers for authentication.
struct RequestHeaders {
    request_line: String,
    cookies: String,
    authorization: Option<String>,
    body: String,
}

async fn read_request(
    stream: &mut TcpStream,
    read_timeout: Duration,
) -> std::io::Result<Option<RequestHeaders>> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];

    // Bound the *whole* request read (headers + body) by a single deadline, not
    // just each individual read. This stops a slow-loris client from holding a
    // connection open indefinitely by dripping a byte just under the per-read
    // timeout (which would otherwise tie up a connection slot).
    let deadline = Instant::now() + read_timeout;

    // 1. Read until the header terminator.
    let header_end = loop {
        let n = match read_until_deadline(stream, &mut tmp, deadline).await {
            Some(n) => n,
            None => return Ok(None),
        };
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        if buf.len() > MAX_REQUEST_BYTES {
            return Ok(None);
        }
    };

    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("").to_string();
    let mut content_length = 0usize;
    let mut cookies = String::new();
    let mut authorization = None;
    for l in lines {
        let Some((k, v)) = l.split_once(':') else {
            continue;
        };
        let k = k.trim();
        if k.eq_ignore_ascii_case("content-length") {
            content_length = v
                .trim()
                .parse::<usize>()
                .unwrap_or(0)
                .min(MAX_REQUEST_BYTES);
        } else if k.eq_ignore_ascii_case("cookie") {
            cookies = v.trim().to_string();
        } else if k.eq_ignore_ascii_case("authorization") {
            authorization = Some(v.trim().to_string());
        }
    }

    // 2. Drain the body (some may already be buffered after the headers).
    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        let n = match read_until_deadline(stream, &mut tmp, deadline).await {
            Some(n) => n,
            None => break,
        };
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_length.min(body.len()));

    Ok(Some(RequestHeaders {
        request_line,
        cookies,
        authorization,
        body: String::from_utf8_lossy(&body).to_string(),
    }))
}

async fn write_response(
    stream: &mut TcpStream,
    response: &Response,
    include_body: bool,
) -> std::io::Result<()> {
    let reason = match response.status {
        200 => "OK",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        415 => "Unsupported Media Type",
        429 => "Too Many Requests",
        502 => "Bad Gateway",
        _ => "OK",
    };
    let (bytes, len): (&[u8], usize) = match &response.body {
        Body::Text(s) => (s.as_bytes(), s.len()),
        Body::Bytes(b) => (b.as_slice(), b.len()),
    };
    let location_header = match &response.location {
        Some(url) => format!("Location: {url}\r\n"),
        None => String::new(),
    };
    let cookie_header = match &response.set_cookie {
        Some(c) => format!("Set-Cookie: {c}\r\n"),
        None => String::new(),
    };
    let rate_limit_header = match response.rate_limit_remaining {
        Some(r) => format!("X-RateLimit-Remaining: {r}\r\n"),
        None => String::new(),
    };
    let www_auth_header = match &response.www_authenticate {
        Some(v) => format!("WWW-Authenticate: {v}\r\n"),
        None => String::new(),
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {ct}\r\n\
         Content-Length: {len}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         X-Metasearch-Stack: {stack}\r\n\
         X-Metasearch-Version: {version}\r\n\
         {security}\
         {location}{cookie}{rate_limit}{www_auth}\
         Cache-Control: {cache}\r\n\
         Connection: close\r\n\r\n",
        stack = crate::build_info::STACK,
        version = crate::build_info::version_label(),
        status = response.status,
        ct = response.content_type,
        cache = response.cache,
        security = SECURITY_HEADERS,
        location = location_header,
        cookie = cookie_header,
        rate_limit = rate_limit_header,
        www_auth = www_auth_header,
    );
    stream.write_all(head.as_bytes()).await?;
    if include_body {
        stream.write_all(bytes).await?;
    }
    stream.flush().await
}

// ------------------------------------------------------- streaming answer (SSE)

/// Write one Server-Sent Event frame (`event:` + `data:` + blank line) and flush
/// so the client receives it immediately. Payloads are single-line JSON (or
/// short token strings), which are always SSE-safe (no embedded newlines).
async fn sse_write(stream: &mut TcpStream, event: &str, data: &str) -> std::io::Result<()> {
    let frame = format!("event: {event}\ndata: {data}\n\n");
    stream.write_all(frame.as_bytes()).await?;
    stream.flush().await
}

/// `GET|POST /answer?q=&context=&prev_answer=&followups=1` — the streaming
/// streaming answer endpoint.
///
/// Runs the search fan-out, then streams a grounded, cited answer token-by-token
/// from the local model over SSE, followed by the structured citation list.
/// Event taxonomy (in order):
///   * `search`    — `{query, number_of_results}` once results are in
///   * `token`     — `{text}` per answer delta (zero or more)
///   * `error`     — `{message}` if the model is unreachable (search still ok)
///   * `citations` — `[{index,title,url,snippet,engine}, …]` (always)
///   * `followups` — `[question, …]` (only when `followups=1`)
///   * `done`      — `{}` terminal marker (always last)
///
/// Degrades gracefully: with no reachable model (or `ai.enabled=false`) the
/// client still gets `search` + `citations` + a clear `error` + `done`.
async fn stream_answer_sse(
    stream: &mut TcpStream,
    method: &str,
    query: &str,
    body: &str,
    ctx: &Ctx,
) -> std::io::Result<()> {
    // Parameters may arrive in the query string (GET) or the request body (POST).
    let src = if method == "POST" && !body.is_empty() {
        body
    } else {
        query
    };

    // SSE response headers (no Content-Length; the connection closes at the end).
    // `X-Accel-Buffering: no` disables proxy buffering so tokens flush live.
    let head = format!(
        "HTTP/1.1 200 OK\r\n\
        Content-Type: text/event-stream; charset=utf-8\r\n\
        Cache-Control: no-store\r\n\
        Access-Control-Allow-Origin: *\r\n\
        {}\
        X-Accel-Buffering: no\r\n\
        Connection: close\r\n\r\n",
        SECURITY_HEADERS
    );
    stream.write_all(head.as_bytes()).await?;
    stream.flush().await?;

    let q = param(src, "q");
    if q.trim().is_empty() {
        sse_write(
            stream,
            "error",
            &serde_json::json!({"message": "empty query"}).to_string(),
        )
        .await?;
        return sse_write(stream, "done", "{}").await;
    }

    let mut params = parse_params(src);
    // The buffered pipeline must NOT also synthesize an answer — we stream it.
    params.ai_answer = Some(false);
    // Conversational follow-up: fold the prior answer into the refinement context
    // so the model rewrites the new question into a standalone query.
    let prev_answer = param(src, "prev_answer");
    if !prev_answer.trim().is_empty() {
        let prev_q = params.context.clone().unwrap_or_default();
        let snippet: String = prev_answer.chars().take(600).collect();
        params.context = Some(
            format!("{prev_q}\nPrevious answer: {snippet}")
                .trim()
                .to_string(),
        );
    }
    let want_followups = matches!(
        param(src, "followups").as_str(),
        "1" | "true" | "on" | "yes"
    );
    let deep = matches!(param(src, "deep").as_str(), "1" | "true" | "on" | "yes");
    let focus = crate::ai::FocusMode::parse(&param(src, "focus"));
    let model_override = {
        let m = param(src, "model");
        if m.is_empty() {
            None
        } else {
            Some(m)
        }
    };

    let settings = ctx.settings();

    // Fast path for weather queries - skip web search
    if let Some(place) = crate::answerers::weather_request(&params.query) {
        let timeout = std::time::Duration::from_secs(5);
        if let Some(weather_answer) = crate::answerers::fetch_weather(&ctx.rt.client, &place, timeout).await {
            // Send weather mode signal (skip "Searching..." message)
            sse_write(
                stream,
                "weather",
                &serde_json::json!({
                    "query": params.query,
                    "place": place,
                })
                .to_string(),
            )
            .await?;

            // Send weather instant answer
            sse_write(
                stream,
                "answers",
                &serde_json::to_string(&vec![&weather_answer]).unwrap_or_else(|_| "[]".into()),
            )
            .await?;

            // Weather news not available in standalone mode
            let weather_news: Vec<crate::types::SearchResult> = vec![];

            // Send news results
            sse_write(
                stream,
                "results",
                &serde_json::json!({
                    "query": params.query,
                    "number_of_results": weather_news.len(),
                    "results": weather_news,
                })
                .to_string(),
            )
            .await?;

            // AI explanation of weather
            if settings.ai.enabled {
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                let ai = settings.ai.clone();
                let client = ctx.rt.client.clone();
                let wx_data = weather_answer.answer.clone();
                let q = params.query.clone();
                let handle = tokio::spawn(async move {
                    crate::ai::stream_weather_answer(&ai, &client, &q, &wx_data, |t| {
                        let _ = tx.send(t.to_string());
                    }).await
                });
                while let Some(tok) = rx.recv().await {
                    sse_write(stream, "token", &serde_json::json!({ "text": tok }).to_string()).await?;
                }
                let _ = handle.await;
            }

            sse_write(stream, "done", "{}").await?;
            return Ok(());
        }
    }

    if deep {
        params.deep = Some(true);
        if params.rerank.is_none() {
            params.rerank = Some(true);
        }
        if settings.ai.enabled {
            let plan_q = params.query.clone();
            let planned = crate::ai::plan_subqueries(&settings.ai, &ctx.rt.client, &plan_q).await;
            params.deep_subqueries = Some(planned.clone());
            sse_write(
                stream,
                "plan",
                &serde_json::json!({ "subqueries": planned }).to_string(),
            )
            .await?;
        }
    }

    let response = search_all(&params, &settings, &ctx.rt).await;

    sse_write(
        stream,
        "search",
        &serde_json::json!({
            "query": response.query,
            "number_of_results": response.number_of_results,
        })
        .to_string(),
    )
    .await?;

    let citations = crate::ai::build_citations(&response.results, settings.ai.answer_top_n);

    // Check for weather instant answer - use it for AI context instead of search results
    let weather_data: Option<String> = response.answers.iter()
        .find(|a| a.engine == "wttr.in")
        .map(|a| a.answer.clone());

    // Stream the answer: run the (owned) synthesizer in a task and forward token
    // deltas through a channel, writing each as an SSE `token` frame as it lands.
    let mut stream_err: Option<String> = None;
    let mut full_answer = String::new();
    #[allow(unused_assignments)]
    let mut usage: Option<crate::ai::TokenUsage> = None;
    if !settings.ai.enabled {
        stream_err = Some("AI disabled (set ai.enabled / ai.base_url)".to_string());
    } else if response.results.is_empty() && weather_data.is_none() {
        stream_err = Some("no results to summarize".to_string());
    } else {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let ai = settings.ai.clone();
        let client = ctx.rt.client.clone();
        let q_eff = response.query.clone();
        let results = response.results.clone();
        let weather = weather_data.clone();
        let is_deep = deep;
        let handle = tokio::spawn(async move {
            if let Some(ref wx) = weather {
                // Weather query: explain the weather data (no token usage tracking)
                crate::ai::stream_weather_answer(
                    &ai,
                    &client,
                    &q_eff,
                    wx,
                    |t| { let _ = tx.send(t.to_string()); },
                ).await.map(|s| crate::ai::StreamArticleResult { article: s, usage: None })
            } else if is_deep {
                // Deep research: use enhanced multi-source analysis prompt
                crate::ai::stream_answer_deep(
                    &ai,
                    &client,
                    &q_eff,
                    &results,
                    focus,
                    model_override.as_deref(),
                    |t| { let _ = tx.send(t.to_string()); },
                ).await
            } else {
                crate::ai::stream_answer_with_options(
                    &ai,
                    &client,
                    &q_eff,
                    &results,
                    focus,
                    model_override.as_deref(),
                    |t| { let _ = tx.send(t.to_string()); },
                ).await
            }
        });
        while let Some(tok) = rx.recv().await {
            full_answer.push_str(&tok);
            sse_write(
                stream,
                "token",
                &serde_json::json!({ "text": tok }).to_string(),
            )
            .await?;
        }
        match handle.await {
            Ok(Ok(result)) => {
                full_answer = result.article;
                usage = result.usage;
            }
            Ok(Err(e)) => stream_err = Some(e),
            Err(_) => stream_err = Some("answer task aborted".to_string()),
        }
    }
    // For weather queries, don't show unrelated search result citations
    let final_citations = if weather_data.is_some() {
        vec![]
    } else {
        citations.clone()
    };
    let with_footnotes = crate::ai::append_citation_footnotes(&full_answer, &final_citations);
    if with_footnotes.len() > full_answer.len() {
        let extra = with_footnotes[full_answer.len()..].to_string();
        sse_write(
            stream,
            "token",
            &serde_json::json!({ "text": extra }).to_string(),
        )
        .await?;
    }

    if let Some(msg) = stream_err {
        sse_write(
            stream,
            "error",
            &serde_json::json!({ "message": msg }).to_string(),
        )
        .await?;
    }

    sse_write(
        stream,
        "citations",
        &serde_json::to_string(&final_citations).unwrap_or_else(|_| "[]".into()),
    )
    .await?;

    // For weather queries, tell client to hide results
    if weather_data.is_some() {
        sse_write(
            stream,
            "weather_mode",
            &serde_json::json!({ "hide_results": true }).to_string(),
        )
        .await?;
    }

    // Skip followups for weather queries - not relevant
    if want_followups && settings.ai.enabled && weather_data.is_none() {
        let fu = crate::ai::suggest_followups(
            &settings.ai,
            &ctx.rt.client,
            &response.query,
            &response.results,
        )
        .await;
        sse_write(
            stream,
            "followups",
            &serde_json::to_string(&fu).unwrap_or_else(|_| "[]".into()),
        )
        .await?;
    }

    let done_data = if let Some(u) = usage {
        serde_json::json!({
            "usage": {
                "input_tokens": u.input_tokens,
                "output_tokens": u.output_tokens,
                "total_tokens": u.total_tokens,
                "cost_usd": u.cost_usd,
                "model": u.model
            }
        })
    } else {
        serde_json::json!({})
    };
    sse_write(stream, "done", &done_data.to_string()).await
}

/// `GET|POST /followups?q=...` — JSON `{query, number_of_results, followups:[…]}`
/// of 3-5 suggested follow-up questions for the query's result set. Returns an
/// empty list when AI is disabled or the model is unreachable (graceful).
async fn followups_json(src: &str, ctx: &Ctx) -> Response {
    let q = param(src, "q");
    if q.trim().is_empty() {
        return Response::json(serde_json::json!({"query": "", "followups": []}).to_string());
    }
    let mut params = parse_params(src);
    params.ai_answer = Some(false);
    let settings = ctx.settings();
    let response = search_all(&params, &settings, &ctx.rt).await;
    let followups = if settings.ai.enabled {
        crate::ai::suggest_followups(
            &settings.ai,
            &ctx.rt.client,
            &response.query,
            &response.results,
        )
        .await
    } else {
        Vec::new()
    };
    Response::json(
        serde_json::json!({
            "query": response.query,
            "number_of_results": response.number_of_results,
            "followups": followups,
        })
        .to_string(),
    )
}

/// `GET|POST /api/v1/research` — agent research API (non-streaming JSON).
async fn research_json(src: &str, is_json: bool, ctx: &Ctx) -> Response {
    let req = match crate::api::parse_research_request(src, is_json) {
        Ok(r) => r,
        Err(e) => {
            return Response::text(400, "application/json; charset=utf-8", {
                serde_json::json!({ "error": e }).to_string()
            });
        }
    };
    if req.query.trim().is_empty() {
        return Response::text(
            400,
            "application/json; charset=utf-8",
            serde_json::json!({ "error": "empty query" }).to_string(),
        );
    }
    let settings = ctx.settings();
    let resp = crate::api::run_research(&req, &settings, &ctx.rt).await;
    Response::json(serde_json::to_string(&resp).unwrap_or_else(|_| "{}".into()))
}

/// `GET|POST /api/v1/search?q=&categories=` — stable agent search JSON.
async fn agent_search_json(src: &str, ctx: &Ctx) -> Response {
    let params = parse_params(src);
    if params.query.trim().is_empty() {
        return Response::text(
            400,
            "application/json; charset=utf-8",
            serde_json::json!({ "error": "empty query" }).to_string(),
        );
    }
    let settings = ctx.settings();
    let resp = crate::api::run_agent_search(&params, &settings, &ctx.rt).await;
    Response::json(serde_json::to_string(&resp).unwrap_or_else(|_| "{}".into()))
}

/// `GET|POST /api/v1/answer?q=` — non-streaming grounded answer JSON.
async fn agent_answer_json(src: &str, ctx: &Ctx) -> Response {
    let params = parse_params(src);
    if params.query.trim().is_empty() {
        return Response::text(
            400,
            "application/json; charset=utf-8",
            serde_json::json!({ "error": "empty query" }).to_string(),
        );
    }
    let include_followups = !matches!(
        param(src, "followups").as_str(),
        "0" | "false" | "off" | "no"
    );
    let focus = crate::ai::FocusMode::parse(&param(src, "focus"));
    let model = {
        let m = param(src, "model");
        if m.is_empty() {
            None
        } else {
            Some(m)
        }
    };
    let settings = ctx.settings();
    let resp = crate::api::run_agent_answer(
        &params,
        &settings,
        &ctx.rt,
        include_followups,
        focus,
        model.as_deref(),
    )
    .await;
    Response::json(serde_json::to_string(&resp).unwrap_or_else(|_| "{}".into()))
}

/// `GET|POST /api/v1/followups?q=` — follow-up question suggestions.
async fn agent_followups_json(src: &str, ctx: &Ctx) -> Response {
    let mut params = parse_params(src);
    if params.query.trim().is_empty() {
        return Response::json(serde_json::json!({ "query": "", "followups": [] }).to_string());
    }
    params.ai_answer = Some(false);
    let settings = ctx.settings();
    let response = search_all(&params, &settings, &ctx.rt).await;
    let followups = if settings.ai.enabled {
        crate::ai::suggest_followups(
            &settings.ai,
            &ctx.rt.client,
            &response.query,
            &response.results,
        )
        .await
    } else {
        Vec::new()
    };
    Response::json(
        serde_json::json!({
            "query": response.query,
            "number_of_results": response.number_of_results,
            "followups": followups,
        })
        .to_string(),
    )
}

fn agent_health_json(ctx: &Ctx) -> String {
    let health = ctx.rt.health_snapshot();
    let cooling: Vec<String> = health
        .iter()
        .filter(|(_, h)| h.cooling_down)
        .map(|(n, _)| n.clone())
        .collect();
    serde_json::to_string(&crate::api::AgentHealthResponse {
        status: "ok".into(),
        uptime_secs: ctx.started.elapsed().as_secs(),
        engine_health_enabled: ctx.rt.health.enabled(),
        cooling_down: cooling,
    })
    .unwrap_or_else(|_| "{}".into())
}

/// `GET /health` — comprehensive health check endpoint for monitoring.
///
/// Returns 200 with health metrics when healthy, 503 when critical issues exist.
fn health_json(ctx: &Ctx) -> Response {
    let settings = ctx.settings();
    let health = ctx.rt.health_snapshot();

    // Count total enabled engines
    let engines_total = settings.engines.iter().filter(|e| e.enabled).count();

    // Count healthy engines (enabled and not cooling down)
    let cooling_engines: std::collections::HashSet<_> = health
        .iter()
        .filter(|(_, h)| h.cooling_down)
        .map(|(n, _)| n.as_str())
        .collect();
    let engines_healthy = settings
        .engines
        .iter()
        .filter(|e| e.enabled && !cooling_engines.contains(e.name.as_str()))
        .count();

    // Cache size (memory backend only; others report 0)
    let cache_size = ctx.rt.cache.len();

    // Memory usage (best-effort, may be 0 on some platforms)
    let memory_mb = get_process_memory_mb();

    // Determine status: degraded if >50% of engines are unhealthy, critical if none healthy
    let status = if engines_healthy == 0 && engines_total > 0 {
        "critical"
    } else if engines_healthy < engines_total / 2 {
        "degraded"
    } else {
        "ok"
    };

    // Real-time metrics
    let (cache_hits, cache_misses) = ctx.metrics.cache_stats();

    let body = serde_json::json!({
        "status": status,
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_secs": ctx.started.elapsed().as_secs(),
        "engines_total": engines_total,
        "engines_healthy": engines_healthy,
        "cache_size": cache_size,
        "memory_mb": memory_mb,
        "metrics": {
            "total_requests": ctx.metrics.total_requests(),
            "requests_per_minute": ctx.metrics.requests_per_minute(),
            "cache_hit_ratio": ctx.metrics.cache_hit_ratio(),
            "cache_hits": cache_hits,
            "cache_misses": cache_misses,
            "avg_response_time_ms": ctx.metrics.avg_response_time_ms(),
            "error_rate": ctx.metrics.error_rate(),
            "total_errors": ctx.metrics.total_errors(),
        },
    });

    let response = Response::json(body.to_string());
    if status == "critical" {
        Response {
            status: 503,
            ..response
        }
    } else {
        response
    }
}

/// Get the current process memory usage in MB (best-effort).
#[cfg(target_os = "macos")]
fn get_process_memory_mb() -> u64 {
    // On macOS, read from mach task_info (simplified: use ps as fallback)
    use std::process::Command;
    Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|kb| kb / 1024)
        .unwrap_or(0)
}

#[cfg(target_os = "linux")]
fn get_process_memory_mb() -> u64 {
    // On Linux, read from /proc/self/statm (resident set size in pages)
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
        .map(|pages| pages * 4 / 1024) // 4KB pages -> MB
        .unwrap_or(0)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn get_process_memory_mb() -> u64 {
    0 // Not implemented for other platforms
}

/// `GET /api/v1/models` — safe proxy to configured Ollama `/api/tags`.
async fn models_json(ctx: &Ctx) -> Response {
    let settings = ctx.settings();
    let body = crate::api::fetch_models(&settings, &ctx.rt.client).await;
    Response::json(body.to_string())
}

/// `GET|POST /api/v1/cache/clear` — drop in-memory search, digest, and article caches.
fn cache_clear_json(ctx: &Ctx) -> Response {
    ctx.rt.cache.clear();
    ctx.rt.digest_cache.clear();
    ctx.rt.discover_snapshot_cache.clear();
    ctx.rt.news_image_cache.clear();
    ctx.rt.article_cache.clear();
    ctx.rt.article_rewrite_cache.clear();
    Response::json(
        serde_json::json!({
            "ok": true,
            "cleared": ["search", "digest", "discover_snapshot", "news_images", "article", "article_rewrite"],
            "build": crate::build_info::GIT_SHA,
        })
        .to_string(),
    )
}

/// `GET /api/v1/discover_snapshot?q=&category=&limit=10&offset=0&language=&country=&refresh=1` — daily Discover feed with pagination.
async fn discover_snapshot_json(query: &str, ctx: &Ctx) -> Response {
    let q_param = param(query, "q");
    let category = param(query, "category");
    let default_limit = ctx.settings().search.news.discover_articles_per_category;
    let limit = param(query, "limit").parse::<usize>().unwrap_or(default_limit).min(default_limit);
    let offset = param(query, "offset").parse::<usize>().unwrap_or(0);
    let language = {
        let lang = param(query, "lang");
        if !lang.is_empty() { lang } else { param(query, "language") }
    };
    let country = param(query, "country");
    let refresh = matches!(
        param(query, "refresh").as_str(),
        "1" | "true" | "yes" | "on"
    );
    // Translate query if it matches category name (frontend sends category as q)
    let q = if q_param.is_empty() || q_param == category {
        category_to_seed_query(&category, &language)
    } else {
        q_param
    };
    // Fetch configured number of articles for pagination
    let fetch_limit = default_limit;
    let mut resp = crate::news_digest::run_discover_snapshot(
        &q,
        &category,
        fetch_limit,
        if language.is_empty() { None } else { Some(&language) },
        if country.is_empty() { None } else { Some(&country) },
        &ctx.settings(),
        &ctx.rt,
        refresh,
    )
    .await;

    // Apply pagination
    let total = resp.articles.len();
    if offset > 0 || limit < total {
        resp.articles = resp.articles.into_iter().skip(offset).take(limit).collect();
    }

    // Add pagination info
    let mut json = serde_json::to_value(&resp).unwrap_or_else(|_| serde_json::json!({"articles":[]}));
    if let Some(obj) = json.as_object_mut() {
        obj.insert("offset".into(), serde_json::json!(offset));
        obj.insert("limit".into(), serde_json::json!(limit));
        obj.insert("total".into(), serde_json::json!(total));
        obj.insert("has_more".into(), serde_json::json!(offset + resp.articles.len() < total));
    }

    Response::json(json.to_string())
}

fn category_to_seed_query(category: &str, lang: &str) -> String {
    static LOCALES: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
    let locales = LOCALES.get_or_init(|| {
        std::fs::read_to_string("locales/categories.json")
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| serde_json::json!({}))
    });
    let lang_key = lang.split('-').next().unwrap_or("en");
    let fallback = if lang_key == "ko" { "뉴스" } else { "news" };
    locales
        .get(lang_key)
        .and_then(|l| l.get(category).or_else(|| l.get("default")))
        .and_then(|v| v.as_str())
        .unwrap_or(fallback)
        .to_string()
}

/// `GET /api/v1/global_feed` — Not available in standalone mode.
async fn global_feed_proxy(_query: &str, _ctx: &Ctx) -> Response {
    Response::json(r#"{"articles":[],"standalone":true}"#.into())
}

/// `GET /api/v1/feed_recommend` — Not available in standalone mode.
async fn feed_recommend_proxy(_query: &str, _ctx: &Ctx) -> Response {
    Response::json(r#"{"recommendations":[],"standalone":true}"#.into())
}

/// `GET /api/v1/trending?geo=KR` — Real-time Google Trends RSS.
async fn trending_json(query: &str, ctx: &Ctx) -> Response {
    use std::sync::OnceLock;
    use std::time::SystemTime;

    static CACHE: OnceLock<RwLock<HashMap<String, (SystemTime, String)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| RwLock::new(HashMap::new()));

    let geo = {
        let g = param(query, "geo");
        if g.is_empty() { "US".to_string() } else { g.to_uppercase() }
    };

    let cache_key = geo.clone();
    let cache_ttl = std::time::Duration::from_secs(300); // 5 min cache
    if let Ok(c) = cache.read() {
        if let Some((ts, json)) = c.get(&cache_key) {
            if ts.elapsed().unwrap_or_default() < cache_ttl {
                return Response::json(json.clone());
            }
        }
    }

    // Use Google Trends RSS (standalone mode)
    let url = format!("https://trends.google.com/trending/rss?geo={}", geo);
    let result = match ctx.rt.client.get(&url).timeout(std::time::Duration::from_secs(10)).send().await {
        Ok(resp) => resp.text().await.unwrap_or_default(),
        Err(e) => return Response::json(format!(r#"{{"error":"fetch failed: {}","trends":[]}}"#, e)),
    };

    let trends = parse_trends_rss(&result);
    let json = serde_json::to_string(&serde_json::json!({ "geo": geo, "trends": trends }))
        .unwrap_or_else(|_| r#"{"trends":[]}"#.into());

    if let Ok(mut c) = cache.write() {
        c.insert(cache_key, (SystemTime::now(), json.clone()));
    }
    Response::json(json)
}

fn parse_trends_rss(xml: &str) -> Vec<serde_json::Value> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut trends = Vec::new();
    let mut in_item = false;
    let mut cur_tag = String::new();
    let mut title = String::new();
    let mut traffic = String::new();
    let mut picture = String::new();
    let mut news_title = String::new();
    let mut news_url = String::new();
    let mut rank = 0usize;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if tag == "item" {
                    in_item = true;
                    rank += 1;
                    title.clear();
                    traffic.clear();
                    picture.clear();
                    news_title.clear();
                    news_url.clear();
                }
                cur_tag = tag;
            }
            Ok(Event::Text(t)) => {
                if in_item {
                    let text = t.decode().unwrap_or_default().to_string();
                    match cur_tag.as_str() {
                        "title" if title.is_empty() => title = text,
                        "ht:approx_traffic" => traffic = text,
                        "ht:picture" if picture.is_empty() => picture = text,
                        "ht:news_item_title" if news_title.is_empty() => news_title = text,
                        "ht:news_item_url" if news_url.is_empty() => news_url = text,
                        _ => {}
                    }
                }
            }
            Ok(Event::End(e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if tag == "item" && in_item {
                    in_item = false;
                    if !title.is_empty() && rank <= 20 {
                        trends.push(serde_json::json!({
                            "rank": rank,
                            "term": title,
                            "traffic": traffic,
                            "picture": picture,
                            "news_title": news_title,
                            "news_url": news_url,
                        }));
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    trends
}

/// `GET /api/v1/telegram` — Telegram feed not available in standalone mode.
async fn telegram_json(_query: &str, _ctx: &Ctx) -> Response {
    Response::json(r#"{"error":"telegram feed not available in standalone mode","posts":[]}"#.to_string())
}

/// `GET /api/v1/standalone_feed?lang=ko&category=general&limit=20` — Built-in RSS feed (standalone mode).
async fn standalone_feed_json(query: &str, ctx: &Ctx) -> Response {
    let settings = ctx.settings();
    let cache = get_global_feed_cache(&settings);

    let lang = {
        let l = param(query, "lang");
        if l.is_empty() { "en".to_string() } else { l }
    };
    let category = {
        let c = param(query, "category");
        if c.is_empty() { None } else { Some(c) }
    };
    let limit = param(query, "limit").parse::<usize>().unwrap_or(20).min(100);

    let items = cache.get_items(&lang, category.as_deref()).await;
    let results: Vec<serde_json::Value> = items.iter()
        .take(limit)
        .map(|item| item.to_search_result(&item.source))
        .collect();

    let json = serde_json::json!({
        "lang": lang,
        "category": category,
        "count": results.len(),
        "results": results,
    });
    Response::json(serde_json::to_string(&json).unwrap_or_else(|_| r#"{"results":[]}"#.into()))
}

/// `GET /api/v1/feed_pool` — List all available feeds for configuration.
async fn feed_pool_json(ctx: &Ctx) -> Response {
    use crate::feeds::FeedsConfig;

    let config = match FeedsConfig::load_embedded() {
        Ok(c) => c,
        Err(e) => return Response::json(format!(r#"{{"error":"{}"}}"#, e)),
    };

    let settings = ctx.settings();
    let disabled: std::collections::HashSet<_> = settings.feeds.disabled_feeds.iter().collect();

    let mut languages: Vec<serde_json::Value> = config.feeds.iter()
        .map(|(code, lf)| {
            let sources: Vec<serde_json::Value> = lf.sources.iter()
                .map(|s| serde_json::json!({
                    "name": s.name,
                    "url": s.url,
                    "category": s.category,
                    "enabled": !disabled.contains(&s.url),
                }))
                .collect();
            serde_json::json!({
                "code": code,
                "name": lf.name,
                "sources": sources,
            })
        })
        .collect();
    languages.sort_by(|a, b| {
        a["code"].as_str().unwrap_or("").cmp(b["code"].as_str().unwrap_or(""))
    });

    let json = serde_json::json!({
        "version": config.meta.version,
        "total_languages": config.feeds.len(),
        "languages": languages,
    });
    Response::json(serde_json::to_string(&json).unwrap_or_else(|_| r#"{"languages":[]}"#.into()))
}

/// `GET /api/v1/feed_manager/stats` — Feed manager statistics.
async fn feed_manager_stats_json(_ctx: &Ctx) -> Response {
    use std::sync::OnceLock;

    static MANAGER: OnceLock<crate::feeds::FeedManager> = OnceLock::new();

    let manager = MANAGER.get_or_init(|| {
        crate::feeds::FeedManager::new(".metasearch-cache/feeds.db")
            .expect("Failed to init FeedManager")
    });

    let stats = manager.stats();
    let json = serde_json::json!({
        "total_feeds": stats.total_feeds,
        "active_feeds": stats.active_feeds,
        "total_documents": stats.total_documents,
    });
    Response::json(serde_json::to_string(&json).unwrap_or_else(|_| r#"{}"#.into()))
}

/// Simple cache for trending results (5 minute TTL)
static TRENDING_CACHE: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, String)>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// `GET /api/v1/local_trending?lang=ko&limit=20` — Local entity trending via burst detection.
async fn local_trending_json(query: &str, _ctx: &Ctx) -> Response {
    use std::collections::HashMap;

    let lang = param(query, "lang");
    let lang = if lang.is_empty() { "ko" } else { lang.as_str() };
    let limit = param(query, "limit").parse::<usize>().unwrap_or(20).min(50);
    let cache_key = format!("{}:{}", lang, limit);

    // Check cache (5 minute TTL)
    if let Ok(cache) = TRENDING_CACHE.lock() {
        if let Some((ts, json)) = cache.get(&cache_key) {
            if ts.elapsed() < std::time::Duration::from_secs(300) {
                return Response::json(json.clone());
            }
        }
    }

    // Get feed cache
    let cache = match crate::engines::local_feeds::get_feed_cache_public() {
        Some(c) => c,
        None => return Response::json(r#"{"trends":[],"count":0}"#.into()),
    };

    // Get more articles to cover today + yesterday
    let all_items = cache.get_items_from_db(lang, None, 10000).await;
    if all_items.is_empty() {
        return Response::json(r#"{"trends":[],"count":0}"#.into());
    }

    // Split into today (last 24h) vs yesterday (24-48h ago)
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let day_ago = now - 86400;
    let two_days_ago = now - 172800;

    let current_items: Vec<_> = all_items.iter()
        .filter(|item| item.published.unwrap_or(0) >= day_ago)
        .cloned()
        .collect();
    let previous_items: Vec<_> = all_items.iter()
        .filter(|item| {
            let pub_ts = item.published.unwrap_or(0);
            pub_ts >= two_days_ago && pub_ts < day_ago
        })
        .cloned()
        .collect();

    if current_items.is_empty() {
        return Response::json(r#"{"trends":[],"count":0}"#.into());
    }

    // Batch extract entities via TokMor API
    async fn extract_entities_batch(texts: &[String], lang: &str) -> Vec<Vec<String>> {
        let client = reqwest::Client::new();
        let payload = serde_json::json!({"texts": texts, "lang": lang});
        match client
            .post("http://127.0.0.1:8892/extract_batch")
            .json(&payload)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
        {
            Ok(resp) => {
                if let Ok(data) = resp.json::<serde_json::Value>().await {
                    data["results"]
                        .as_array()
                        .map(|arr| {
                            arr.iter()
                                .map(|item| {
                                    // Each item is directly an array of entity strings
                                    item.as_array()
                                        .map(|ents| {
                                            ents.iter()
                                                .filter_map(|e| e.as_str().map(|s| s.to_string()))
                                                .collect()
                                        })
                                        .unwrap_or_default()
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                } else {
                    Vec::new()
                }
            }
            Err(_) => Vec::new(),
        }
    }

    // Prepare texts for batch extraction
    let current_texts: Vec<String> = current_items
        .iter()
        .map(|item| format!("{} {}", item.title, item.description))
        .collect();
    let previous_texts: Vec<String> = previous_items
        .iter()
        .skip(500)
        .map(|item| format!("{} {}", item.title, item.description))
        .collect();

    // Batch extract entities
    let current_entities = extract_entities_batch(&current_texts, lang).await;
    let previous_entities = extract_entities_batch(&previous_texts, lang).await;

    // Count entities in current window
    let mut current_counts: HashMap<String, (u32, Vec<String>)> = HashMap::new();
    for (i, entities) in current_entities.iter().enumerate() {
        let title = &current_items[i].title;
        for entity in entities {
            let entry = current_counts.entry(entity.clone()).or_insert((0, Vec::new()));
            entry.0 += 1;
            if entry.1.len() < 3 {
                entry.1.push(title.clone());
            }
        }
    }

    // Count entities in previous window
    let mut previous_counts: HashMap<String, u32> = HashMap::new();
    for entities in &previous_entities {
        for entity in entities {
            *previous_counts.entry(entity.clone()).or_insert(0) += 1;
        }
    }

    // Calculate burst score: entities that appear MORE in current vs previous
    let mut bursts: Vec<(String, u32, u32, f64, Vec<String>)> = Vec::new();

    // Media outlet names and common article prefixes to exclude from trending
    const MEDIA_BLOCKLIST: &[&str] = &[
        "연합뉴스", "YonhapnewsTV", "연합뉴스TV", "MBC", "KBS", "SBS", "JTBC", "TV조선",
        "MBN", "채널A", "YTN", "뉴스1", "뉴시스", "아시아경제", "한국경제", "매일경제",
        "조선일보", "중앙일보", "동아일보", "한겨레", "경향신문", "국민일보", "서울신문",
        "문화일보", "세계일보", "헤럴드경제", "이데일리", "머니투데이", "파이낸셜뉴스",
        "노컷뉴스", "오마이뉴스", "프레시안", "미디어오늘", "PD저널", "기자협회보",
        "Reuters", "AP", "AFP", "Bloomberg", "CNN", "BBC", "NHK", "Xinhua",
        "뉴스", "NEWS", "News", "TV", "라디오", "신문", "일보", "경제",
        "OSEN", "osen", "오센", "스포탈코리아", "게티이미지", "게티", "AFP",
        // Common article title prefixes (not real topics)
        "사진", "영상", "동영상", "포토", "화보", "인터뷰", "속보", "단독", "긴급",
        "종합", "업데이트", "수정", "정정", "breaking", "photo", "video", "update",
        // Common verbs/nouns that aren't topics
        "경기", "상대", "결과", "오늘", "내일", "어제",
        // Korean administrative divisions (always in news, not trending)
        "서울", "부산", "대구", "인천", "광주", "대전", "울산", "세종",
        "경기", "강원", "충북", "충남", "전북", "전남", "경북", "경남", "제주",
        "서울시", "부산시", "대구시", "인천시", "광주시", "대전시", "울산시", "세종시",
        "경기도", "강원도", "충청북도", "충청남도", "전라북도", "전라남도", "경상북도", "경상남도", "제주도",
        "수원", "성남", "고양", "용인", "창원", "청주", "천안", "전주", "포항", "김해",
        "수원시", "성남시", "고양시", "용인시", "창원시", "청주시", "천안시", "전주시", "포항시", "김해시",
        "진주", "진주시", "사천", "사천시", "국도", "한국", "대한민국", "Korea",
        // Common time/period words
        "주간", "월간", "연간", "오전", "오후", "새벽", "저녁", "아침",
        // Small cities that appear frequently
        "창녕", "홍성", "예산", "당진", "서산", "태안", "보령", "공주", "논산",
    ];

    for (entity, (current, samples)) in current_counts {
        let previous = previous_counts.get(&entity).copied().unwrap_or(0);

        // Skip media outlet names
        if MEDIA_BLOCKLIST.iter().any(|&m| entity.eq_ignore_ascii_case(m) || entity.contains(m)) {
            continue;
        }

        // Skip dates and times (숫자+일/월/년/시/분, 2026, etc.)
        let is_date_or_time = entity.chars().any(|c| c.is_ascii_digit())
            && (entity.ends_with('일') || entity.ends_with('월') || entity.ends_with('년')
                || entity.ends_with('시') || entity.ends_with('분') || entity.ends_with('초')
                || entity.ends_with("日") || entity.ends_with("月") || entity.ends_with("年")
                || entity.starts_with("202") || entity.starts_with("201")
                || entity.contains("시 ") || entity.contains(":"));
        if is_date_or_time {
            continue;
        }

        // Skip if not enough mentions
        if current < 3 {
            continue;
        }

        // Calculate burst score - prioritize NEW entities (wasn't there, now appears)
        let burst_score = if previous == 0 && current >= 5 {
            // NEW: didn't exist yesterday, appears today → highest priority
            current as f64 * 10.0
        } else if previous <= 2 && current >= 10 {
            // EMERGING: barely existed yesterday, now significant
            current as f64 * 5.0
        } else if previous > 0 {
            // EXISTING: check growth ratio (must be 3x+ to be trending)
            let growth = current as f64 / previous as f64;
            if growth >= 3.0 && current >= 10 {
                (current - previous) as f64 * growth
            } else {
                // Not enough growth - skip
                0.0
            }
        } else {
            0.0
        };

        if burst_score > 0.0 {
            bursts.push((entity, current, previous, burst_score, samples));
        }
    }

    // Sort by burst score (descending)
    bursts.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));

    let trends: Vec<serde_json::Value> = bursts.into_iter()
        .take(limit)
        .enumerate()
        .map(|(i, (entity, current, previous, score, samples))| {
            serde_json::json!({
                "rank": i + 1,
                "entity": entity,
                "type": "unknown",
                "count": current,
                "previous": previous,
                "score": (score * 10.0).round() / 10.0,
                "samples": samples,
            })
        })
        .collect();

    let json = serde_json::json!({
        "trends": trends,
        "count": trends.len(),
    });
    let json_str = serde_json::to_string(&json).unwrap_or_else(|_| r#"{"trends":[]}"#.into());

    // Store in cache
    if let Ok(mut cache) = TRENDING_CACHE.lock() {
        cache.insert(cache_key, (std::time::Instant::now(), json_str.clone()));
    }

    Response::json(json_str)
}

/// `POST /api/v1/translate` — Translate a query to target language using AI.
async fn translate_query_json(body: &str, ctx: &Ctx) -> Response {
    #[derive(serde::Deserialize)]
    struct Req {
        text: String,
        target: String,
    }
    let req: Req = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(_) => return Response::json(r#"{"error":"invalid request"}"#.into()),
    };

    let settings = ctx.settings();
    if !settings.ai.enabled {
        return Response::json(format!(r#"{{"translated":"{}"}}"#, escape(&req.text)));
    }

    let lang_name = match req.target.as_str() {
        "ko" => "Korean",
        "ja" => "Japanese",
        "zh" => "Chinese",
        "de" => "German",
        "fr" => "French",
        "es" => "Spanish",
        "pt" => "Portuguese",
        "ru" => "Russian",
        "ar" => "Arabic",
        _ => return Response::json(format!(r#"{{"translated":"{}"}}"#, escape(&req.text))),
    };

    // Call Ollama directly for translation
    let prompt = format!(
        "Translate to {}: \"{}\"\nReturn ONLY the translation, no explanation.",
        lang_name, req.text
    );
    let ollama_url = format!("{}/api/generate", settings.ai.base_url);
    let ollama_body = serde_json::json!({
        "model": &settings.ai.model,
        "prompt": prompt,
        "stream": false,
        "options": { "temperature": 0.1 }
    });

    match ctx.rt.client.post(&ollama_url)
        .json(&ollama_body)
        .timeout(std::time::Duration::from_secs(15))
        .send().await
    {
        Ok(resp) if resp.status().is_success() => {
            if let Ok(data) = resp.json::<serde_json::Value>().await {
                let translated = data["response"].as_str().unwrap_or(&req.text)
                    .trim().trim_matches('"').to_string();
                return Response::json(format!(
                    r#"{{"translated":"{}","original":"{}"}}"#,
                    escape(&translated), escape(&req.text)
                ));
            }
        }
        _ => {}
    }
    Response::json(format!(r#"{{"translated":"{}"}}"#, escape(&req.text)))
}

/// `GET /api/v1/briefing?lang=ko&type=global&categories=ai,tech&sources=news` — Proxy to briefing service
async fn briefing_proxy(query: &str) -> Response {
    let lang = param(query, "lang");
    let lang = if lang.is_empty() { "en" } else { &lang };
    let category = param(query, "category");
    let briefing_type = param(query, "type");
    let categories = param(query, "categories");
    let sources = param(query, "sources");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(90))
        .build()
        .unwrap_or_default();

    let mut url = format!("http://127.0.0.1:8893/briefing?lang={}", lang);
    if !category.is_empty() {
        url.push_str(&format!("&category={}", category));
    }
    if !briefing_type.is_empty() {
        url.push_str(&format!("&type={}", briefing_type));
    }
    if !categories.is_empty() {
        url.push_str(&format!("&categories={}", categories));
    }
    if !sources.is_empty() {
        url.push_str(&format!("&sources={}", sources));
    }

    match client
        .get(&url)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let body = resp.text().await.unwrap_or_default();
            Response::json(body)
        }
        Ok(resp) => Response::json(format!(
            r#"{{"error":"briefing service returned {}"}}"#,
            resp.status()
        )),
        Err(e) => Response::json(format!(
            r#"{{"error":"briefing service unavailable: {}"}}"#,
            e
        )),
    }
}

/// `GET /api/v1/briefing/audio?id=xxx` — Proxy audio file from briefing service
async fn briefing_audio_proxy(query: &str) -> Response {
    let id = param(query, "id");
    if id.is_empty() {
        return Response::json(r#"{"error":"missing id"}"#.to_string());
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_default();

    match client
        .get(format!("http://127.0.0.1:8893/audio/{}.mp3", id))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let bytes = resp.bytes().await.unwrap_or_default();
            Response::bytes(bytes.to_vec(), "audio/mpeg")
        }
        _ => Response::not_found(),
    }
}

/// Local radio stations database path
const RADIO_DB: &str = "static/radio_stations.db";

/// Query local radio database and return JSON (with favicon, bitrate, codec)
fn radio_db_query(sql: &str) -> Response {
    match rusqlite::Connection::open(RADIO_DB) {
        Ok(conn) => {
            match conn.prepare(sql) {
                Ok(mut stmt) => {
                    let rows: Vec<serde_json::Value> = stmt
                        .query_map([], |row| {
                            Ok(serde_json::json!({
                                "name": row.get::<_, String>(0).unwrap_or_default(),
                                "country": row.get::<_, String>(1).unwrap_or_default(),
                                "tags": row.get::<_, String>(2).unwrap_or_default(),
                                "url": row.get::<_, String>(3).unwrap_or_default(),
                                "favicon": row.get::<_, String>(4).unwrap_or_default(),
                                "bitrate": row.get::<_, i32>(5).unwrap_or(0),
                                "codec": row.get::<_, String>(6).unwrap_or_default(),
                            }))
                        })
                        .map(|r| r.filter_map(|x| x.ok()).collect())
                        .unwrap_or_default();
                    Response::json(serde_json::to_string(&rows).unwrap_or_else(|_| "[]".into()))
                }
                Err(e) => Response::json(format!(r#"{{"error":"sql: {}"}}"#, e)),
            }
        }
        Err(e) => Response::json(format!(r#"{{"error":"db: {}"}}"#, e)),
    }
}

/// `GET /api/v1/radio/search?q=jazz&limit=50` — Search stations
fn radio_search(query: &str) -> Response {
    let q = param(query, "q").replace('\'', "''");
    let limit: u32 = param(query, "limit").parse().unwrap_or(100);
    if q.is_empty() {
        return Response::json(r#"{"error":"missing q"}"#.into());
    }
    radio_db_query(&format!(
        "SELECT name, country, tags, COALESCE(url_resolved, url), favicon, bitrate, codec \
         FROM stations WHERE is_alive=1 AND (name LIKE '%{q}%' OR tags LIKE '%{q}%') \
         ORDER BY votes DESC LIMIT {limit}"
    ))
}

/// `GET /api/v1/radio/recommend?limit=50` — Popular stations (prefer MP3/AAC over HLS)
fn radio_recommend(query: &str) -> Response {
    let limit: u32 = param(query, "limit").parse().unwrap_or(100);
    radio_db_query(&format!(
        "SELECT name, country, tags, COALESCE(url_resolved, url), favicon, MAX(bitrate) as bitrate, codec \
         FROM stations WHERE is_alive=1 AND bitrate >= 96 \
         GROUP BY name ORDER BY (CASE WHEN url LIKE '%.m3u8' OR url_resolved LIKE '%.m3u8' THEN 1 ELSE 0 END), votes DESC, clickcount DESC LIMIT {limit}"
    ))
}

/// `GET /api/v1/radio/genre?genre=jazz&limit=50` — Stations by genre (prefer MP3/AAC over HLS)
fn radio_by_genre(query: &str) -> Response {
    let genre = param(query, "genre").replace('\'', "''");
    let limit: u32 = param(query, "limit").parse().unwrap_or(100);
    if genre.is_empty() {
        return Response::json(r#"{"error":"missing genre"}"#.into());
    }
    radio_db_query(&format!(
        "SELECT name, country, tags, COALESCE(url_resolved, url), favicon, MAX(bitrate) as bitrate, codec \
         FROM stations WHERE is_alive=1 AND bitrate >= 96 AND tags LIKE '%{genre}%' \
         GROUP BY name ORDER BY (CASE WHEN url LIKE '%.m3u8' OR url_resolved LIKE '%.m3u8' THEN 1 ELSE 0 END), votes DESC, clickcount DESC LIMIT {limit}"
    ))
}

/// `GET /api/v1/radio/country?code=KR&tag=pop&limit=50` — Stations by country (prefer MP3/AAC over HLS)
fn radio_by_country(query: &str) -> Response {
    let code = param(query, "code").replace('\'', "''");
    let tag = param(query, "tag").replace('\'', "''");
    let limit: u32 = param(query, "limit").parse().unwrap_or(100);
    if code.is_empty() {
        return Response::json(r#"{"error":"missing code"}"#.into());
    }
    let tag_filter = if tag.is_empty() { String::new() } else { format!(" AND tags LIKE '%{tag}%'") };
    radio_db_query(&format!(
        "SELECT name, country, tags, COALESCE(url_resolved, url), favicon, MAX(bitrate) as bitrate, codec \
         FROM stations WHERE is_alive=1 AND bitrate >= 96 AND countrycode='{code}'{tag_filter} \
         GROUP BY name ORDER BY (CASE WHEN url LIKE '%.m3u8' OR url_resolved LIKE '%.m3u8' THEN 1 ELSE 0 END), votes DESC, clickcount DESC LIMIT {limit}"
    ))
}

/// `GET /api/v1/radio/stream?url=...` — Proxy radio stream to bypass CORS
async fn radio_stream_proxy(query: &str) -> Response {
    let url = param(query, "url");
    if url.is_empty() {
        return Response::json(r#"{"error":"missing url"}"#.to_string());
    }

    let decoded_url = url.replace("%3A", ":").replace("%2F", "/").replace("%3F", "?").replace("%3D", "=").replace("%26", "&");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_default();

    match client
        .get(&decoded_url)
        .header("User-Agent", "Mozilla/5.0 (compatible; Metasearch/1.0)")
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let content_type = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("audio/mpeg")
                .to_string();

            let bytes = resp.bytes().await.unwrap_or_default();
            Response::bytes(bytes.to_vec(), &content_type)
        }
        Ok(resp) => Response::json(format!(
            r#"{{"error":"stream returned {}"}}"#,
            resp.status()
        )),
        Err(e) => Response::json(format!(
            r#"{{"error":"stream unavailable: {}"}}"#,
            e
        )),
    }
}

/// `GET /api/v1/lens?lens=conservative&title=...&content=...` — Analyze article from different perspective
async fn lens_analyze_json(query: &str, ctx: &Ctx) -> Response {
    let lens = param(query, "lens");
    let title = param(query, "title");
    let content = param(query, "content");

    if title.is_empty() && content.is_empty() {
        return Response::json(r#"{"error":"title or content required"}"#.into());
    }

    let lens_prompt = match lens.as_str() {
        "conservative" => "Interpret this from a conservative perspective, focusing on tradition, stability, and cautious change.",
        "progressive" => "Interpret this from a progressive perspective, focusing on social change, equality, and reform.",
        "philosopher" => "Analyze this philosophically, exploring deeper meanings, ethics, and existential implications.",
        "critical" => "Critically examine this article, identifying potential biases, missing context, and questionable claims.",
        "casual" | "simple" => "Explain this simply in plain language that anyone can understand, avoiding jargon.",
        _ => "Analyze this article objectively, highlighting key facts and implications.",
    };

    let prompt = format!(
        "{}\n\nArticle: \"{}\"\n{}\n\nRespond in 2-3 concise paragraphs in the same language as the article.",
        lens_prompt,
        title,
        &content.chars().take(2000).collect::<String>()
    );

    let settings = ctx.settings();
    let api_url = format!("{}/chat/completions", settings.ai.base_url);
    let api_body = serde_json::json!({
        "model": &settings.ai.model,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": 500
    });

    let api_key = settings.ai.api_key.clone().unwrap_or_default();

    match ctx.rt.client.post(&api_url)
        .header("Authorization", format!("Bearer {}", api_key))
        .json(&api_body)
        .timeout(std::time::Duration::from_secs(60))
        .send().await
    {
        Ok(resp) if resp.status().is_success() => {
            if let Ok(data) = resp.json::<serde_json::Value>().await {
                let analysis = data["choices"][0]["message"]["content"]
                    .as_str().unwrap_or("").to_string();
                let input_tokens = data["usage"]["prompt_tokens"].as_u64().unwrap_or(0);
                let output_tokens = data["usage"]["completion_tokens"].as_u64().unwrap_or(0);
                let json_resp = serde_json::json!({
                    "analysis": analysis,
                    "usage": {
                        "input": input_tokens,
                        "output": output_tokens
                    }
                });
                return Response::json(json_resp.to_string());
            }
            Response::json(r#"{"error":"invalid response"}"#.into())
        }
        Ok(resp) => {
            let status = resp.status();
            Response::json(format!(r#"{{"error":"API returned {}"}}"#, status))
        }
        Err(e) => {
            Response::json(format!(r#"{{"error":"{}"}}"#, e))
        }
    }
}

/// `GET /api/v1/followup?q=&context=` — Answer followup question about article
async fn followup_answer_json(query: &str, ctx: &Ctx) -> Response {
    let q = param(query, "q");
    let context = param(query, "context");

    if q.is_empty() {
        return Response::json(r#"{"error":"question required"}"#.into());
    }

    let prompt = format!(
        "Based on this article context, answer the following question concisely.\n\n\
        Article context:\n{}\n\n\
        Question: {}\n\n\
        Answer in the same language as the question. Be concise (2-3 paragraphs max).",
        &context.chars().take(3000).collect::<String>(),
        q
    );

    let settings = ctx.settings();
    let api_url = format!("{}/chat/completions", settings.ai.base_url);
    let api_body = serde_json::json!({
        "model": &settings.ai.model,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": 500
    });

    let api_key = settings.ai.api_key.clone().unwrap_or_default();

    match ctx.rt.client.post(&api_url)
        .header("Authorization", format!("Bearer {}", api_key))
        .json(&api_body)
        .timeout(std::time::Duration::from_secs(60))
        .send().await
    {
        Ok(resp) if resp.status().is_success() => {
            if let Ok(data) = resp.json::<serde_json::Value>().await {
                let answer = data["choices"][0]["message"]["content"]
                    .as_str().unwrap_or("").to_string();
                let input_tokens = data["usage"]["prompt_tokens"].as_u64().unwrap_or(0);
                let output_tokens = data["usage"]["completion_tokens"].as_u64().unwrap_or(0);
                let json_resp = serde_json::json!({
                    "answer": answer,
                    "usage": {
                        "input": input_tokens,
                        "output": output_tokens
                    }
                });
                return Response::json(json_resp.to_string());
            }
            Response::json(r#"{"error":"invalid response"}"#.into())
        }
        Ok(resp) => {
            let status = resp.status();
            Response::json(format!(r#"{{"error":"API returned {}"}}"#, status))
        }
        Err(e) => {
            Response::json(format!(r#"{{"error":"{}"}}"#, e))
        }
    }
}

/// `POST /api/v1/feed_subscribe` — Not available in standalone mode.
async fn feed_subscribe_proxy(_body: &str, _ctx: &Ctx) -> Response {
    Response::json(r#"{"error":"feed subscription not available in standalone mode","standalone":true}"#.into())
}

/// `GET /api/v1/news_digest?q=&limit=5&refresh=1` — Discover feed (search + teasers, no AI).
async fn news_digest_json(query: &str, ctx: &Ctx) -> Response {
    let q = param(query, "q");
    let limit = param(query, "limit").parse::<usize>().unwrap_or(5);
    let refresh = matches!(
        param(query, "refresh").as_str(),
        "1" | "true" | "yes" | "on"
    );
    let resp =
        crate::news_digest::run_news_digest(&q, limit, &ctx.settings(), &ctx.rt, refresh).await;
    Response::json(serde_json::to_string(&resp).unwrap_or_else(|_| "{\"articles\":[]}".into()))
}

/// `GET|POST /api/v1/news_images` — cached async image hydration for news cards.
async fn news_images_json(method: &str, query: &str, body: &str, ctx: &Ctx) -> Response {
    let refresh = matches!(
        param(query, "refresh").as_str(),
        "1" | "true" | "yes" | "on"
    );
    let req = if method == "POST" && body.trim_start().starts_with('{') {
        match serde_json::from_str::<crate::news_digest::NewsImagesRequest>(body) {
            Ok(mut req) => {
                if req.limit == 0 {
                    req.limit = param(query, "limit").parse::<usize>().unwrap_or(30);
                }
                if req.query.trim().is_empty() {
                    req.query = param(query, "q");
                }
                req
            }
            Err(e) => {
                return Response::json(
                    serde_json::json!({"error": format!("invalid json: {e}")}).to_string(),
                );
            }
        }
    } else {
        crate::news_digest::NewsImagesRequest {
            query: param(query, "q"),
            limit: param(query, "limit").parse::<usize>().unwrap_or(30),
            articles: Vec::new(),
        }
    };
    let resp = crate::news_digest::run_news_images(req, &ctx.settings(), &ctx.rt, refresh).await;
    Response::json(serde_json::to_string(&resp).unwrap_or_else(|_| "{}".into()))
}

/// `GET /api/v1/news_article?url=&title=&engine=` — full-page rewrite (JSON).
async fn news_article_json(query: &str, ctx: &Ctx) -> Response {
    let url = param(query, "url");
    if url.trim().is_empty() {
        return Response::json(serde_json::json!({"error": "missing url parameter"}).to_string());
    }
    let title = param(query, "title");
    let engine = param(query, "engine");
    let publisher_url = param(query, "publisher_url");
    let model = {
        let m = param(query, "model");
        if m.is_empty() {
            None
        } else {
            Some(m)
        }
    };
    match crate::news_article::rewrite_news_article(
        &url,
        &title,
        &engine,
        &publisher_url,
        &ctx.settings(),
        &ctx.rt,
        model.as_deref(),
    )
    .await
    {
        Ok(resp) => Response::json(serde_json::to_string(&resp).unwrap_or_else(|_| "{}".into())),
        Err(e) => Response::json(serde_json::json!({"error": e}).to_string()),
    }
}

/// SSE stream for `GET /api/v1/news_article?url=&stream=1`.
///
/// Event order: phase events (`resolving`/`fetching`) → `extracted` →
/// `rewriting` → `token`* → `sections_done` → `media` → `done`. A warm rewrite
/// may instead emit `cached` → `done`.
async fn stream_news_article_sse(
    stream: &mut TcpStream,
    query: &str,
    ctx: &Ctx,
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 200 OK\r\n\
        Content-Type: text/event-stream; charset=utf-8\r\n\
        Cache-Control: no-store\r\n\
        Access-Control-Allow-Origin: *\r\n\
        {}\
        X-Accel-Buffering: no\r\n\
        Connection: close\r\n\r\n",
        SECURITY_HEADERS
    );
    stream.write_all(head.as_bytes()).await?;
    stream.flush().await?;

    let url = param(query, "url");
    if url.trim().is_empty() {
        sse_write(
            stream,
            "error",
            &serde_json::json!({"message": "missing url"}).to_string(),
        )
        .await?;
        return sse_write(stream, "done", "{}").await;
    }
    let title = param(query, "title");
    let engine = param(query, "engine");
    let publisher_url = param(query, "publisher_url");
    let lang_override = param(query, "lang");
    let model_override = {
        let m = param(query, "model");
        if m.is_empty() { None } else { Some(m) }
    };
    let perspective = {
        let p = param(query, "perspective");
        if p.is_empty() { None } else { Some(crate::ai::ArticlePerspective::from_str(&p)) }
    };

    let settings = ctx.settings();
    let model_key =
        crate::news_article::effective_article_model_name(&settings, model_override.as_deref());
    if let Some(cached) = ctx.rt.article_rewrite_cache.get(&url, &title, &model_key) {
        sse_write(
            stream,
            "cached",
            &serde_json::to_string(&cached).unwrap_or_else(|_| "{}".into()),
        )
        .await?;
        return sse_write(
            stream,
            "done",
            &serde_json::json!({"usage": cached.usage}).to_string(),
        )
        .await;
    }

    if crate::googlenews_decode::is_google_news_article_url(&url) {
        sse_write(
            stream,
            "resolving",
            &serde_json::json!({"url": url.as_str(), "message": "Resolving Google News link"})
                .to_string(),
        )
        .await?;
    }
    sse_write(
        stream,
        "fetching",
        &serde_json::json!({"url": url.as_str(), "message": "Fetching publisher article"})
            .to_string(),
    )
    .await?;

    let body = crate::news_article::fetch_article_for_rewrite(
        &ctx.rt.article_cache,
        &url,
        &title,
        &publisher_url,
        &settings,
        &ctx.rt,
    )
    .await;
    // Use extracted title if param is empty
    let title = if title.trim().is_empty() {
        body.title.clone()
    } else {
        title
    };
    if body.error.is_some() || !crate::article::is_usable_article_text(&body.text, &title) {
        let msg = body
            .error
            .unwrap_or_else(|| "could not extract article text".into());
        sse_write(
            stream,
            "error",
            &serde_json::json!({"message": msg}).to_string(),
        )
        .await?;
        return sse_write(stream, "done", "{}").await;
    }
    if let Some(cached) = ctx
        .rt
        .article_rewrite_cache
        .get(&body.url, &title, &model_key)
    {
        sse_write(
            stream,
            "cached",
            &serde_json::to_string(&cached).unwrap_or_else(|_| "{}".into()),
        )
        .await?;
        return sse_write(stream, "done", "{}").await;
    }

    sse_write(
        stream,
        "extracted",
        &serde_json::to_string(&crate::news_article::ExtractedEvent {
            title: title.clone(),
            url: body.url.clone(),
            text_chars: body.text.chars().count(),
        })
        .unwrap_or_else(|_| "{}".into()),
    )
    .await?;

    if !settings.ai.enabled {
        sse_write(
            stream,
            "error",
            &serde_json::json!({"message": "AI disabled"}).to_string(),
        )
        .await?;
        return sse_write(stream, "done", "{}").await;
    }
    sse_write(
        stream,
        "rewriting",
        &serde_json::json!({"model": model_key.as_str(), "message": "Rewriting article"})
            .to_string(),
    )
    .await?;

    // Send source analysis event
    let source_info = crate::article_analysis::lookup_source(&body.url);
    let text_analysis = crate::article_analysis::analyze_text(&body.text, &body.url);
    sse_write(
        stream,
        "analysis",
        &serde_json::json!({
            "source": source_info.as_ref().map(|s| serde_json::json!({
                "name": s.name,
                "bias": s.bias,
                "bias_label": s.bias.map(|b| crate::article_analysis::bias_label(b, "en")),
                "tier": s.tier,
                "tier_label": crate::article_analysis::tier_label(s.tier, "en"),
            })),
            "article_type": format!("{:?}", text_analysis.article_type),
            "factual_score": text_analysis.factual_score,
            "emotional_score": text_analysis.emotional_score,
            "opinion_score": text_analysis.opinion_score,
            "tone": crate::article_analysis::tone_label(&text_analysis, "en"),
        }).to_string(),
    ).await?;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let mut ai = settings.ai.clone();
    if (ai.answer_language.is_empty() || ai.answer_language == "auto") && !lang_override.is_empty() {
        ai.answer_language = lang_override.clone();
    }
    let client = ctx.rt.client.clone();
    let card_title = title.clone();
    let source_url = body.url.clone();
    let source_text = body.text.clone();
    let model = model_key.clone();
    let perspective_clone = perspective;
    let handle = tokio::spawn(async move {
        crate::ai::stream_news_article(
            &ai,
            &client,
            &card_title,
            &source_url,
            &source_text,
            Some(&model),
            perspective_clone,
            |t| {
                let _ = tx.send(t.to_string());
            },
        )
        .await
    });

    let mut article = String::new();
    #[allow(unused_assignments)]
    let mut usage: Option<crate::ai::TokenUsage> = None;
    while let Some(tok) = rx.recv().await {
        article.push_str(&tok);
        sse_write(
            stream,
            "token",
            &serde_json::json!({ "text": tok }).to_string(),
        )
        .await?;
    }

    match handle.await {
        Ok(Ok(result)) => {
            article = result.article;
            usage = result.usage;
        }
        Ok(Err(e)) => {
            sse_write(
                stream,
                "error",
                &serde_json::json!({"message": e}).to_string(),
            )
            .await?;
            return sse_write(stream, "done", "{}").await;
        }
        Err(_) => {
            sse_write(
                stream,
                "error",
                &serde_json::json!({"message": "rewrite task aborted"}).to_string(),
            )
            .await?;
            return sse_write(stream, "done", "{}").await;
        }
    }

    let sections = crate::news_article::parse_sections(&article);
    sse_write(
        stream,
        "sections_done",
        &serde_json::to_string(&crate::news_article::SectionsDoneEvent {
            sections: sections.clone(),
        })
        .unwrap_or_else(|_| "{}".into()),
    )
    .await?;

    let media = crate::news_article::related_media_for(&body, &title, &settings, &ctx.rt).await;
    let source = crate::news_article::build_source(&body, &engine);
    let response = crate::news_article::NewsArticleResponse {
        title: title.clone(),
        url: body.url.clone(),
        engine: engine.clone(),
        article: article.clone(),
        sections: sections.clone(),
        source: source.clone(),
        media: media.clone(),
        usage: usage.clone(),
    };
    ctx.rt
        .article_rewrite_cache
        .put(&[&url, &body.url], &title, &model_key, response);
    sse_write(
        stream,
        "media",
        &serde_json::json!({ "items": media, "source": source }).to_string(),
    )
    .await?;

    sse_write(
        stream,
        "done",
        &serde_json::json!({
            "title": body.title,
            "url": body.url,
            "engine": engine,
            "sections": sections,
            "usage": usage,
        })
        .to_string(),
    )
    .await
}

/// SSE stream for `POST|GET /api/v1/research?stream=true`.
///
/// Event order: `plan`? → `results` → `token`* → `citations` → `followups`? → `done`
async fn stream_research_sse(
    stream: &mut TcpStream,
    req: &crate::api::ResearchRequest,
    ctx: &Ctx,
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 200 OK\r\n\
        Content-Type: text/event-stream; charset=utf-8\r\n\
        Cache-Control: no-store\r\n\
        Access-Control-Allow-Origin: *\r\n\
        {}\
        X-Accel-Buffering: no\r\n\
        Connection: close\r\n\r\n",
        SECURITY_HEADERS
    );
    stream.write_all(head.as_bytes()).await?;
    stream.flush().await?;

    if req.query.trim().is_empty() {
        sse_write(
            stream,
            "error",
            &serde_json::json!({"message": "empty query"}).to_string(),
        )
        .await?;
        return sse_write(stream, "done", "{}").await;
    }

    let started = std::time::Instant::now();
    let focus = crate::ai::FocusMode::parse(req.focus.as_deref().unwrap_or("general"));
    let model_override = req.model.clone();
    let mut params = crate::api::research_to_search_params(req);
    let settings = ctx.settings();

    if params.deep == Some(true) && settings.ai.enabled {
        let planned = crate::ai::plan_subqueries(&settings.ai, &ctx.rt.client, &params.query).await;
        params.deep_subqueries = Some(planned.clone());
        sse_write(
            stream,
            "plan",
            &serde_json::json!({ "subqueries": planned }).to_string(),
        )
        .await?;
    }

    let response = search_all(&params, &settings, &ctx.rt).await;

    sse_write(
        stream,
        "results",
        &serde_json::json!({
            "query": response.query,
            "number_of_results": response.number_of_results,
            "results": response.results,
            "engines_used": crate::api::engines_used(&response),
        })
        .to_string(),
    )
    .await?;

    let citations = crate::ai::build_citations(&response.results, settings.ai.answer_top_n);
    let mut stream_err: Option<String> = None;
    let mut answer = String::new();
    #[allow(unused_assignments)]
    let mut usage: Option<crate::ai::TokenUsage> = None;

    if !settings.ai.enabled {
        stream_err = Some("AI disabled (set ai.enabled / ai.base_url)".to_string());
    } else if response.results.is_empty() {
        stream_err = Some("no results to summarize".to_string());
    } else {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let ai = settings.ai.clone();
        let client = ctx.rt.client.clone();
        let q_eff = response.query.clone();
        let results = response.results.clone();
        let is_deep = params.deep == Some(true);
        let handle = tokio::spawn(async move {
            if is_deep {
                // Deep research: enhanced multi-source analysis
                crate::ai::stream_answer_deep(
                    &ai,
                    &client,
                    &q_eff,
                    &results,
                    focus,
                    model_override.as_deref(),
                    |t| { let _ = tx.send(t.to_string()); },
                ).await
            } else {
                crate::ai::stream_answer_with_options(
                    &ai,
                    &client,
                    &q_eff,
                    &results,
                    focus,
                    model_override.as_deref(),
                    |t| { let _ = tx.send(t.to_string()); },
                ).await
            }
        });
        while let Some(tok) = rx.recv().await {
            answer.push_str(&tok);
            sse_write(
                stream,
                "token",
                &serde_json::json!({ "text": tok }).to_string(),
            )
            .await?;
        }
        match handle.await {
            Ok(Ok(result)) => {
                answer = result.article;
                usage = result.usage;
            }
            Ok(Err(e)) => stream_err = Some(e),
            Err(_) => stream_err = Some("answer task aborted".to_string()),
        }
    }

    let with_footnotes = crate::ai::append_citation_footnotes(&answer, &citations);
    if with_footnotes.len() > answer.len() {
        let extra = with_footnotes[answer.len()..].to_string();
        answer = with_footnotes;
        sse_write(
            stream,
            "token",
            &serde_json::json!({ "text": extra }).to_string(),
        )
        .await?;
    }

    if let Some(msg) = stream_err {
        sse_write(
            stream,
            "error",
            &serde_json::json!({ "message": msg }).to_string(),
        )
        .await?;
    }

    sse_write(
        stream,
        "citations",
        &serde_json::to_string(&citations).unwrap_or_else(|_| "[]".into()),
    )
    .await?;

    if req.followups && settings.ai.enabled {
        let fu = crate::ai::suggest_followups(
            &settings.ai,
            &ctx.rt.client,
            &response.query,
            &response.results,
        )
        .await;
        sse_write(
            stream,
            "followups",
            &serde_json::to_string(&fu).unwrap_or_else(|_| "[]".into()),
        )
        .await?;
    }

    sse_write(
        stream,
        "meta",
        &serde_json::json!({
            "latency_ms": started.elapsed().as_millis(),
            "answer": if answer.is_empty() { serde_json::Value::Null } else { serde_json::Value::String(answer) },
        })
        .to_string(),
    )
    .await?;

    let done_data = if let Some(u) = usage {
        serde_json::json!({
            "usage": {
                "input_tokens": u.input_tokens,
                "output_tokens": u.output_tokens,
                "total_tokens": u.total_tokens,
                "cost_usd": u.cost_usd,
                "model": u.model
            }
        })
    } else {
        serde_json::json!({})
    };
    sse_write(stream, "done", &done_data.to_string()).await
}

// ----------------------------------------------------------------- JSON APIs

fn config_json(settings: &Settings) -> String {
    let engines: Vec<serde_json::Value> = settings
        .engines
        .iter()
        .map(|e| {
            serde_json::json!({
                "name": e.name,
                "enabled": e.enabled,
                "weight": e.weight,
                "categories": e.categories,
            })
        })
        .collect();
    let custom_engines: Vec<serde_json::Value> = settings
        .custom_engines
        .iter()
        .map(|e| {
            serde_json::json!({
                "name": e.name,
                "type": e.kind,
                "enabled": e.enabled,
                "weight": e.weight,
                "categories": e.categories,
            })
        })
        .collect();
    serde_json::json!({
        "instance": "ai-studio-metasearch",
        "formats": settings.search.formats,
        "default_lang": settings.search.default_lang,
        "default_language": settings.search.default_language,
        "safe_search": settings.search.safe_search,
        "categories": settings.categories(),
        "autocomplete": settings.search.autocomplete,
        "engines": engines,
        "custom_engines": custom_engines,
        "ai": {
            "enabled": settings.ai.enabled,
            "answer": settings.ai.answer,
            "expand": settings.ai.expand,
            "rerank": settings.ai.rerank,
            "cluster": settings.ai.cluster,
            "conversational": settings.ai.conversational,
            "vision": settings.ai.vision,
            "model": settings.ai.model,
            "answer_language": settings.ai.answer_language,
            "base_url": settings.ai.base_url,
        },
        "personalization": settings.search.personalization,
    })
    .to_string()
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn stats_json(ctx: &Ctx) -> String {
    let stats = ctx.rt.stats();
    let health = ctx.rt.health_snapshot();
    // Union of engines seen in stats and/or health (a cooled engine always has
    // stats too, but be defensive).
    let mut names: std::collections::BTreeSet<String> = stats.keys().cloned().collect();
    names.extend(health.keys().cloned());
    let cooling_down: Vec<&String> = health
        .iter()
        .filter(|(_, h)| h.cooling_down)
        .map(|(n, _)| n)
        .collect();
    let mut engines: Vec<serde_json::Value> = names
        .iter()
        .map(|name| {
            let s = stats.get(name).cloned().unwrap_or_default();
            let avg = if s.calls > 0 {
                s.total_ms as f64 / s.calls as f64
            } else {
                0.0
            };
            let hi = health.get(name).cloned().unwrap_or_default();
            serde_json::json!({
                "engine": name,
                "calls": s.calls,
                "errors": s.errors,
                "results": s.results,
                "avg_ms": round2(avg),
                "success_rate": round2(s.success_rate()),
                "recent_success_rate": round2(s.recent_success_rate()),
                "recent_avg_ms": round2(s.recent_avg_ms()),
                "recent_latencies_ms": s.recent_ms.iter().copied().collect::<Vec<u128>>(),
                "health": serde_json::to_value(&hi).unwrap_or(serde_json::Value::Null),
            })
        })
        .collect();
    engines.sort_by(|a, b| a["engine"].as_str().cmp(&b["engine"].as_str()));

    // Real-time metrics
    let (cache_hits, cache_misses) = ctx.metrics.cache_stats();
    serde_json::json!({
        "uptime_secs": ctx.started.elapsed().as_secs(),
        "cache_backend": ctx.rt.cache.backend_name(),
        "engine_health_enabled": ctx.rt.health.enabled(),
        "cooling_down": cooling_down,
        "engines": engines,
        "realtime_metrics": {
            "total_requests": ctx.metrics.total_requests(),
            "requests_per_minute": round2(ctx.metrics.requests_per_minute()),
            "cache_hits": cache_hits,
            "cache_misses": cache_misses,
            "cache_hit_ratio": round2(ctx.metrics.cache_hit_ratio()),
            "avg_response_time_ms": round2(ctx.metrics.avg_response_time_ms()),
            "total_errors": ctx.metrics.total_errors(),
            "error_rate": round2(ctx.metrics.error_rate()),
        },
    })
    .to_string()
}

/// Render a tiny unicode sparkline from recent latency samples.
fn sparkline(samples: &[u128]) -> String {
    if samples.is_empty() {
        return String::new();
    }
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let max = samples.iter().copied().max().unwrap_or(1).max(1);
    samples
        .iter()
        .map(|&v| {
            let idx = ((v as f64 / max as f64) * (BARS.len() - 1) as f64).round() as usize;
            BARS[idx.min(BARS.len() - 1)]
        })
        .collect()
}

fn stats_page(ctx: &Ctx, theme: Theme) -> String {
    let stats = ctx.rt.stats();
    let health = ctx.rt.health_snapshot();
    let (cache_hits, cache_misses) = ctx.metrics.cache_stats();
    let settings = ctx.settings();

    // Compute summary statistics
    let total_calls: u64 = stats.values().map(|s| s.calls).sum();
    let total_errors: u64 = stats.values().map(|s| s.errors).sum();
    let total_results: u64 = stats.values().map(|s| s.results).sum();
    let uptime_secs = ctx.started.elapsed().as_secs();

    // Format uptime nicely
    let uptime_str = if uptime_secs >= 86400 {
        format!("{}d {}h", uptime_secs / 86400, (uptime_secs % 86400) / 3600)
    } else if uptime_secs >= 3600 {
        format!("{}h {}m", uptime_secs / 3600, (uptime_secs % 3600) / 60)
    } else if uptime_secs >= 60 {
        format!("{}m {}s", uptime_secs / 60, uptime_secs % 60)
    } else {
        format!("{}s", uptime_secs)
    };

    // Count healthy vs unhealthy engines
    let engines_total = settings.engines.iter().filter(|e| e.enabled).count();
    let cooling_engines: std::collections::HashSet<_> = health
        .iter()
        .filter(|(_, h)| h.cooling_down)
        .map(|(n, _)| n.as_str())
        .collect();
    let engines_healthy = settings
        .engines
        .iter()
        .filter(|e| e.enabled && !cooling_engines.contains(e.name.as_str()))
        .count();

    // Overall success rate
    let overall_success_rate = if total_calls > 0 {
        ((total_calls - total_errors) as f64 / total_calls as f64) * 100.0
    } else {
        100.0
    };

    // Build table rows
    let mut rows: Vec<(&String, &crate::search::EngineStat)> = stats.iter().collect();
    rows.sort_by(|a, b| a.0.cmp(b.0));
    let mut table_body = String::new();
    for (idx, (name, s)) in rows.iter().enumerate() {
        let avg = if s.calls > 0 {
            s.total_ms as f64 / s.calls as f64
        } else {
            0.0
        };
        let rate = s.success_rate() * 100.0;
        let recent: Vec<u128> = s.recent_ms.iter().copied().collect();
        let hi = health.get(*name).cloned().unwrap_or_default();
        let health_cell = if hi.cooling_down {
            format!(
                r#"<span class="health-badge cooling"><span class="dot"></span>cooling {}s</span>"#,
                hi.cooldown_remaining_secs,
            )
        } else if hi.consecutive_failures > 0 {
            format!(
                r#"<span class="health-badge degraded"><span class="dot"></span>{}x fail</span>"#,
                hi.consecutive_failures,
            )
        } else {
            r#"<span class="health-badge ok"><span class="dot"></span>healthy</span>"#.to_string()
        };
        let row_class = if idx % 2 == 0 { "even" } else { "odd" };
        table_body.push_str(&format!(
            r#"<tr class="{row_class}"><td class="engine-name">{name}</td><td class="num">{calls}</td><td class="num {err_class}">{errors}</td><td class="num">{results}</td><td class="num">{avg:.0}<span class="unit">ms</span></td><td class="num">{rate:.0}<span class="unit">%</span></td><td class="num">{recent_avg:.0}<span class="unit">ms</span></td><td class="spark">{spark}</td><td>{health_cell}</td></tr>"#,
            row_class = row_class,
            name = escape(name),
            calls = s.calls,
            errors = s.errors,
            err_class = if s.errors > 0 { "has-errors" } else { "" },
            results = s.results,
            avg = avg,
            rate = rate,
            recent_avg = s.recent_avg_ms(),
            spark = sparkline(&recent),
            health_cell = health_cell,
        ));
    }
    if table_body.is_empty() {
        table_body.push_str(r#"<tr><td colspan="9" class="empty-state">No engine calls yet. Run a search to see statistics.</td></tr>"#);
    }

    format!(
        r##"<!doctype html>
<html lang="en"{theme_attr}><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Stats Dashboard - Metasearch</title>
<style>
:root {{
  --accent: #0d9488;
  --accent-light: rgba(13,148,136,0.1);
  --bg: #f8fafc;
  --bg-card: #ffffff;
  --fg: #1e293b;
  --muted: #64748b;
  --border: #e2e8f0;
  --success: #10b981;
  --success-bg: rgba(16,185,129,0.1);
  --warning: #f59e0b;
  --warning-bg: rgba(245,158,11,0.1);
  --error: #ef4444;
  --error-bg: rgba(239,68,68,0.1);
  --row-alt: #f8fafc;
}}
html[data-theme="dark"] {{
  --bg: #0f172a;
  --bg-card: #1e293b;
  --fg: #f1f5f9;
  --muted: #94a3b8;
  --border: #334155;
  --row-alt: #1e293b;
  --success-bg: rgba(16,185,129,0.15);
  --warning-bg: rgba(245,158,11,0.15);
  --error-bg: rgba(239,68,68,0.15);
}}
@media (prefers-color-scheme: dark) {{
  :root:not([data-theme="light"]) {{
    --bg: #0f172a;
    --bg-card: #1e293b;
    --fg: #f1f5f9;
    --muted: #94a3b8;
    --border: #334155;
    --row-alt: #1e293b;
    --success-bg: rgba(16,185,129,0.15);
    --warning-bg: rgba(245,158,11,0.15);
    --error-bg: rgba(239,68,68,0.15);
  }}
}}
* {{ box-sizing: border-box; margin: 0; padding: 0; }}
body {{
  font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;
  background: var(--bg);
  color: var(--fg);
  line-height: 1.5;
  padding: 1.5rem;
  min-height: 100vh;
}}
.container {{ max-width: 1200px; margin: 0 auto; }}
header {{
  display: flex;
  align-items: center;
  justify-content: space-between;
  margin-bottom: 1.5rem;
  flex-wrap: wrap;
  gap: 1rem;
}}
.logo {{
  display: flex;
  align-items: center;
  gap: 0.5rem;
  text-decoration: none;
  color: var(--fg);
  font-size: 1.25rem;
  font-weight: 600;
}}
.logo svg {{ width: 28px; height: 28px; }}
.header-actions {{
  display: flex;
  gap: 0.75rem;
  align-items: center;
}}
.btn {{
  display: inline-flex;
  align-items: center;
  gap: 0.375rem;
  padding: 0.5rem 1rem;
  font-size: 0.875rem;
  font-weight: 500;
  border-radius: 8px;
  border: 1px solid var(--border);
  background: var(--bg-card);
  color: var(--fg);
  cursor: pointer;
  text-decoration: none;
  transition: all 0.15s;
}}
.btn:hover {{ border-color: var(--accent); color: var(--accent); }}
.btn-primary {{
  background: var(--accent);
  border-color: var(--accent);
  color: white;
}}
.btn-primary:hover {{ opacity: 0.9; color: white; }}
.summary-cards {{
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(200px, 1fr));
  gap: 1rem;
  margin-bottom: 1.5rem;
}}
.card {{
  background: var(--bg-card);
  border: 1px solid var(--border);
  border-radius: 12px;
  padding: 1.25rem;
}}
.card-label {{
  font-size: 0.75rem;
  font-weight: 500;
  text-transform: uppercase;
  letter-spacing: 0.05em;
  color: var(--muted);
  margin-bottom: 0.375rem;
}}
.card-value {{
  font-size: 1.75rem;
  font-weight: 700;
  color: var(--fg);
  display: flex;
  align-items: baseline;
  gap: 0.25rem;
}}
.card-value .unit {{
  font-size: 0.875rem;
  font-weight: 500;
  color: var(--muted);
}}
.card-sub {{
  font-size: 0.8125rem;
  color: var(--muted);
  margin-top: 0.25rem;
}}
.card.success .card-value {{ color: var(--success); }}
.card.warning .card-value {{ color: var(--warning); }}
.card.error .card-value {{ color: var(--error); }}

/* Instant Answer Cards */
.card.instant {{
  display: flex;
  align-items: center;
  gap: 0.75rem;
  padding: 0.875rem 1.25rem;
  background: linear-gradient(135deg, var(--bg-card) 0%, var(--surface) 100%);
  border: 1px solid var(--accent);
  border-left: 4px solid var(--accent);
  margin-bottom: 1rem;
}}
.card.instant.stock {{ border-left-color: #10b981; }}
.card.instant.weather {{ border-left-color: #3b82f6; }}
.card.instant.currency {{ border-left-color: #f59e0b; }}
.card.instant.calc {{ border-left-color: #8b5cf6; }}
.instant-icon {{
  font-size: 1.5rem;
  flex-shrink: 0;
}}
.instant-label {{
  font-size: 0.7rem;
  font-weight: 600;
  text-transform: uppercase;
  letter-spacing: 0.05em;
  color: var(--muted);
  margin-right: 0.5rem;
}}
.instant-answer {{
  font-size: 1rem;
  font-weight: 500;
  color: var(--fg);
  flex: 1;
}}
.source-link {{
  font-size: 0.75rem;
  color: var(--muted);
  text-decoration: none;
  opacity: 0.7;
}}
.source-link:hover {{ opacity: 1; }}

.cache-section {{
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(280px, 1fr));
  gap: 1rem;
  margin-bottom: 1.5rem;
}}
.cache-card {{
  background: var(--bg-card);
  border: 1px solid var(--border);
  border-radius: 12px;
  padding: 1.25rem;
}}
.cache-card h3 {{
  font-size: 0.875rem;
  font-weight: 600;
  margin-bottom: 1rem;
  display: flex;
  align-items: center;
  gap: 0.5rem;
}}
.cache-stats {{
  display: grid;
  grid-template-columns: repeat(3, 1fr);
  gap: 1rem;
  text-align: center;
}}
.cache-stat-value {{
  font-size: 1.25rem;
  font-weight: 700;
}}
.cache-stat-label {{
  font-size: 0.75rem;
  color: var(--muted);
}}
.hit-rate-bar {{
  height: 8px;
  background: var(--border);
  border-radius: 4px;
  overflow: hidden;
  margin-top: 1rem;
}}
.hit-rate-fill {{
  height: 100%;
  background: linear-gradient(90deg, var(--accent), var(--success));
  border-radius: 4px;
  transition: width 0.3s;
}}
.table-section {{
  background: var(--bg-card);
  border: 1px solid var(--border);
  border-radius: 12px;
  overflow: hidden;
  margin-bottom: 1.5rem;
}}
.table-header {{
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 1rem 1.25rem;
  border-bottom: 1px solid var(--border);
}}
.table-header h2 {{
  font-size: 1rem;
  font-weight: 600;
}}
.table-wrapper {{
  overflow-x: auto;
}}
table {{
  width: 100%;
  border-collapse: collapse;
  font-size: 0.875rem;
}}
th {{
  text-align: left;
  padding: 0.75rem 1rem;
  font-weight: 600;
  font-size: 0.75rem;
  text-transform: uppercase;
  letter-spacing: 0.05em;
  color: var(--muted);
  background: var(--bg);
  border-bottom: 1px solid var(--border);
  white-space: nowrap;
}}
td {{
  padding: 0.875rem 1rem;
  border-bottom: 1px solid var(--border);
  vertical-align: middle;
}}
tr.odd {{ background: var(--row-alt); }}
tr:last-child td {{ border-bottom: none; }}
.engine-name {{ font-weight: 500; }}
.num {{ font-variant-numeric: tabular-nums; }}
.num .unit {{ font-size: 0.75rem; color: var(--muted); margin-left: 0.125rem; }}
.has-errors {{ color: var(--error); }}
.spark {{
  font-family: monospace;
  font-size: 1rem;
  letter-spacing: -1px;
  color: var(--accent);
}}
.health-badge {{
  display: inline-flex;
  align-items: center;
  gap: 0.375rem;
  padding: 0.25rem 0.625rem;
  font-size: 0.75rem;
  font-weight: 500;
  border-radius: 9999px;
}}
.health-badge .dot {{
  width: 6px;
  height: 6px;
  border-radius: 50%;
}}
.health-badge.ok {{
  background: var(--success-bg);
  color: var(--success);
}}
.health-badge.ok .dot {{ background: var(--success); }}
.health-badge.degraded {{
  background: var(--warning-bg);
  color: var(--warning);
}}
.health-badge.degraded .dot {{ background: var(--warning); }}
.health-badge.cooling {{
  background: var(--error-bg);
  color: var(--error);
}}
.health-badge.cooling .dot {{ background: var(--error); animation: pulse 1.5s infinite; }}
@keyframes pulse {{
  0%, 100% {{ opacity: 1; }}
  50% {{ opacity: 0.4; }}
}}
.empty-state {{
  text-align: center;
  padding: 3rem 1rem !important;
  color: var(--muted);
}}
footer {{
  display: flex;
  align-items: center;
  justify-content: space-between;
  flex-wrap: wrap;
  gap: 1rem;
  padding-top: 1rem;
  border-top: 1px solid var(--border);
  font-size: 0.8125rem;
  color: var(--muted);
}}
footer a {{ color: var(--accent); text-decoration: none; }}
footer a:hover {{ text-decoration: underline; }}
@media (max-width: 640px) {{
  .summary-cards {{ grid-template-columns: repeat(2, 1fr); }}
  .card {{ padding: 1rem; }}
  .card-value {{ font-size: 1.5rem; }}
  th, td {{ padding: 0.625rem 0.75rem; }}
  .table-header {{ flex-direction: column; align-items: flex-start; gap: 0.5rem; }}
}}
</style>
</head><body>
<div class="container">
  <header>
    <a href="/" class="logo">
      <svg viewBox="0 0 64 64" fill="none"><circle cx="32" cy="32" r="28" stroke="currentColor" stroke-width="4"/><circle cx="32" cy="32" r="12" fill="currentColor"/></svg>
      Metasearch Stats
    </a>
    <div class="header-actions">
      <a href="/stats.json" class="btn">Export JSON</a>
      <button class="btn btn-primary" onclick="location.reload()">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M23 4v6h-6M1 20v-6h6"/><path d="M3.51 9a9 9 0 0 1 14.85-3.36L23 10M1 14l4.64 4.36A9 9 0 0 0 20.49 15"/></svg>
        Refresh
      </button>
    </div>
  </header>

  <section class="summary-cards">
    <div class="card">
      <div class="card-label">Uptime</div>
      <div class="card-value">{uptime_str}</div>
      <div class="card-sub">{uptime_secs} seconds total</div>
    </div>
    <div class="card">
      <div class="card-label">Total Queries</div>
      <div class="card-value">{total_calls}</div>
      <div class="card-sub">{total_results} results returned</div>
    </div>
    <div class="card {success_class}">
      <div class="card-label">Success Rate</div>
      <div class="card-value">{overall_success_rate:.1}<span class="unit">%</span></div>
      <div class="card-sub">{total_errors} errors total</div>
    </div>
    <div class="card {engines_class}">
      <div class="card-label">Engine Health</div>
      <div class="card-value">{engines_healthy}<span class="unit">/ {engines_total}</span></div>
      <div class="card-sub">{cooling_count} cooling down</div>
    </div>
  </section>

  <section class="summary-cards">
    <div class="card">
      <div class="card-label">Total Requests</div>
      <div class="card-value">{metrics_total_requests}</div>
      <div class="card-sub">All HTTP requests</div>
    </div>
    <div class="card">
      <div class="card-label">Requests/min</div>
      <div class="card-value">{metrics_rpm:.1}</div>
      <div class="card-sub">Rolling 60s window</div>
    </div>
    <div class="card">
      <div class="card-label">Avg Response</div>
      <div class="card-value">{metrics_avg_response:.1}<span class="unit">ms</span></div>
      <div class="card-sub">All requests</div>
    </div>
    <div class="card {metrics_error_class}">
      <div class="card-label">Error Rate</div>
      <div class="card-value">{metrics_error_rate:.2}<span class="unit">%</span></div>
      <div class="card-sub">{metrics_total_errors} total errors</div>
    </div>
  </section>

  <section class="cache-section">
    <div class="cache-card">
      <h3>
        <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M12 2L2 7l10 5 10-5-10-5zM2 17l10 5 10-5M2 12l10 5 10-5"/></svg>
        Cache Statistics
      </h3>
      <div class="cache-stats">
        <div>
          <div class="cache-stat-value">{cache_hits}</div>
          <div class="cache-stat-label">Hits</div>
        </div>
        <div>
          <div class="cache-stat-value">{cache_misses}</div>
          <div class="cache-stat-label">Misses</div>
        </div>
        <div>
          <div class="cache-stat-value">{hit_rate:.1}%</div>
          <div class="cache-stat-label">Hit Rate</div>
        </div>
      </div>
      <div class="hit-rate-bar">
        <div class="hit-rate-fill" style="width: {hit_rate}%"></div>
      </div>
    </div>
    <div class="cache-card">
      <h3>
        <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><circle cx="12" cy="12" r="10"/><path d="M12 6v6l4 2"/></svg>
        Cache Configuration
      </h3>
      <div class="cache-stats">
        <div>
          <div class="cache-stat-value" style="text-transform: capitalize">{backend}</div>
          <div class="cache-stat-label">Backend</div>
        </div>
        <div>
          <div class="cache-stat-value">{cache_size}</div>
          <div class="cache-stat-label">Entries</div>
        </div>
        <div>
          <div class="cache-stat-value">{cache_ttl}s</div>
          <div class="cache-stat-label">TTL</div>
        </div>
      </div>
    </div>
  </section>

  <section class="table-section">
    <div class="table-header">
      <h2>Engine Performance</h2>
      <span style="color: var(--muted); font-size: 0.8125rem">{engine_count} engines tracked</span>
    </div>
    <div class="table-wrapper">
      <table>
        <thead>
          <tr>
            <th>Engine</th>
            <th>Calls</th>
            <th>Errors</th>
            <th>Results</th>
            <th>Avg Latency</th>
            <th>Success</th>
            <th>Recent Avg</th>
            <th>Trend</th>
            <th>Health</th>
          </tr>
        </thead>
        <tbody>
          {table_body}
        </tbody>
      </table>
    </div>
  </section>

  <footer>
    <div>
      <a href="/">Home</a> &middot;
      <a href="/preferences">Preferences</a> &middot;
      <a href="/health">Health Check</a> &middot;
      {theme_tools}
    </div>
    <div>
      Version {version} &middot; {stack}
    </div>
  </footer>
</div>
<script>
// Auto-refresh every 30 seconds
setTimeout(function() {{ location.reload(); }}, 30000);
</script>
</body></html>"##,
        theme_attr = theme.attr(),
        uptime_str = uptime_str,
        uptime_secs = uptime_secs,
        total_calls = total_calls,
        total_results = total_results,
        overall_success_rate = overall_success_rate,
        success_class = if overall_success_rate >= 95.0 { "success" } else if overall_success_rate >= 80.0 { "warning" } else { "error" },
        total_errors = total_errors,
        engines_healthy = engines_healthy,
        engines_total = engines_total,
        engines_class = if engines_healthy == engines_total { "success" } else if engines_healthy > engines_total / 2 { "warning" } else { "error" },
        cooling_count = cooling_engines.len(),
        // Real-time metrics
        metrics_total_requests = ctx.metrics.total_requests(),
        metrics_rpm = ctx.metrics.requests_per_minute(),
        metrics_avg_response = ctx.metrics.avg_response_time_ms(),
        metrics_error_rate = ctx.metrics.error_rate() * 100.0,
        metrics_total_errors = ctx.metrics.total_errors(),
        metrics_error_class = if ctx.metrics.error_rate() < 0.01 { "success" } else if ctx.metrics.error_rate() < 0.05 { "warning" } else { "error" },
        // Cache stats
        cache_hits = cache_hits,
        cache_misses = cache_misses,
        hit_rate = {
            let total = cache_hits + cache_misses;
            if total == 0 { 0.0 } else { (cache_hits as f64 / total as f64) * 100.0 }
        },
        backend = ctx.rt.cache.backend_name(),
        cache_size = ctx.rt.cache.len(),
        cache_ttl = settings.server.cache_ttl_secs,
        engine_count = rows.len(),
        table_body = table_body,
        theme_tools = theme_tools(theme, "/stats"),
        version = crate::build_info::VERSION,
        stack = crate::build_info::STACK,
    )
}

fn opensearch_xml(settings: &Settings) -> String {
    let base = format!(
        "http://{}:{}",
        settings.server.bind_address, settings.server.port
    );
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<OpenSearchDescription xmlns="http://a9.com/-/spec/opensearch/1.1/">
  <ShortName>metasearch</ShortName>
  <Description>AI Studio privacy-respecting metasearch</Description>
  <InputEncoding>UTF-8</InputEncoding>
  <Url type="text/html" method="get" template="{base}/search?q={{searchTerms}}"/>
  <Url type="application/json" method="get" template="{base}/search?q={{searchTerms}}&amp;format=json"/>
  <Url type="application/rss+xml" method="get" template="{base}/search?q={{searchTerms}}&amp;format=rss"/>
  <Url type="application/x-suggestions+json" method="get" template="{base}/autocompleter?q={{searchTerms}}"/>
</OpenSearchDescription>"#
    )
}

// -------------------------------------------------------------- RSS / CSV

fn rss(response: &SearchResponse) -> String {
    let mut items = String::new();
    for r in &response.results {
        items.push_str(&format!(
            "    <item>\n      <title>{}</title>\n      <link>{}</link>\n      <description>{}</description>\n    </item>\n",
            xml_escape(&r.title),
            xml_escape(&r.url),
            xml_escape(&r.content),
        ));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0">
  <channel>
    <title>metasearch: {q}</title>
    <description>Search results for {q}</description>
{items}  </channel>
</rss>"#,
        q = xml_escape(&response.query),
        items = items,
    )
}

fn csv(response: &SearchResponse) -> String {
    let mut out = String::from("title,url,content,engines,score,category\n");
    for r in &response.results {
        out.push_str(&format!(
            "{},{},{},{},{:.4},{}\n",
            csv_field(&r.title),
            csv_field(&r.url),
            csv_field(&r.content),
            csv_field(&r.engines.join(" ")),
            r.score,
            csv_field(&r.category),
        ));
    }
    out
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn llms_txt() -> String {
    include_str!("../../static/llms.txt").replace("{{BUILD}}", &crate::build_info::version_label())
}

fn ai_handoff_json() -> String {
    serde_json::json!({
        "project": "metasearch",
        "description": "Privacy-first self-hostable metasearch and local-AI answer engine.",
        "domain": "https://orgos2.zeus.kim",
        "api_base": "https://orgos2.zeus.kim/api/v1",
        "openapi_url": "https://orgos2.zeus.kim/openapi.json",
        "health_url": "https://orgos2.zeus.kim/api/v1/health",
        "models_url": "https://orgos2.zeus.kim/api/v1/models",
        "sanity_urls": [
            "https://orgos2.zeus.kim/api/v1/health",
            "https://orgos2.zeus.kim/api/v1/models",
            "https://orgos2.zeus.kim/api/v1/news_digest?q=technology&refresh=1"
        ],
        "docs_urls": [
            "https://orgos2.zeus.kim/llms.txt",
            "https://orgos2.zeus.kim/ai-handoff",
            "https://orgos2.zeus.kim/docs/project-status",
            "repo:docs/PROJECT-STATUS.md",
            "repo:docs/CONTINUATION.md",
            "repo:docs/adding-an-engine.md",
            "repo:docs/custom-engines.md"
        ],
        "continuation_urls": [
            "https://orgos2.zeus.kim/llms.txt",
            "https://orgos2.zeus.kim/.well-known/ai-handoff.json"
        ],
        "repo_path_hint": "/Users/dragon/Projects/metasearch",
        "do_not_touch_without_explicit_request": [
            "/Users/dragon/Projects/ai-studio"
        ],
        "current_build": {
            "stack": crate::build_info::STACK,
            "version": crate::build_info::VERSION,
            "git_sha": crate::build_info::GIT_SHA,
            "label": crate::build_info::version_label()
        },
        "core_features": [
            "privacy-first metasearch",
            "Discover/News digest",
            "local Ollama-compatible AI",
            "article rewrite and summary flows",
            "image handling and proxying",
            "Korean language auto-detect"
        ],
        "recommended_workflow": [
            "Start at https://orgos2.zeus.kim/llms.txt.",
            "Fetch /.well-known/ai-handoff.json and /openapi.json.",
            "Run /api/v1/health, /api/v1/models, and /api/v1/news_digest?q=technology&refresh=1.",
            "For local agents, inspect git status in /Users/dragon/Projects/metasearch before edits.",
            "Read docs/PROJECT-STATUS.md and docs/CONTINUATION.md before continuing.",
            "Keep changes small and avoid unrelated dirty files."
        ],
        "safety_notes": [
            "Preserve user WIP and never revert dirty files without approval.",
            "Do not add tracking, analytics, or third-party client beacons.",
            "Prefer local Ollama-compatible AI endpoints; hosted AI calls need explicit approval.",
            "Treat fetched article and image content as untrusted input.",
            "Recent news-card image/fetch work changed; validate before further edits."
        ]
    })
    .to_string()
}

fn ai_handoff_page() -> String {
    let build = escape(&crate::build_info::version_label());
    format!(
        r#"<!doctype html>
<meta charset="utf-8">
<meta name="robots" content="noindex">
<title>metasearch AI handoff</title>
<style>
body {{ font-family: system-ui, sans-serif; max-width: 780px; margin: 2rem auto; padding: 0 1rem; line-height: 1.55; }}
code {{ background: #8882; padding: .1rem .25rem; border-radius: .25rem; }}
li {{ margin: .25rem 0; }}
</style>
<h1>metasearch AI handoff</h1>
<p>Start at <a href="/llms.txt"><code>/llms.txt</code></a>, then read <a href="/.well-known/ai-handoff.json"><code>/.well-known/ai-handoff.json</code></a> and <a href="/openapi.json"><code>/openapi.json</code></a>.</p>
<p>Local source repo for agents on this machine: <code>/Users/dragon/Projects/metasearch</code>. Do not touch <code>/Users/dragon/Projects/ai-studio</code> for this app unless explicitly asked.</p>
<h2>Sanity checks</h2>
<ul>
<li><a href="/api/v1/health"><code>/api/v1/health</code></a></li>
<li><a href="/api/v1/models"><code>/api/v1/models</code></a></li>
<li><a href="/api/v1/news_digest?q=technology&amp;refresh=1"><code>/api/v1/news_digest?q=technology&amp;refresh=1</code></a></li>
</ul>
<h2>Repo docs</h2>
<ul>
<li><code>docs/PROJECT-STATUS.md</code></li>
<li><code>docs/CONTINUATION.md</code></li>
</ul>
<p>Current build: <code>{build}</code></p>
<p>Caution: news-card image/fetch work recently changed; validate that area before further edits.</p>
"#
    )
}

/// Answer UI (vanilla HTML/CSS/JS, embedded at build time).
fn answer_ui_page(_theme: Theme, settings: &Settings) -> String {
    let logo_html = match &settings.branding.logo_url {
        Some(url) => format!(
            r#"<img src="{}" alt="{}" style="height:24px;width:auto;vertical-align:middle;margin-right:.375rem">{}"#,
            escape(url),
            escape(&settings.branding.app_name),
            escape(&settings.branding.app_name)
        ),
        None => format!(
            r#"<svg width="24" height="24" viewBox="0 0 64 64" fill="none" style="vertical-align:middle;margin-right:.375rem"><circle cx="32" cy="32" r="28" stroke="currentColor" stroke-width="4"/><circle cx="32" cy="32" r="12" fill="currentColor"/></svg>{}"#,
            escape(&settings.branding.app_name)
        ),
    };
    let favicon_html = match &settings.branding.favicon_url {
        Some(url) => format!(r#"<link rel="icon" href="{}"><link rel="apple-touch-icon" href="{}">"#, escape(url), escape(url)),
        None => r#"<link rel="icon" type="image/svg+xml" href="/favicon.svg"><link rel="apple-touch-icon" href="/favicon.svg">"#.to_string(),
    };
    include_str!("../../static/index.html") 
        .replace("{{AI_MODEL}}", &escape(&settings.ai.model))
        .replace("{{AI_BASE}}", &escape(&settings.ai.base_url))
        .replace(
            "{{AI_ENABLED}}",
            if settings.ai.enabled { "true" } else { "false" },
        )
        .replace("{{VERSION}}", crate::build_info::VERSION)
        .replace("{{GIT_SHA}}", crate::build_info::GIT_SHA)
        .replace("{{APP_NAME}}", &escape(&settings.branding.app_name))
        .replace("{{LOGO_HTML}}", &logo_html)
        .replace("{{FAVICON_HTML}}", &favicon_html)
}

// ----------------------------------------------------------------- HTML UI

fn page_styles() -> &'static str {
    r#"<style>
  :root {
    color-scheme: light dark;
    --accent: #0d9488;
    --accent-hover: #0f766e;
    --accent-light: rgba(13,148,136,0.08);
    --accent-glow: rgba(13,148,136,0.2);
    --bg: #f8fafc;
    --bg-subtle: #f1f5f9;
    --fg: #1e293b;
    --muted: #64748b;
    --card-bg: #ffffff;
    --card-border: #e2e8f0;
    --card-shadow: 0 1px 3px rgba(0,0,0,0.04), 0 1px 2px rgba(0,0,0,0.02);
    --card-shadow-hover: 0 8px 25px rgba(0,0,0,0.08), 0 4px 10px rgba(0,0,0,0.04);
    --badge-local: #10b981;
    --badge-local-bg: rgba(16,185,129,0.1);
    --badge-web: #6366f1;
    --badge-web-bg: rgba(99,102,241,0.1);
    --badge-ai: #8b5cf6;
    --badge-news: #f59e0b;
    --badge-news-bg: rgba(245,158,11,0.1);
    --badge-images: #ec4899;
    --input-bg: #ffffff;
    --input-border: #cbd5e1;
    --divider: #e2e8f0;
  }
  html[data-theme="light"] {
    color-scheme: light;
    --bg: #f8fafc;
    --bg-subtle: #f1f5f9;
    --fg: #1e293b;
    --muted: #64748b;
    --card-bg: #ffffff;
    --card-border: #e2e8f0;
    --input-bg: #ffffff;
    --input-border: #cbd5e1;
    --divider: #e2e8f0;
  }
  html[data-theme="dark"] {
    color-scheme: dark;
    --bg: #0f172a;
    --bg-subtle: #1e293b;
    --fg: #f1f5f9;
    --muted: #94a3b8;
    --card-bg: #1e293b;
    --card-border: #334155;
    --card-shadow: 0 1px 3px rgba(0,0,0,0.3);
    --card-shadow-hover: 0 12px 35px rgba(0,0,0,0.4);
    --input-bg: #1e293b;
    --input-border: #475569;
    --divider: #334155;
    --accent-light: rgba(13,148,136,0.12);
    --badge-local-bg: rgba(16,185,129,0.15);
    --badge-web-bg: rgba(99,102,241,0.15);
    --badge-news-bg: rgba(245,158,11,0.15);
  }
  @media (prefers-color-scheme: dark) {
    :root:not([data-theme="light"]) {
      --bg: #0f172a;
      --bg-subtle: #1e293b;
      --fg: #f1f5f9;
      --muted: #94a3b8;
      --card-bg: #1e293b;
      --card-border: #334155;
      --card-shadow: 0 1px 3px rgba(0,0,0,0.3);
      --card-shadow-hover: 0 12px 35px rgba(0,0,0,0.4);
      --input-bg: #1e293b;
      --input-border: #475569;
      --divider: #334155;
      --accent-light: rgba(13,148,136,0.12);
      --badge-local-bg: rgba(16,185,129,0.15);
      --badge-web-bg: rgba(99,102,241,0.15);
      --badge-news-bg: rgba(245,158,11,0.15);
    }
  }
  * { box-sizing: border-box; margin: 0; padding: 0; }
  body {
    font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, 'Helvetica Neue', Arial, sans-serif;
    max-width: 860px;
    margin: 0 auto;
    padding: 1.5rem 1.5rem 3rem;
    background: var(--bg);
    color: var(--fg);
    line-height: 1.65;
    -webkit-font-smoothing: antialiased;
    -moz-osx-font-smoothing: grayscale;
  }
  h1 { margin-bottom: 1.25rem; }
  h1 a {
    color: inherit;
    text-decoration: none;
    font-weight: 700;
    font-size: 1.5rem;
    letter-spacing: -0.03em;
    display: inline-flex;
    align-items: center;
    gap: 0.5rem;
    transition: color 0.2s;
  }
  h1 a:hover { color: var(--accent); }
  form.search {
    display: flex;
    gap: 0.75rem;
    position: relative;
    margin-bottom: 1rem;
  }
  input[type=search] {
    flex: 1;
    padding: 0.9375rem 1.125rem;
    font-size: 1rem;
    border-radius: 14px;
    border: 1.5px solid var(--input-border);
    background: var(--input-bg);
    color: var(--fg);
    outline: none;
    transition: border-color 0.2s, box-shadow 0.2s;
  }
  input[type=search]:focus {
    border-color: var(--accent);
    box-shadow: 0 0 0 4px var(--accent-glow);
  }
  button {
    padding: 0.9375rem 1.75rem;
    font-size: 1rem;
    font-weight: 600;
    border-radius: 14px;
    border: 0;
    background: var(--accent);
    color: #fff;
    cursor: pointer;
    transition: background 0.2s, transform 0.1s, box-shadow 0.2s;
  }
  button:hover {
    background: var(--accent-hover);
    box-shadow: 0 4px 12px var(--accent-glow);
  }
  button:active { transform: scale(0.97); }

  /* Category tabs */
  .tabs {
    display: flex;
    gap: 0.375rem;
    flex-wrap: wrap;
    margin: 0.75rem 0 1.5rem;
    padding: 0.5rem;
    background: var(--bg-subtle);
    border-radius: 12px;
  }
  .tabs a {
    font-size: 0.8125rem;
    font-weight: 500;
    padding: 0.5rem 0.875rem;
    border-radius: 8px;
    border: none;
    background: transparent;
    text-decoration: none;
    color: var(--muted);
    transition: all 0.2s;
  }
  .tabs a:hover {
    color: var(--fg);
    background: var(--card-bg);
  }
  .tabs a.active {
    background: var(--accent);
    color: #fff;
    box-shadow: 0 2px 8px var(--accent-glow);
  }

  /* Result cards */
  .results-list {
    display: flex;
    flex-direction: column;
    gap: 0.875rem;
  }
  article.result-card {
    background: var(--card-bg);
    border: 1px solid var(--card-border);
    border-radius: 16px;
    padding: 1.25rem 1.375rem;
    box-shadow: var(--card-shadow);
    transition: all 0.25s cubic-bezier(0.4, 0, 0.2, 1);
    position: relative;
  }
  article.result-card::before {
    content: '';
    position: absolute;
    left: 0;
    top: 0;
    bottom: 0;
    width: 3px;
    background: transparent;
    border-radius: 16px 0 0 16px;
    transition: background 0.25s;
  }
  article.result-card:hover {
    box-shadow: var(--card-shadow-hover);
    border-color: var(--accent);
    transform: translateY(-2px);
  }
  article.result-card:hover::before {
    background: var(--accent);
  }
  .result-header {
    display: flex;
    align-items: flex-start;
    gap: 0.875rem;
    margin-bottom: 0.625rem;
  }
  .result-favicon {
    width: 36px;
    height: 36px;
    border-radius: 10px;
    background: var(--bg-subtle);
    border: 1px solid var(--divider);
    flex-shrink: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    overflow: hidden;
    transition: transform 0.2s, box-shadow 0.2s;
  }
  article.result-card:hover .result-favicon {
    transform: scale(1.05);
    box-shadow: 0 2px 8px rgba(0,0,0,0.1);
  }
  .result-favicon img {
    width: 22px;
    height: 22px;
    object-fit: contain;
  }
  .result-favicon-placeholder {
    width: 22px;
    height: 22px;
    border-radius: 6px;
    background: linear-gradient(135deg, var(--accent) 0%, var(--accent-hover) 100%);
    opacity: 0.4;
  }
  .result-title-section { flex: 1; min-width: 0; }
  a.title {
    font-size: 1.0625rem;
    font-weight: 600;
    color: var(--fg);
    text-decoration: none;
    line-height: 1.45;
    display: block;
    margin-bottom: 0.1875rem;
    transition: color 0.15s;
  }
  a.title:hover { color: var(--accent); }
  .url {
    color: var(--accent);
    font-size: 0.8125rem;
    word-break: break-all;
    display: flex;
    align-items: center;
    gap: 0.375rem;
    opacity: 0.85;
  }
  .url-text {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 100%;
  }
  .content {
    color: var(--muted);
    font-size: 0.9375rem;
    line-height: 1.65;
    margin: 0.625rem 0 0;
  }
  .result-footer {
    display: flex;
    align-items: center;
    justify-content: space-between;
    flex-wrap: wrap;
    gap: 0.625rem;
    margin-top: 0.875rem;
    padding-top: 0.75rem;
    border-top: 1px solid var(--divider);
  }
  .source-badges {
    display: flex;
    gap: 0.375rem;
    flex-wrap: wrap;
  }
  .badge {
    display: inline-flex;
    align-items: center;
    gap: 0.25rem;
    font-size: 0.6875rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    padding: 0.3125rem 0.5625rem;
    border-radius: 6px;
    transition: transform 0.15s, box-shadow 0.15s;
  }
  .badge:hover {
    transform: translateY(-1px);
  }
  .badge.local {
    background: var(--badge-local-bg);
    color: var(--badge-local);
  }
  .badge.web {
    background: var(--badge-web-bg);
    color: var(--badge-web);
  }
  .badge.ai {
    background: rgba(139,92,246,0.1);
    color: var(--badge-ai);
  }
  .badge.news {
    background: var(--badge-news-bg);
    color: var(--badge-news);
  }
  .badge.images {
    background: rgba(236,72,153,0.1);
    color: var(--badge-images);
  }
  .meta {
    font-size: 0.75rem;
    color: var(--muted);
    display: flex;
    align-items: center;
    gap: 0.5rem;
  }
  .score-bar {
    width: 48px;
    height: 5px;
    background: var(--divider);
    border-radius: 3px;
    overflow: hidden;
  }
  .score-fill {
    height: 100%;
    background: linear-gradient(90deg, var(--accent) 0%, var(--accent-hover) 100%);
    border-radius: 3px;
    transition: width 0.3s ease-out;
  }

  /* Summary and highlights */
  .summary {
    margin: 0.625rem 0 0;
    font-size: 0.875rem;
    color: var(--muted);
    font-style: italic;
    padding: 0.625rem 0.875rem;
    background: var(--accent-light);
    border-radius: 8px;
    border-left: 3px solid var(--accent);
  }
  .hls {
    margin: 0.625rem 0 0;
    display: flex;
    flex-wrap: wrap;
    gap: 0.375rem;
  }
  .hls .hl {
    display: inline-block;
    font-size: 0.6875rem;
    font-weight: 500;
    padding: 0.25rem 0.625rem;
    border-radius: 999px;
    background: var(--accent-light);
    color: var(--accent);
    border: 1px solid transparent;
    transition: all 0.15s;
  }
  .hls .hl:hover {
    border-color: var(--accent);
    background: var(--accent-glow);
  }

  /* Warning and info boxes */
  .warn {
    background: linear-gradient(135deg, #fef3c7 0%, #fef9c3 100%);
    border: 1px solid #fbbf24;
    color: #92400e;
    padding: 0.875rem 1.125rem;
    border-radius: 12px;
    font-size: 0.875rem;
    margin: 1rem 0;
    display: flex;
    align-items: center;
    gap: 0.625rem;
    box-shadow: 0 2px 8px rgba(251,191,36,0.15);
  }
  .warn::before {
    content: '';
    flex-shrink: 0;
    width: 18px;
    height: 18px;
    background: url("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24' fill='%23f59e0b'%3E%3Cpath d='M12 2L1 21h22L12 2zm0 3.5L19.5 19h-15L12 5.5zM11 10v4h2v-4h-2zm0 6v2h2v-2h-2z'/%3E%3C/svg%3E") center/contain no-repeat;
  }
  @media (prefers-color-scheme: dark) {
    :root:not([data-theme="light"]) .warn {
      background: linear-gradient(135deg, rgba(245,158,11,0.15) 0%, rgba(251,191,36,0.1) 100%);
      border-color: rgba(245,158,11,0.4);
      color: #fcd34d;
    }
    :root:not([data-theme="light"]) .warn::before {
      background-image: url("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24' fill='%23fcd34d'%3E%3Cpath d='M12 2L1 21h22L12 2zm0 3.5L19.5 19h-15L12 5.5zM11 10v4h2v-4h-2zm0 6v2h2v-2h-2z'/%3E%3C/svg%3E");
    }
  }
  html[data-theme="dark"] .warn {
    background: linear-gradient(135deg, rgba(245,158,11,0.15) 0%, rgba(251,191,36,0.1) 100%);
    border-color: rgba(245,158,11,0.4);
    color: #fcd34d;
  }
  html[data-theme="dark"] .warn::before {
    background-image: url("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24' fill='%23fcd34d'%3E%3Cpath d='M12 2L1 21h22L12 2zm0 3.5L19.5 19h-15L12 5.5zM11 10v4h2v-4h-2zm0 6v2h2v-2h-2z'/%3E%3C/svg%3E");
  }

  /* AI/Answer cards */
  .card {
    background: var(--card-bg);
    border: 1px solid var(--card-border);
    border-radius: 16px;
    padding: 1.375rem;
    margin: 1.25rem 0;
    box-shadow: var(--card-shadow);
    transition: box-shadow 0.2s;
  }
  .card:hover {
    box-shadow: var(--card-shadow-hover);
  }
  .card.ai {
    border-color: rgba(139,92,246,0.3);
    background: linear-gradient(135deg, rgba(139,92,246,0.06) 0%, rgba(139,92,246,0.02) 100%);
    position: relative;
    overflow: hidden;
  }
  .card.ai::before {
    content: '';
    position: absolute;
    top: 0;
    left: 0;
    right: 0;
    height: 3px;
    background: linear-gradient(90deg, var(--badge-ai) 0%, #a78bfa 100%);
  }
  .card h3 {
    margin: 0 0 0.875rem;
    font-size: 0.6875rem;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--muted);
    display: flex;
    align-items: center;
    gap: 0.5rem;
  }
  .card.ai h3 { color: var(--badge-ai); }

  /* Results count */
  .count {
    color: var(--muted);
    font-size: 0.8125rem;
    margin-bottom: 1.125rem;
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.5rem 0.875rem;
    background: var(--bg-subtle);
    border-radius: 8px;
    width: fit-content;
  }

  /* Image grid */
  .imgs {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(175px, 1fr));
    gap: 0.875rem;
  }
  .imgs a {
    display: block;
    border-radius: 14px;
    overflow: hidden;
    background: var(--card-bg);
    border: 1px solid var(--card-border);
    transition: all 0.25s cubic-bezier(0.4, 0, 0.2, 1);
    aspect-ratio: 1;
  }
  .imgs a:hover {
    transform: scale(1.03) translateY(-2px);
    box-shadow: var(--card-shadow-hover);
    border-color: var(--accent);
  }
  .imgs img {
    width: 100%;
    height: 100%;
    object-fit: cover;
  }

  /* Pagination */
  .pager {
    display: flex;
    justify-content: center;
    align-items: center;
    gap: 0.375rem;
    margin: 2.5rem 0 1.25rem;
    padding-top: 1.75rem;
    border-top: 1px solid var(--divider);
  }
  .pager a, .pager span {
    text-decoration: none;
    padding: 0.625rem 0.875rem;
    min-width: 42px;
    text-align: center;
    font-size: 0.875rem;
    font-weight: 500;
    border-radius: 10px;
    color: var(--muted);
    background: var(--card-bg);
    border: 1px solid var(--card-border);
    transition: all 0.2s cubic-bezier(0.4, 0, 0.2, 1);
  }
  .pager a:hover {
    background: var(--accent);
    color: #fff;
    border-color: var(--accent);
    transform: translateY(-2px);
    box-shadow: 0 4px 12px var(--accent-glow);
  }
  .pager .pg-cur {
    background: var(--accent);
    color: #fff;
    border-color: var(--accent);
    font-weight: 600;
    box-shadow: 0 2px 8px var(--accent-glow);
  }
  .pager .pg-prev, .pager .pg-next {
    font-weight: 600;
    padding: 0.625rem 1.125rem;
    display: flex;
    align-items: center;
    gap: 0.25rem;
  }

  /* Chips/suggestions */
  .chips {
    margin: 1rem 0;
    font-size: 0.875rem;
    color: var(--muted);
    padding: 0.875rem 1rem;
    background: var(--bg-subtle);
    border-radius: 10px;
  }
  .chips a {
    color: var(--accent);
    text-decoration: none;
    font-weight: 500;
    margin-right: 0.875rem;
    padding: 0.25rem 0.5rem;
    border-radius: 4px;
    transition: background 0.15s;
  }
  .chips a:hover {
    background: var(--accent-light);
  }

  /* Tools/footer */
  .tools {
    font-size: 0.8125rem;
    color: var(--muted);
    margin: 1.75rem 0;
    padding: 1rem 1.25rem;
    background: var(--card-bg);
    border: 1px solid var(--card-border);
    border-radius: 14px;
    display: flex;
    gap: 1.5rem;
    flex-wrap: wrap;
    align-items: center;
    box-shadow: var(--card-shadow);
  }
  .tools a {
    color: var(--accent);
    text-decoration: none;
    font-weight: 500;
    padding: 0.25rem 0.5rem;
    border-radius: 6px;
    transition: background 0.15s;
  }
  .tools a:hover {
    background: var(--accent-light);
  }

  /* Stats table */
  table.stats { width: 100%; border-collapse: collapse; font-size: 0.875rem; }
  table.stats td, table.stats th {
    border-bottom: 1px solid var(--divider);
    padding: 0.875rem 0.625rem;
    text-align: left;
  }
  table.stats th { font-weight: 600; color: var(--muted); font-size: 0.75rem; text-transform: uppercase; letter-spacing: 0.05em; }
  table.stats tr:hover td { background: var(--bg-subtle); }
  .bar {
    height: 6px;
    background: linear-gradient(90deg, var(--accent) 0%, var(--accent-hover) 100%);
    border-radius: 3px;
    display: inline-block;
  }
  .spark { font-family: monospace; letter-spacing: -1px; }
  table.stats .ok { color: #16a34a; }
  table.stats .degraded { color: #d97706; }
  table.stats .cooling { color: #dc2626; font-weight: 600; }

  /* Autocomplete dropdown */
  .ac {
    position: absolute;
    top: 100%;
    left: 0;
    right: 0;
    background: var(--card-bg);
    border: 1px solid var(--card-border);
    border-radius: 14px;
    margin-top: 0.5rem;
    z-index: 100;
    overflow: hidden;
    box-shadow: var(--card-shadow-hover);
  }
  .ac div {
    padding: 0.875rem 1.125rem;
    cursor: pointer;
    font-size: 0.9375rem;
    transition: background 0.15s;
    border-bottom: 1px solid var(--divider);
  }
  .ac div:last-child { border-bottom: none; }
  .ac div:hover { background: var(--accent-light); color: var(--accent); }

  /* Mobile responsive */
  @media (max-width: 640px) {
    body { padding: 1rem 1rem 2rem; }
    h1 a { font-size: 1.25rem; }
    form.search { flex-direction: column; gap: 0.625rem; }
    input[type=search] { font-size: 16px; /* Prevents iOS zoom on focus */ }
    button { width: 100%; padding: 0.875rem 1.5rem; min-height: 48px; }
    article.result-card { padding: 1rem 1.125rem; border-radius: 14px; }
    article.result-card:hover { transform: none; }
    article.result-card::before { width: 2px; }
    .result-header { gap: 0.625rem; }
    .result-favicon { width: 32px; height: 32px; border-radius: 8px; }
    .result-favicon img { width: 18px; height: 18px; }
    a.title { font-size: 0.9375rem; line-height: 1.4; }
    .content { font-size: 0.875rem; }
    .result-footer { gap: 0.5rem; padding-top: 0.625rem; margin-top: 0.625rem; }
    .pager { gap: 0.375rem; flex-wrap: wrap; justify-content: center; }
    .pager a, .pager span {
      padding: 0.625rem 0.875rem;
      min-width: 44px;
      min-height: 44px;
      font-size: 0.875rem;
      border-radius: 8px;
      display: flex;
      align-items: center;
      justify-content: center;
    }
    .tools { padding: 0.875rem 1rem; gap: 1rem; border-radius: 12px; flex-wrap: wrap; }
    .tabs { padding: 0.375rem; border-radius: 10px; overflow-x: auto; -webkit-overflow-scrolling: touch; flex-wrap: nowrap; }
    .tabs::-webkit-scrollbar { display: none; }
    .tabs a { padding: 0.5rem 0.875rem; font-size: 0.75rem; border-radius: 6px; min-height: 44px; white-space: nowrap; display: flex; align-items: center; }
    .count { font-size: 0.75rem; padding: 0.375rem 0.75rem; }
    .chips { padding: 0.75rem; font-size: 0.8125rem; }
    .chips a { display: inline-block; padding: 0.5rem 0; min-height: 44px; line-height: 1.8; }
    .imgs { grid-template-columns: repeat(2, 1fr); gap: 0.5rem; }
    table.stats { font-size: 0.75rem; display: block; overflow-x: auto; }
    table.stats td, table.stats th { padding: 0.5rem 0.375rem; white-space: nowrap; }
  }
  @media (max-width: 380px) {
    body { padding: 0.75rem 0.75rem 2rem; }
    .pager .pg-prev, .pager .pg-next { padding: 0.5rem 0.75rem; }
    .tabs a { padding: 0.4375rem 0.625rem; font-size: 0.6875rem; }
    .tools { gap: 0.75rem; padding: 0.75rem; }
  }
  /* Touch device enhancements */
  @media (hover: none) and (pointer: coarse) {
    button:active { transform: scale(0.97); }
    .pager a:active { transform: scale(0.95); }
    .tabs a:active { background: var(--accent-light); }
    article.result-card:active { background: var(--bg-subtle); }
    /* Safe area insets for notched phones */
    body {
      padding-left: max(1rem, env(safe-area-inset-left));
      padding-right: max(1rem, env(safe-area-inset-right));
      padding-bottom: max(2rem, env(safe-area-inset-bottom));
    }
  }
</style>"#
}

fn autocomplete_script() -> &'static str {
    r#"<script>
(function(){
  var inp=document.querySelector('input[name=q]'); if(!inp) return;
  var box=document.createElement('div'); box.className='ac'; box.style.display='none';
  inp.parentNode.appendChild(box); var t;
  inp.addEventListener('input',function(){
    clearTimeout(t); var q=inp.value.trim(); if(!q){box.style.display='none';return;}
    t=setTimeout(function(){
      fetch('/autocompleter?q='+encodeURIComponent(q)).then(r=>r.json()).then(function(d){
        var list=(d&&d[1])||[]; box.innerHTML='';
        if(!list.length){box.style.display='none';return;}
        list.slice(0,8).forEach(function(s){
          var el=document.createElement('div'); el.textContent=s;
          el.onclick=function(){inp.value=s; box.style.display='none'; inp.form.submit();};
          box.appendChild(el);
        }); box.style.display='block';
      }).catch(function(){box.style.display='none';});
    },140);
  });
  document.addEventListener('click',function(e){ if(e.target!==inp) box.style.display='none'; });
})();
</script>"#
}

/// A small "Theme: auto · light · dark" switcher linking to `/theme`, returning
/// to `return_to`. The current theme is shown in bold.
fn theme_tools(theme: Theme, return_to: &str) -> String {
    let to = urlencode(return_to);
    let opt = |label: &str, val: &str| {
        if theme.as_str() == val {
            format!("<strong>{label}</strong>")
        } else {
            format!(r#"<a href="/theme?set={val}&to={to}">{label}</a>"#)
        }
    };
    format!(
        r#"<span>Theme: {} · {} · {}</span>"#,
        opt("auto", "auto"),
        opt("light", "light"),
        opt("dark", "dark"),
    )
}

fn home_page(settings: &Settings, theme: Theme) -> String {
    format!(
        r#"<!doctype html>
<html lang="en"{theme_attr}><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>metasearch</title>
<link rel="search" type="application/opensearchdescription+xml" title="metasearch" href="/opensearch.xml">
{styles}</head><body>
  <h1><a href="/classic">metasearch</a></h1>
  <p class="count">Classic standard search UI. <a href="/">Try the answer UI →</a></p>
  <form class="search" action="/classic" method="get" autocomplete="off">
    <input type="search" name="q" placeholder="Search the web…  (try !w einstein, !gh ripgrep, !!g redirect, weather london, 2*21)" autofocus>
    <button type="submit">Search</button>
  </form>
  <div class="tools">
    <span>Bangs: <code>!w !wd !gh !se !arx !hn !img</code></span>
    <span>Redirect: <code>!!g !!ddg !!yt</code></span>
    <span>Lang: <code>:en :de</code></span>
  </div>
  <div class="tools">
    <a href="/preferences">Preferences</a><a href="/stats">Stats</a><a href="/config">Config</a>
    {theme_tools}
  </div>
  <div class="tools"><span>Engines: {engines}</span></div>
{script}
</body></html>"#,
        theme_attr = theme.attr(),
        styles = page_styles(),
        script = autocomplete_script(),
        theme_tools = theme_tools(theme, "/classic"),
        engines = settings
            .engines
            .iter()
            .filter(|e| e.enabled)
            .map(|e| e.name.as_str())
            .collect::<Vec<_>>()
            .join(", "),
    )
}

fn results_page(response: &SearchResponse, settings: &Settings, theme: Theme) -> String {
    let q = escape(&response.query);
    let cats = settings.categories();
    // Category tabs.
    let mut tabs = String::new();
    for c in &cats {
        tabs.push_str(&format!(
            r#"<a href="/search?q={q}&categories={c}">{c}</a>"#,
            q = q,
            c = escape(c)
        ));
    }

    // Answers (instant answers rendered as compact cards).
    let mut answers_html = String::new();
    for a in &response.answers {
        let ans = &a.answer;
        let (icon, label, class) = if a.engine == "ai" {
            (r#"<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M12 2L2 7l10 5 10-5-10-5z"/><path d="M2 17l10 5 10-5"/><path d="M2 12l10 5 10-5"/></svg>"#, "AI", "card ai")
        } else if ans.contains("KRW") || ans.contains("USD") || ans.contains("▲") || ans.contains("▼") {
            ("📈", "", "card instant stock")
        } else if ans.contains("°C") || ans.contains("°F") || ans.contains("humidity") {
            ("🌤", "", "card instant weather")
        } else if ans.contains("→") && (ans.contains("USD") || ans.contains("EUR") || ans.contains("KRW")) {
            ("💱", "", "card instant currency")
        } else if ans.contains("=") && ans.chars().any(|c| c.is_ascii_digit()) {
            ("🔢", "", "card instant calc")
        } else {
            ("💡", "", "card instant")
        };
        let url_link = a.url.as_ref().map(|u| format!(r#" <a href="{}" class="source-link" target="_blank">↗</a>"#, escape(u))).unwrap_or_default();
        answers_html.push_str(&format!(
            r#"<div class="{class}"><span class="instant-icon">{icon}</span><span class="instant-label">{label}</span><span class="instant-answer">{ans}</span>{url_link}</div>"#,
            class = class,
            icon = icon,
            label = label,
            ans = escape(ans),
            url_link = url_link,
        ));
    }

    // Infoboxes.
    let mut infobox_html = String::new();
    for ib in &response.infoboxes {
        let links = ib
            .urls
            .iter()
            .map(|u| format!(r#"<a href="{}">{}</a>"#, escape(&u.url), escape(&u.title)))
            .collect::<Vec<_>>()
            .join(" · ");
        infobox_html.push_str(&format!(
            r#"<div class="card"><h3>{title}</h3><div>{content}</div><div class="chips" style="margin-top:.75rem">{links}</div></div>"#,
            title = escape(&ib.infobox),
            content = escape(&ib.content),
            links = links,
        ));
    }

    // Results: image grid vs. list.
    let is_images = response
        .results
        .iter()
        .filter(|r| r.template == "images.html")
        .count()
        > response.results.len() / 2
        && !response.results.is_empty();

    let mut items = String::new();
    if is_images {
        items.push_str(r#"<div class="imgs">"#);
        for r in &response.results {
            let thumb = if r.thumbnail.is_empty() {
                &r.img_src
            } else {
                &r.thumbnail
            };
            let proxied = if settings.server.image_proxy && !thumb.is_empty() {
                format!("/image_proxy?url={}", urlencode(thumb))
            } else {
                thumb.clone()
            };
            items.push_str(&format!(
                r#"<a href="{url}" title="{title}"><img loading="lazy" src="{src}" alt="{title}"></a>"#,
                url = escape(&r.url),
                title = escape(&r.title),
                src = escape(&proxied),
            ));
        }
        items.push_str("</div>");
    } else {
        items.push_str(r#"<div class="results-list">"#);
        for r in &response.results {
            // Favicon handling
            let favicon_html = if r.favicon.is_empty() {
                r#"<div class="result-favicon"><div class="result-favicon-placeholder"></div></div>"#.to_string()
            } else {
                format!(
                    r#"<div class="result-favicon"><img src="{}" alt="" loading="lazy" onerror="this.parentElement.innerHTML='<div class=result-favicon-placeholder></div>'"></div>"#,
                    escape(&r.favicon)
                )
            };

            // URL display (truncated for readability)
            let url_display = r.url
                .replace("https://", "")
                .replace("http://", "")
                .replace("www.", "");
            let url_truncated: String = if url_display.len() > 60 {
                format!("{}...", url_display.chars().take(60).collect::<String>())
            } else {
                url_display
            };

            // Source badges
            let mut badges = String::new();
            for engine in &r.engines {
                let badge_class = match engine.as_str() {
                    "local" | "file" | "files" => "badge local",
                    "bing_news" | "google_news" | "duckduckgo_news" => "badge news",
                    "bing_images" | "google_images" | "duckduckgo_images" => "badge images",
                    _ => "badge web",
                };
                // Capitalize first letter
                let label: String = engine.chars().take(1).flat_map(|c| c.to_uppercase()).chain(engine.chars().skip(1)).collect();
                badges.push_str(&format!(r#"<span class="{}">{}</span>"#, badge_class, escape(&label)));
            }

            // Summary
            let summary = match &r.summary {
                Some(s) if !s.is_empty() => {
                    format!(r#"<p class="summary">{}</p>"#, escape(s))
                }
                _ => String::new(),
            };

            // Highlights
            let highlights = if r.highlights.is_empty() {
                String::new()
            } else {
                let chips = r
                    .highlights
                    .iter()
                    .map(|h| format!(r#"<span class="hl">{}</span>"#, escape(h)))
                    .collect::<Vec<_>>()
                    .join("");
                format!(r#"<div class="hls">{chips}</div>"#)
            };

            // Score bar (visual representation)
            let score_percent = (r.score * 100.0).min(100.0);

            // Cluster info
            let cluster_info = match r.cluster {
                Some(c) => format!(r#" <span style="opacity:0.6">· cluster {c}</span>"#),
                None => String::new(),
            };

            items.push_str(&format!(
                r#"<article class="result-card">
  <div class="result-header">
    {favicon}
    <div class="result-title-section">
      <a class="title" href="{url}">{title}</a>
      <div class="url"><span class="url-text">{url_disp}</span></div>
    </div>
  </div>
  {summary}
  <p class="content">{content}</p>
  {highlights}
  <div class="result-footer">
    <div class="source-badges">{badges}</div>
    <div class="meta">
      <div class="score-bar"><div class="score-fill" style="width:{score_pct}%"></div></div>
      <span>{score:.2}{cluster}</span>
    </div>
  </div>
</article>"#,
                favicon = favicon_html,
                url = escape(&r.url),
                url_disp = escape(&url_truncated),
                title = escape(&r.title),
                content = escape(&r.content),
                summary = summary,
                highlights = highlights,
                badges = badges,
                score = r.score,
                score_pct = score_percent,
                cluster = cluster_info,
            ));
        }
        items.push_str("</div>");
    }
    if response.results.is_empty() {
        items.push_str(r#"<div class="card"><p style="text-align:center;color:var(--muted)">No results found. Try a different search term.</p></div>"#);
    }

    // Suggestions / corrections.
    let mut chips = String::new();
    if !response.corrections.is_empty() {
        chips.push_str("<div class=\"chips\">Did you mean: ");
        for c in &response.corrections {
            chips.push_str(&format!(
                r#"<a href="/search?q={}">{}</a>"#,
                urlencode(c),
                escape(c)
            ));
        }
        chips.push_str("</div>");
    }
    if !response.suggestions.is_empty() {
        chips.push_str("<div class=\"chips\">Related: ");
        for s in response.suggestions.iter().take(8) {
            chips.push_str(&format!(
                r#"<a href="/search?q={}">{}</a>"#,
                urlencode(s),
                escape(s)
            ));
        }
        chips.push_str("</div>");
    }

    let unresponsive = if response.unresponsive_engines.is_empty() {
        String::new()
    } else {
        let list = response
            .unresponsive_engines
            .iter()
            .map(|(name, reason)| format!("{} ({})", escape(name), escape(reason)))
            .collect::<Vec<_>>()
            .join(", ");
        format!(r#"<div class="warn">Unresponsive engines: {list}</div>"#)
    };

    // Pagination with page numbers
    let q_enc = urlencode(&response.query);
    let current = response.pageno;
    let has_results = !response.results.is_empty();
    let has_more = response.results.len() >= settings.server.max_results_per_engine;
    let mut pager_html = String::new();

    if has_results {
        // Previous button
        if current > 1 {
            pager_html.push_str(&format!(
                r#"<a href="/search?q={}&pageno={}" class="pg-prev">←</a>"#,
                q_enc, current - 1
            ));
        }

        // Page numbers (show up to 5 pages around current)
        let start = if current <= 3 { 1 } else { current - 2 };
        let end = if has_more { start + 4 } else { current };

        for p in start..=end {
            if p == current {
                pager_html.push_str(&format!(r#"<span class="pg-cur">{}</span>"#, p));
            } else {
                pager_html.push_str(&format!(
                    r#"<a href="/search?q={}&pageno={}">{}</a>"#,
                    q_enc, p, p
                ));
            }
        }

        // Next button (only if likely more results)
        if has_more {
            pager_html.push_str(&format!(
                r#"<a href="/search?q={}&pageno={}" class="pg-next">→</a>"#,
                q_enc, current + 1
            ));
        }
    }

    let return_to = format!("/search?q={}", urlencode(&response.query));
    let exports = format!(
        r#"<div class="tools">Export:
        <a href="/search?q={q}&format=json">JSON</a>
        <a href="/search?q={q}&format=rss">RSS</a>
        <a href="/search?q={q}&format=csv">CSV</a>
        <a href="/preferences">Preferences</a>
        <a href="/stats">Stats</a>
        {theme_tools}</div>"#,
        q = urlencode(&response.query),
        theme_tools = theme_tools(theme, &return_to),
    );

    format!(
        r#"<!doctype html>
<html lang="en"{theme_attr}><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{q} — metasearch</title>
<link rel="search" type="application/opensearchdescription+xml" title="metasearch" href="/opensearch.xml">
{styles}</head><body>
  <h1><a href="/">metasearch</a></h1>
  <form class="search" action="/search" method="get" autocomplete="off">
    <input type="search" name="q" value="{q}">
    <button type="submit">Search</button>
  </form>
  <div class="tabs">{tabs}</div>
  {unresponsive}
  {chips}
  {answers}
  {infoboxes}
  <div class="count">{count} results · page {page}</div>
  {items}
  <div class="pager">{pager}</div>
  {exports}
{script}
</body></html>"#,
        q = q,
        theme_attr = theme.attr(),
        styles = page_styles(),
        tabs = tabs,
        unresponsive = unresponsive,
        chips = chips,
        answers = answers_html,
        infoboxes = infobox_html,
        count = response.number_of_results,
        page = response.pageno,
        items = items,
        pager = pager_html,
        exports = exports,
        script = autocomplete_script(),
    )
}

fn preferences_page(settings: &Settings, saved: bool, theme: Theme) -> String {
    // Server-side, persisted preferences. The form POSTs back to /preferences,
    // which validates and writes settings.yml (see `save_preferences`). The
    // standalone server and any other reader share the same settings file.

    // Group engines by category for collapsible sections
    // Organized by usage frequency and logical grouping
    let categories_order = [
        // Primary search
        ("general", "🔍 General"),
        ("news", "📰 News"),
        ("images", "🖼️ Images"),
        ("videos", "🎬 Videos"),
        // Knowledge & Learning
        ("science", "🔬 Science / Academic"),
        ("it", "💻 IT / Development"),
        ("tech", "🚀 Tech"),
        ("books", "📚 Books"),
        ("dictionary", "📖 Dictionary"),
        ("howto", "❓ How-To"),
        // Lifestyle
        ("social", "👥 Social"),
        ("music", "🎵 Music"),
        ("games", "🎮 Games"),
        ("shopping", "🛒 Shopping"),
        ("jobs", "💼 Jobs"),
        ("finance", "💰 Finance"),
        // Utilities
        ("map", "🗺️ Maps"),
        ("files", "📁 Files"),
        ("other", "📦 Other"),
    ];

    // Engine display info: (name, enabled, weight)
    struct EngineInfo<'a> {
        name: &'a str,
        enabled: bool,
        weight: f64,
    }

    let mut categorized: std::collections::HashMap<&str, Vec<EngineInfo>> =
        std::collections::HashMap::new();

    // Include both built-in engines and custom engines
    for e in &settings.engines {
        for cat in &e.categories {
            categorized
                .entry(cat.as_str())
                .or_default()
                .push(EngineInfo { name: &e.name, enabled: e.enabled, weight: e.weight });
        }
    }
    for e in &settings.custom_engines {
        let cats: Vec<&str> = if e.categories.is_empty() {
            vec!["other"]
        } else {
            e.categories.iter().map(|s| s.as_str()).collect()
        };
        for cat in cats {
            categorized
                .entry(cat)
                .or_default()
                .push(EngineInfo { name: &e.name, enabled: e.enabled, weight: e.weight });
        }
    }

    let mut engine_sections = String::new();
    for (cat_key, cat_label) in &categories_order {
        if let Some(engines) = categorized.get(*cat_key) {
            let rows: String = engines
                .iter()
                .map(|e| {
                    let checked = if e.enabled { " checked" } else { "" };
                    format!(
                        r#"<tr><td><label class="eng"><input type="checkbox" name="en_{name}"{checked}> {name}</label></td><td><input type="number" name="wt_{name}" value="{w}" step="0.1" min="0" class="weight-input"></td></tr>"#,
                        name = escape(e.name),
                        checked = checked,
                        w = e.weight,
                    )
                })
                .collect();
            let open = if *cat_key == "general" || *cat_key == "news" { " open" } else { "" };
            engine_sections.push_str(&format!(
                r#"<details class="engine-group"{open}>
                <summary><span class="cat-badge">{count}</span> {label}</summary>
                <p class="cat-desc">{desc}</p>
                <table><tr><th>Engine</th><th>Weight</th></tr>{rows}</table>
                </details>"#,
                open = open,
                label = cat_label,
                count = engines.len(),
                desc = category_description(*cat_key),
                rows = rows,
            ));
        }
    }

    let ss = settings.search.safe_search;
    let sel = |v: u8| if ss == v { " selected" } else { "" };
    let banner = if saved {
        r#"<div class="saved-banner">Saved — written to settings.yml and applied to new searches.</div>"#
    } else {
        ""
    };

    let app_name = escape(&settings.branding.app_name);
    let logo_url = settings
        .branding
        .logo_url
        .as_deref()
        .map(escape)
        .unwrap_or_default();
    let favicon_url = settings
        .branding
        .favicon_url
        .as_deref()
        .map(escape)
        .unwrap_or_default();

    format!(
        r##"<!doctype html>
<html lang="en"{theme_attr}><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Preferences — {app_name}</title>{styles}
<style>
:root {{
  --bg: #fafafa; --surface: #ffffff; --fg: #18181b; --muted: #71717a;
  --accent: #0d9488; --accent-soft: rgba(13,148,136,.08); --border: #e4e4e7;
  --radius: 12px; --shadow: 0 1px 3px rgba(0,0,0,.06);
}}
html[data-theme="dark"] {{
  --bg: #09090b; --surface: #18181b; --fg: #fafafa; --muted: #a1a1aa;
  --accent: #2dd4bf; --accent-soft: rgba(45,212,191,.1); --border: #27272a;
}}
@media (prefers-color-scheme: dark) {{
  :root:not([data-theme="light"]) {{
    --bg: #09090b; --surface: #18181b; --fg: #fafafa; --muted: #a1a1aa;
    --accent: #2dd4bf; --accent-soft: rgba(45,212,191,.1); --border: #27272a;
  }}
}}
*, *::before, *::after {{ box-sizing: border-box; }}
html {{ scroll-behavior: smooth; }}
body {{ margin: 0; font-family: system-ui, -apple-system, sans-serif; background: var(--bg); color: var(--fg); line-height: 1.6; }}
a {{ color: var(--accent); text-decoration: none; }}
a:hover {{ text-decoration: underline; }}
.container {{ width: 100%; padding: 1.5rem 3rem; box-sizing: border-box; }}
header {{ display: flex; align-items: center; gap: .75rem; margin-bottom: 1.5rem; }}
header .logo {{ font-weight: 600; font-size: 1.25rem; color: inherit; text-decoration: none; display: flex; align-items: center; gap: .5rem; }}
header .logo svg {{ width: 28px; height: 28px; }}
.saved-banner {{ background: var(--accent-soft); color: var(--accent); border: 1px solid var(--accent); border-radius: var(--radius); padding: .875rem 1rem; margin-bottom: 1.25rem; font-weight: 500; }}

/* Sidebar layout */
.pref-layout {{ display: grid; grid-template-columns: 200px 1fr; gap: 1.5rem; }}
.pref-sidebar {{ position: sticky; top: 1rem; height: fit-content; }}
.pref-sidebar a {{ display: block; padding: .5rem .75rem; border-radius: 6px; color: var(--muted); text-decoration: none; font-size: .875rem; }}
.pref-sidebar a:hover, .pref-sidebar a.active {{ background: var(--accent-soft); color: var(--accent); text-decoration: none; }}
.pref-main {{ min-width: 0; }}
@media (max-width: 768px) {{
  .pref-layout {{ grid-template-columns: 1fr; }}
  .pref-sidebar {{ position: static; display: flex; flex-wrap: wrap; gap: .25rem; margin-bottom: 1rem; }}
  .pref-sidebar a {{ padding: .35rem .6rem; font-size: .8125rem; }}
}}

.section {{ background: var(--surface); border: 1px solid var(--border); border-radius: var(--radius); padding: 1.25rem; margin-bottom: 1rem; box-shadow: var(--shadow); }}
.section h2 {{ margin: 0 0 .75rem; font-size: 1rem; font-weight: 600; }}
.section-desc {{ font-size: .875rem; color: var(--muted); margin: 0 0 1rem; }}
.field {{ display: flex; flex-direction: column; gap: .35rem; margin-bottom: 1rem; }}
.field:last-child {{ margin-bottom: 0; }}
.field label {{ font-size: .875rem; font-weight: 500; }}
.field input, .field select {{ padding: .5rem .75rem; border: 1px solid var(--border); border-radius: 8px; background: var(--bg); color: inherit; font-size: .875rem; }}
.field input:focus, .field select:focus {{ outline: 2px solid var(--accent); outline-offset: -1px; }}
.field small {{ font-size: .75rem; color: var(--muted); }}
.engine-group {{ border: 1px solid var(--border); border-radius: var(--radius); margin-bottom: .5rem; overflow: hidden; }}
.engine-group summary {{ padding: .75rem 1rem; cursor: pointer; font-weight: 500; display: flex; align-items: center; gap: .5rem; background: var(--surface); }}
.engine-group summary:hover {{ background: var(--accent-soft); }}
.engine-group[open] summary {{ border-bottom: 1px solid var(--border); }}
.cat-badge {{ display: inline-flex; align-items: center; justify-content: center; min-width: 1.5rem; height: 1.5rem; background: var(--accent); color: #fff; border-radius: 999px; font-size: .75rem; font-weight: 600; }}
.cat-desc {{ font-size: .8125rem; color: var(--muted); margin: .75rem 1rem .5rem; }}
.engine-group table {{ width: 100%; border-collapse: collapse; font-size: .85rem; margin: 0; }}
.engine-group td, .engine-group th {{ padding: .5rem 1rem; text-align: left; border-bottom: 1px solid var(--border); }}
.engine-group tr:last-child td {{ border-bottom: none; }}
.engine-group th {{ font-size: .75rem; text-transform: uppercase; letter-spacing: .05em; color: var(--muted); font-weight: 500; }}
.eng {{ display: flex; align-items: center; gap: .5rem; cursor: pointer; }}
.eng input {{ accent-color: var(--accent); }}
.weight-input {{ width: 4.5rem; padding: .35rem .5rem; border: 1px solid var(--border); border-radius: 6px; background: var(--bg); color: inherit; font-size: .8125rem; }}
.btn {{ background: var(--accent); color: #fff; border: none; padding: .75rem 1.5rem; border-radius: 8px; font-size: .9375rem; font-weight: 500; cursor: pointer; transition: opacity .15s; }}
.btn:hover {{ opacity: .9; }}
.tools {{ display: flex; gap: 1rem; flex-wrap: wrap; font-size: .8125rem; color: var(--muted); margin-top: 1.5rem; }}
.theme-grid {{ display: grid; grid-template-columns: repeat(auto-fill, minmax(120px, 1fr)); gap: .5rem; }}
.theme-option {{ display: flex; align-items: center; gap: .5rem; padding: .5rem .75rem; border: 1px solid var(--border); border-radius: 8px; cursor: pointer; transition: all .15s; }}
.theme-option:hover {{ border-color: var(--accent); background: var(--accent-soft); }}
.theme-option input {{ accent-color: var(--accent); }}
.checkbox-label {{ display: flex; align-items: center; gap: .5rem; cursor: pointer; }}
.checkbox-label input {{ accent-color: var(--accent); width: 1.125rem; height: 1.125rem; }}
.btn-secondary {{ background: var(--surface); color: var(--fg); border: 1px solid var(--border); padding: .5rem 1rem; border-radius: 6px; font-size: .8125rem; cursor: pointer; transition: all .15s; }}
.btn-secondary:hover {{ background: var(--accent-soft); border-color: var(--accent); }}
.shortcuts-table {{ width: 100%; font-size: .875rem; }}
.shortcuts-table td {{ padding: .5rem 0; }}
.shortcuts-table td:first-child {{ width: 40%; color: var(--muted); }}
.shortcuts-table kbd {{ display: inline-block; padding: .125rem .375rem; background: var(--surface); border: 1px solid var(--border); border-radius: 4px; font-family: inherit; font-size: .75rem; box-shadow: 0 1px 0 var(--border); }}
.provider-grid {{ display: grid; grid-template-columns: repeat(auto-fill, minmax(130px, 1fr)); gap: .5rem; }}
.provider-option {{ display: flex; align-items: center; gap: .5rem; padding: .5rem .75rem; border: 1px solid var(--border); border-radius: 8px; cursor: pointer; transition: all .15s; font-size: .875rem; }}
.provider-option:hover {{ border-color: var(--accent); background: var(--accent-soft); }}
.provider-option.selected {{ border-color: var(--accent); background: var(--accent-soft); }}
.provider-option input {{ accent-color: var(--accent); }}
.field-group {{ border: 1px solid var(--border); border-radius: 8px; padding: 0; margin: .5rem 0; }}
.field-group summary {{ padding: .75rem 1rem; cursor: pointer; font-weight: 500; font-size: .875rem; }}
.field-group summary:hover {{ background: var(--accent-soft); }}
.field-group[open] summary {{ border-bottom: 1px solid var(--border); }}
.field-group .field {{ padding: .5rem 1rem; margin: 0; }}
.lang-quick {{ display: flex; flex-wrap: wrap; gap: .5rem; margin-bottom: .5rem; }}
.lang-chip {{ padding: .625rem 1rem; border: 2px solid var(--border); border-radius: 10px; background: var(--surface); font-size: .9375rem; cursor: pointer; transition: all .15s; }}
.lang-chip:hover {{ border-color: var(--accent); background: var(--accent-soft); }}
.lang-chip.active {{ border-color: var(--accent); background: var(--accent); color: #fff; }}
</style>
</head><body>
<div class="container">
  <header style="display:flex;justify-content:space-between;align-items:center;">
    <a class="logo" href="/"><svg viewBox="0 0 64 64" fill="none"><circle cx="32" cy="32" r="28" stroke="currentColor" stroke-width="4"/><circle cx="32" cy="32" r="12" fill="currentColor"/></svg>{app_name}</a>
    <a href="/preferences/logout" style="font-size:.875rem;color:var(--muted);text-decoration:none;">Logout</a>
  </header>
  {banner}
  <form action="/preferences" method="post">
    <div class="pref-layout">
      <nav class="pref-sidebar">
        <a href="#branding" data-i18n="preferences.branding">Branding</a>
        <a href="#appearance" data-i18n="preferences.appearance">Appearance</a>
        <a href="#language" data-i18n="preferences.language">Language</a>
        <a href="#search" data-i18n="preferences.search">Search</a>
        <a href="#ai" data-i18n="preferences.ai">AI</a>
        <a href="#news" data-i18n="preferences.news">News</a>
        <a href="#engines" data-i18n="preferences.engines">Engines</a>
        <a href="#privacy" data-i18n="preferences.privacy">Privacy</a>
        <a href="#shortcuts" data-i18n="preferences.shortcuts">Shortcuts</a>
        <a href="#server" data-i18n="preferences.server">Server</a>
        <a href="#advanced" data-i18n="preferences.advanced">Advanced</a>
      </nav>
      <main class="pref-main">
        <section id="branding" class="section">
          <h2 data-i18n="preferences.branding">Branding</h2>
          <p class="section-desc" data-i18n="preferences.branding_desc">Customize the application name, logo, and favicon displayed in the UI.</p>
          <div class="field">
            <label for="app_name" data-i18n="preferences.app_name">Application Name</label>
            <input type="text" id="app_name" name="app_name" value="{app_name}" placeholder="Orgos">
            <small data-i18n="preferences.app_name_hint">Name shown in the header and page titles</small>
          </div>
          <div class="field">
            <label for="logo_url" data-i18n="preferences.logo_url">Logo URL</label>
            <input type="text" id="logo_url" name="logo_url" value="{logo_url}" placeholder="/static/logo.png">
            <small data-i18n="preferences.logo_url_hint">URL or path to your logo image (leave empty for default)</small>
          </div>
          <div class="field">
            <label for="favicon_url" data-i18n="preferences.favicon_url">Favicon URL</label>
            <input type="text" id="favicon_url" name="favicon_url" value="{favicon_url}" placeholder="/static/favicon.ico">
            <small data-i18n="preferences.favicon_url_hint">URL or path to your favicon (leave empty for browser default)</small>
          </div>
        </section>

        <section id="appearance" class="section">
          <h2 data-i18n="preferences.appearance">Appearance</h2>
          <p class="section-desc" data-i18n="preferences.appearance_desc">Customize the visual style and layout.</p>
          <div class="field">
            <label data-i18n="preferences.theme">Theme</label>
            <div class="theme-grid">
              <label class="theme-option"><input type="radio" name="theme" value="auto" checked> <span>🌓 Auto</span></label>
              <label class="theme-option"><input type="radio" name="theme" value="light"> <span>☀️ Light</span></label>
              <label class="theme-option"><input type="radio" name="theme" value="dark"> <span>🌙 Dark</span></label>
              <label class="theme-option"><input type="radio" name="theme" value="nord"> <span>❄️ Nord</span></label>
              <label class="theme-option"><input type="radio" name="theme" value="dracula"> <span>🧛 Dracula</span></label>
              <label class="theme-option"><input type="radio" name="theme" value="solarized"> <span>🌅 Solarized</span></label>
            </div>
          </div>
          <div class="field">
            <label data-i18n="preferences.font_size">Font Size</label>
            <select name="font_size">
              <option value="small">Small</option>
              <option value="medium" selected>Medium</option>
              <option value="large">Large</option>
            </select>
          </div>
          <div class="field">
            <label data-i18n="preferences.results_layout">Results Layout</label>
            <select name="results_layout">
              <option value="list" selected>List</option>
              <option value="compact">Compact</option>
              <option value="cards">Cards</option>
            </select>
          </div>
        </section>

        <section id="language" class="section">
          <h2 data-i18n="preferences.language">🌐 Language</h2>
          <p class="section-desc" data-i18n="preferences.language_desc">Select your preferred interface language</p>

          <div class="field">
            <label for="uiLangSelect" data-i18n="preferences.ui_language">Interface Language</label>
            <select id="uiLangSelect" style="width:100%;font-size:1rem;padding:.75rem;">
              <option value="en">English</option>
              <option value="ko">한국어</option>
              <option value="ja">日本語</option>
              <option value="zh">中文</option>
              <option value="es">Español</option>
              <option value="fr">Français</option>
              <option value="de">Deutsch</option>
              <option value="it">Italiano</option>
              <option value="pt">Português</option>
              <option value="ru">Русский</option>
              <option value="ar">العربية</option>
              <option value="hi">हिन्दी</option>
              <option value="vi">Tiếng Việt</option>
              <option value="id">Indonesia</option>
              <option value="ms">Melayu</option>
              <option value="tl">Filipino</option>
              <option value="th">ไทย</option>
              <option value="sv">Svenska</option>
              <option value="no">Norsk</option>
              <option value="da">Dansk</option>
              <option value="fi">Suomi</option>
              <option value="nl">Nederlands</option>
              <option value="pl">Polski</option>
              <option value="el">Ελληνικά</option>
              <option value="he">עברית</option>
              <option value="cs">Čeština</option>
              <option value="hu">Magyar</option>
              <option value="ro">Română</option>
              <option value="uk">Українська</option>
              <option value="tr">Türkçe</option>
              <option value="af">Afrikaans</option>
              <option value="bg">Български</option>
              <option value="ca">Català</option>
              <option value="et">Eesti</option>
              <option value="eu">Euskara</option>
              <option value="fa">فارسی</option>
              <option value="gl">Galego</option>
              <option value="hr">Hrvatski</option>
              <option value="hy">Հայdelays</option>
              <option value="is">Íslenska</option>
              <option value="ka">ქართული</option>
              <option value="kk">Қазақ</option>
              <option value="km">ខ្មែរ</option>
              <option value="lt">Lietuvių</option>
              <option value="lv">Latviešu</option>
              <option value="mk">Македонски</option>
              <option value="ml">മലയാളം</option>
              <option value="mn">Монгол</option>
              <option value="mr">मराठी</option>
              <option value="my">မြန်မာ</option>
              <option value="ne">नेपाली</option>
              <option value="pa">ਪੰਜਾਬੀ</option>
              <option value="si">සිංහල</option>
              <option value="sk">Slovenčina</option>
              <option value="sl">Slovenščina</option>
              <option value="sq">Shqip</option>
              <option value="sr">Srpski</option>
              <option value="sw">Kiswahili</option>
              <option value="ta">தமிழ்</option>
              <option value="te">తెలుగు</option>
              <option value="ur">اردو</option>
              <option value="uz">Oʻzbekcha</option>
            </select>
          </div>
          <p style="margin-top:1rem;font-size:.8125rem;color:var(--muted);" data-i18n="preferences.language_storage">Interface language is stored in your browser.</p>
        </section>

        <section id="search-lang" class="section">
          <h2>Search Settings</h2>
          <p class="section-desc">Configure search language detection.</p>
          <div class="field">
            <label for="default_language" data-i18n="preferences.search_language">Search Language</label>
            <input type="text" id="default_language" name="default_language" value="{search_lang}" placeholder="auto">
            <small data-i18n="preferences.search_language_hint">auto = detect from query (Hangul -&gt; ko-KR); or all, or a fixed locale like ko-KR</small>
          </div>
          <div class="field">
            <label for="default_lang" data-i18n="preferences.fallback_language">Fallback Language Code</label>
            <input type="text" id="default_lang" name="default_lang" value="{lang}">
            <small data-i18n="preferences.fallback_language_hint">Used when auto-detection is inconclusive</small>
          </div>
        </section>

        <section id="search" class="section">
          <h2 data-i18n="preferences.search">Search</h2>
          <p class="section-desc" data-i18n="preferences.search_desc">Default settings applied to all searches.</p>
          <div class="field">
            <label for="safe_search" data-i18n="preferences.safe_search">Safe Search</label>
            <select id="safe_search" name="safe_search">
              <option value="0"{s0} data-i18n="preferences.safe_off">Off</option>
              <option value="1"{s1} data-i18n="preferences.safe_moderate">Moderate</option>
              <option value="2"{s2} data-i18n="preferences.safe_strict">Strict</option>
            </select>
          </div>
          <div class="field">
            <label for="results_per_page" data-i18n="preferences.results_per_engine">Results Per Engine</label>
            <input type="number" id="results_per_page" name="results_per_page" value="{rpp}" min="1" max="50">
          </div>
        </section>

        <section id="ai" class="section">
          <h2 data-i18n="preferences.ai">AI</h2>
          <p class="section-desc" data-i18n="preferences.ai_desc">Configure AI models for answer synthesis and article rewriting.</p>
          <div class="field">
            <label><input type="checkbox" name="ai_enabled"{ai_enabled_chk}> Enable AI features</label>
          </div>
          <div class="field">
            <label>Provider</label>
            <div class="provider-grid">
              <label class="provider-option{provider_local_sel}"><input type="radio" name="ai_provider" value="local"{provider_local_chk}> <span>🖥️ Local (Ollama)</span></label>
              <label class="provider-option{provider_openai_sel}"><input type="radio" name="ai_provider" value="openai"{provider_openai_chk}> <span>OpenAI</span></label>
              <label class="provider-option{provider_anthropic_sel}"><input type="radio" name="ai_provider" value="anthropic"{provider_anthropic_chk}> <span>Anthropic</span></label>
              <label class="provider-option{provider_groq_sel}"><input type="radio" name="ai_provider" value="groq"{provider_groq_chk}> <span>Groq</span></label>
              <label class="provider-option{provider_together_sel}"><input type="radio" name="ai_provider" value="together"{provider_together_chk}> <span>Together</span></label>
              <label class="provider-option{provider_custom_sel}"><input type="radio" name="ai_provider" value="custom"{provider_custom_chk}> <span>⚙️ Custom</span></label>
            </div>
          </div>
          <div class="field">
            <label for="ai_base_url">API Base URL</label>
            <input type="text" id="ai_base_url" name="ai_base_url" value="{ai_base_url}" placeholder="http://127.0.0.1:11434">
            <small>Local: http://127.0.0.1:11434 | OpenAI: https://api.openai.com/v1 | Anthropic: https://api.anthropic.com</small>
          </div>
          <div class="field">
            <label for="ai_api_key">API Key</label>
            <input type="password" id="ai_api_key" name="ai_api_key" value="{ai_api_key}" placeholder="sk-... (leave empty for local Ollama)">
            <small>Required for cloud providers (OpenAI, Anthropic, Groq, etc.)</small>
          </div>
          <div class="field">
            <label for="ai_model" data-i18n="preferences.ai_model">Model</label>
            <input type="text" id="ai_model" name="ai_model" value="{ai_model}" placeholder="gemma3:4b / gpt-4o-mini / claude-sonnet-4-20250514">
            <small data-i18n="preferences.ai_model_hint">Chat/instruct model for answer synthesis and query expansion</small>
          </div>
          <div class="field">
            <label for="ai_article_model" data-i18n="preferences.ai_article_model">Article Model</label>
            <input type="text" id="ai_article_model" name="ai_article_model" value="{ai_article_model}" placeholder="gemma4:e4b">
            <small data-i18n="preferences.ai_article_model_hint">Model for full-page news article rewrites (defaults to main model)</small>
          </div>
          <div class="field">
            <label for="ai_embedding_model" data-i18n="preferences.ai_embedding_model">Embedding Model</label>
            <input type="text" id="ai_embedding_model" name="ai_embedding_model" value="{ai_embedding_model}" placeholder="nomic-embed-text">
          </div>
          <div class="field">
            <label for="ai_vision_model" data-i18n="preferences.ai_vision_model">Vision Model</label>
            <input type="text" id="ai_vision_model" name="ai_vision_model" value="{ai_vision_model}" placeholder="llava">
          </div>
          <div class="field">
            <label for="ai_answer_top_n" data-i18n="preferences.ai_answer_top_n">Answer Top N</label>
            <input type="number" id="ai_answer_top_n" name="ai_answer_top_n" value="{ai_answer_top_n}" min="1" max="20">
            <small data-i18n="preferences.ai_answer_top_n_hint">Number of top results fed into answer synthesis</small>
          </div>
          <div class="field">
            <label for="ai_timeout_secs" data-i18n="preferences.ai_timeout">Timeout (seconds)</label>
            <input type="number" id="ai_timeout_secs" name="ai_timeout_secs" value="{ai_timeout_secs}" min="1" max="300">
          </div>
          <details class="field-group">
            <summary data-i18n="preferences.ai_cost_tracking">💰 Cost Tracking</summary>
            <div class="field">
              <label><input type="checkbox" name="ai_track_usage"{ai_track_usage_chk}> <span data-i18n="preferences.ai_track_usage">Track token usage</span></label>
              <small data-i18n="preferences.ai_track_usage_hint">Display token count and estimated cost for AI responses</small>
            </div>
            <div class="field">
              <label for="ai_input_cost" data-i18n="preferences.ai_input_cost">Input cost ($/1M tokens)</label>
              <input type="number" id="ai_input_cost" name="ai_input_cost" value="{ai_input_cost}" step="0.01" min="0" placeholder="2.50">
              <small>GPT-4o: 2.50 | Claude 3.5: 3.00 | Gemini 1.5: 1.25</small>
            </div>
            <div class="field">
              <label for="ai_output_cost" data-i18n="preferences.ai_output_cost">Output cost ($/1M tokens)</label>
              <input type="number" id="ai_output_cost" name="ai_output_cost" value="{ai_output_cost}" step="0.01" min="0" placeholder="10.00">
              <small>GPT-4o: 10.00 | Claude 3.5: 15.00 | Gemini 1.5: 5.00</small>
            </div>
          </details>
          <div class="field">
            <label for="ai_chat_retention_days" data-i18n="preferences.ai_chat_retention">Chat History Retention (days)</label>
            <input type="number" id="ai_chat_retention_days" name="ai_chat_retention_days" value="{ai_chat_retention_days}" min="0" max="365" placeholder="30">
            <small data-i18n="preferences.ai_chat_retention_hint">Days to keep chat history (0 = no limit). Older conversations will be automatically deleted.</small>
          </div>
          <div class="field" style="display:flex;flex-wrap:wrap;gap:1rem;">
            <label><input type="checkbox" name="ai_answer"{ai_answer_chk}> <span data-i18n="preferences.ai_answer_synthesis">Answer synthesis</span></label>
            <label><input type="checkbox" name="ai_expand"{ai_expand_chk}> <span data-i18n="preferences.ai_query_expansion">Query expansion</span></label>
            <label><input type="checkbox" name="ai_rerank"{ai_rerank_chk}> <span data-i18n="preferences.ai_reranking">Re-ranking</span></label>
            <label><input type="checkbox" name="ai_cluster"{ai_cluster_chk}> <span data-i18n="preferences.ai_clustering">Clustering</span></label>
            <label><input type="checkbox" name="ai_vision"{ai_vision_chk}> <span data-i18n="preferences.ai_vision">Vision</span></label>
          </div>
          <div class="field">
            <label for="ai_news_prompt_ko" data-i18n="preferences.ai_news_prompt_ko">Korean News Analysis Prompt</label>
            <textarea id="ai_news_prompt_ko" name="ai_news_prompt_ko" rows="6" data-i18n-placeholder="preferences.ai_news_prompt_ko_placeholder" placeholder="Leave empty for default prompt. Use {{title}} and {{excerpt}} placeholders.">{ai_news_prompt_ko}</textarea>
            <small data-i18n="preferences.ai_news_prompt_ko_hint">Custom prompt for Korean news article analysis</small>
          </div>
          <div class="field">
            <label for="ai_news_prompt_en" data-i18n="preferences.ai_news_prompt_en">English News Analysis Prompt</label>
            <textarea id="ai_news_prompt_en" name="ai_news_prompt_en" rows="6" data-i18n-placeholder="preferences.ai_news_prompt_en_placeholder" placeholder="Leave empty for default prompt. Use {{title}} and {{excerpt}} placeholders.">{ai_news_prompt_en}</textarea>
            <small data-i18n="preferences.ai_news_prompt_en_hint">Custom prompt for English news article analysis</small>
          </div>
          <div class="field">
            <label for="ai_answer_language" data-i18n="preferences.ai_answer_language">Answer Language</label>
            <select id="ai_answer_language" name="ai_answer_language">
              <option value="auto"{answer_lang_auto} data-i18n="preferences.ai_answer_auto">Auto-detect</option>
              <option value="en"{answer_lang_en}>English</option>
              <option value="ko"{answer_lang_ko}>한국어</option>
              <option value="ja"{answer_lang_ja}>日本語</option>
              <option value="zh"{answer_lang_zh}>中文</option>
              <option value="es"{answer_lang_es}>Español</option>
              <option value="fr"{answer_lang_fr}>Français</option>
              <option value="de"{answer_lang_de}>Deutsch</option>
              <option value="pt"{answer_lang_pt}>Português</option>
              <option value="it"{answer_lang_it}>Italiano</option>
              <option value="ru"{answer_lang_ru}>Русский</option>
              <option value="vi"{answer_lang_vi}>Tiếng Việt</option>
              <option value="th"{answer_lang_th}>ไทย</option>
              <option value="ar"{answer_lang_ar}>العربية</option>
            </select>
            <small data-i18n="preferences.ai_answer_language_hint">Target language for AI-generated responses (news rewrite, answers)</small>
          </div>
        </section>

        <section id="news" class="section">
          <h2 data-i18n="preferences.news">News</h2>
          <p class="section-desc" data-i18n="preferences.news_desc">News aggregation and article settings.</p>
          <div class="field">
            <label for="news_per_source_cap" data-i18n="preferences.news_per_source">Articles per source</label>
            <input type="number" id="news_per_source_cap" name="news_per_source_cap" value="{news_per_source_cap}" min="0" max="20">
          </div>
          <div class="field">
            <label for="news_freshness_half_life" data-i18n="preferences.news_freshness">Freshness half-life (hours)</label>
            <input type="number" id="news_freshness_half_life" name="news_freshness_half_life" value="{news_freshness_half_life}" step="0.5" min="1" max="168">
          </div>
          <div class="field">
            <label for="news_freshness_weight" data-i18n="preferences.news_freshness_weight">Freshness weight</label>
            <input type="number" id="news_freshness_weight" name="news_freshness_weight" value="{news_freshness_weight}" step="0.1" min="0" max="1">
          </div>
          <div class="field">
            <label for="news_dedup_similarity" data-i18n="preferences.news_dedup">Dedup similarity threshold</label>
            <input type="number" id="news_dedup_similarity" name="news_dedup_similarity" value="{news_dedup_similarity}" step="0.1" min="0" max="1">
          </div>
          <div class="field">
            <label for="news_max_age_days" data-i18n="preferences.news_max_age">Max age (days)</label>
            <input type="number" id="news_max_age_days" name="news_max_age_days" value="{news_max_age_days}" min="0" max="90">
          </div>
          <div class="field">
            <label for="news_cache_ttl" data-i18n="preferences.news_cache_ttl">Cache TTL (seconds)</label>
            <input type="number" id="news_cache_ttl" name="news_cache_ttl" value="{news_cache_ttl}" min="0" max="3600">
          </div>
          <div class="field">
            <label for="news_enrich_max" data-i18n="preferences.news_enrich_max">Max articles to enrich</label>
            <input type="number" id="news_enrich_max" name="news_enrich_max" value="{news_enrich_max}" min="0" max="50">
          </div>
          <details class="field-group" open>
            <summary data-i18n="preferences.discover_settings">📰 Discover Settings</summary>
            <div class="field">
              <label data-i18n="preferences.discover_categories">Categories to show</label>
              <div class="category-grid" style="display:grid;grid-template-columns:repeat(auto-fill,minmax(140px,1fr));gap:.5rem;margin-top:.5rem;">
                <label class="checkbox-label"><input type="checkbox" name="discover_cat_news"{discover_cat_news}> News</label>
                <label class="checkbox-label"><input type="checkbox" name="discover_cat_politics"{discover_cat_politics}> Politics</label>
                <label class="checkbox-label"><input type="checkbox" name="discover_cat_business"{discover_cat_business}> Business</label>
                <label class="checkbox-label"><input type="checkbox" name="discover_cat_finance"{discover_cat_finance}> Finance</label>
                <label class="checkbox-label"><input type="checkbox" name="discover_cat_tech"{discover_cat_tech}> Tech</label>
                <label class="checkbox-label"><input type="checkbox" name="discover_cat_world"{discover_cat_world}> World</label>
                <label class="checkbox-label"><input type="checkbox" name="discover_cat_sports"{discover_cat_sports}> Sports</label>
                <label class="checkbox-label"><input type="checkbox" name="discover_cat_entertainment"{discover_cat_entertainment}> Entertainment</label>
                <label class="checkbox-label"><input type="checkbox" name="discover_cat_health"{discover_cat_health}> Health</label>
                <label class="checkbox-label"><input type="checkbox" name="discover_cat_science"{discover_cat_science}> Science</label>
                <label class="checkbox-label"><input type="checkbox" name="discover_cat_culture"{discover_cat_culture}> Culture</label>
                <label class="checkbox-label"><input type="checkbox" name="discover_cat_opinion"{discover_cat_opinion}> Opinion</label>
                <label class="checkbox-label"><input type="checkbox" name="discover_cat_lifestyle"{discover_cat_lifestyle}> Lifestyle</label>
                <label class="checkbox-label"><input type="checkbox" name="discover_cat_society"{discover_cat_society}> Society</label>
              </div>
              <small data-i18n="preferences.discover_categories_hint">Select categories to display in Discover feed. Empty = show all.</small>
            </div>
            <div class="field">
              <label for="discover_articles_per_category" data-i18n="preferences.discover_articles_count">Articles per category</label>
              <input type="number" id="discover_articles_per_category" name="discover_articles_per_category" value="{discover_articles_per_category}" min="1" max="20">
              <small data-i18n="preferences.discover_articles_count_hint">Number of articles shown in each category section (default: 8)</small>
            </div>
          </details>
        </section>

        <section id="server" class="section">
          <h2 data-i18n="preferences.server">Server</h2>
          <p class="section-desc" data-i18n="preferences.server_desc">Server configuration and network settings.</p>
          <div class="field">
            <label for="bind_address" data-i18n="preferences.bind_address">Bind Address</label>
            <input type="text" id="bind_address" name="bind_address" value="{bind_address}" placeholder="127.0.0.1">
          </div>
          <div class="field">
            <label for="port" data-i18n="preferences.port">Port</label>
            <input type="number" id="port" name="port" value="{port}" min="1" max="65535">
          </div>
          <div class="field">
            <label for="max_connections" data-i18n="preferences.max_connections">Max Connections</label>
            <input type="number" id="max_connections" name="max_connections" value="{max_connections}" min="1" max="1000">
          </div>
          <div class="field">
            <label><input type="checkbox" name="image_proxy"{image_proxy_chk}> <span data-i18n="preferences.image_proxy">Enable Image Proxy</span></label>
          </div>
          <div class="field">
            <label for="cache_backend" data-i18n="preferences.cache_backend">Cache Backend</label>
            <select id="cache_backend" name="cache_backend">
              <option value="memory"{cache_memory}>Memory</option>
              <option value="disk"{cache_disk}>Disk</option>
              <option value="redis"{cache_redis}>Redis</option>
            </select>
          </div>
          <div class="field">
            <label for="cache_dir" data-i18n="preferences.cache_dir">Cache Directory</label>
            <input type="text" id="cache_dir" name="cache_dir" value="{cache_dir}" placeholder=".metasearch-cache">
          </div>
          <div class="field">
            <label for="redis_url" data-i18n="preferences.redis_url">Redis URL</label>
            <input type="text" id="redis_url" name="redis_url" value="{redis_url}" placeholder="redis://127.0.0.1:6379">
          </div>
        </section>

        <section id="engines" class="section">
          <h2 data-i18n="preferences.engines">Search Engines</h2>
          <p class="section-desc" data-i18n="preferences.engines_desc">Enable/disable engines and adjust their ranking weights. Higher weight = more influence on result order.</p>
          {engine_sections}
        </section>

        <section id="privacy" class="section">
          <h2 data-i18n="preferences.privacy">Privacy</h2>
          <p class="section-desc" data-i18n="preferences.privacy_desc">Control how your data is handled.</p>
          <div class="field">
            <label class="checkbox-label">
              <input type="checkbox" name="proxy_images" checked>
              <span data-i18n="preferences.proxy_images">Proxy images</span>
            </label>
            <small data-i18n="preferences.proxy_images_hint">Route images through server to hide your IP from third parties</small>
          </div>
          <div class="field">
            <label class="checkbox-label">
              <input type="checkbox" name="remove_trackers" checked>
              <span data-i18n="preferences.remove_trackers">Remove tracking parameters</span>
            </label>
            <small data-i18n="preferences.remove_trackers_hint">Strip UTM and other tracking parameters from URLs</small>
          </div>
          <div class="field">
            <label class="checkbox-label">
              <input type="checkbox" name="save_history">
              <span data-i18n="preferences.save_history">Save search history</span>
            </label>
            <small data-i18n="preferences.save_history_hint">Store searches locally in browser (never sent to server)</small>
          </div>
          <div class="field">
            <label data-i18n="preferences.clear_data">Clear Data</label>
            <div style="display:flex;gap:.5rem;margin-top:.5rem;">
              <button type="button" class="btn-secondary" data-i18n="preferences.clear_localstorage" onclick="localStorage.clear();alert('Cleared!')">Clear Local Storage</button>
              <button type="button" class="btn-secondary" data-i18n="preferences.clear_indexeddb" onclick="indexedDB.deleteDatabase('orgos');alert('Cleared!')">Clear IndexedDB</button>
            </div>
          </div>
        </section>

        <section id="shortcuts" class="section">
          <h2 data-i18n="preferences.shortcuts">Keyboard Shortcuts</h2>
          <p class="section-desc" data-i18n="preferences.shortcuts_desc">Quick access keys for power users.</p>
          <table class="shortcuts-table">
            <tr><td><kbd>/</kbd></td><td data-i18n="shortcuts.focus_search">Focus search box</td></tr>
            <tr><td><kbd>Esc</kbd></td><td data-i18n="shortcuts.close_panel">Close panel / Cancel</td></tr>
            <tr><td><kbd>↑</kbd> <kbd>↓</kbd></td><td data-i18n="shortcuts.navigate_results">Navigate results</td></tr>
            <tr><td><kbd>Enter</kbd></td><td data-i18n="shortcuts.open_result">Open selected result</td></tr>
            <tr><td><kbd>Ctrl</kbd>+<kbd>Enter</kbd></td><td data-i18n="shortcuts.open_new_tab">Open in new tab</td></tr>
            <tr><td><kbd>Tab</kbd></td><td data-i18n="shortcuts.switch_mode">Switch search mode</td></tr>
            <tr><td><kbd>?</kbd></td><td data-i18n="shortcuts.show_help">Show this help</td></tr>
          </table>
        </section>

        <section id="advanced" class="section">
          <h2 data-i18n="preferences.advanced">Advanced</h2>
          <p class="section-desc" data-i18n="preferences.advanced_desc">Cache and timeout settings.</p>
          <div class="field">
            <label for="cache_ttl_secs" data-i18n="preferences.cache_ttl">Cache TTL (seconds)</label>
            <input type="number" id="cache_ttl_secs" name="cache_ttl_secs" value="{cache_ttl_secs}" min="0" max="86400">
            <small data-i18n="preferences.cache_ttl_hint">Result cache time-to-live (0 disables caching)</small>
          </div>
          <div class="field">
            <label for="request_timeout_secs" data-i18n="preferences.request_timeout">Request Timeout (seconds)</label>
            <input type="number" id="request_timeout_secs" name="request_timeout_secs" value="{request_timeout_secs}" min="1" max="60">
            <small data-i18n="preferences.request_timeout_hint">Per-engine network timeout</small>
          </div>
        </section>

        <button type="submit" class="btn" data-i18n="preferences.save">Save Preferences</button>
      </main>
    </div>
  </form>

  <div class="tools">
    <a href="/config" data-i18n="preferences.raw_config">Config JSON</a>
    <a href="/stats" data-i18n="preferences.stats">Stats</a>
    <a href="/" data-i18n="preferences.home">Home</a>
    {theme_tools}
  </div>

  <script>
    (function() {{
      var LANG = {{}};
      var currentLang = localStorage.getItem('ms-lang') || (navigator.language.startsWith('ko') ? 'ko' : 'en');
      function t(key) {{
        var keys = key.split('.');
        var val = LANG;
        for (var i = 0; i < keys.length; i++) {{
          if (val && typeof val === 'object' && keys[i] in val) val = val[keys[i]];
          else return key;
        }}
        return typeof val === 'string' ? val : key;
      }}
      function applyLang() {{
        console.log('[i18n] applyLang called, LANG.preferences keys:', LANG.preferences ? Object.keys(LANG.preferences) : 'none');
        console.log('[i18n] test: preferences.ai_vision_model =', t('preferences.ai_vision_model'));
        document.querySelectorAll('[data-i18n]').forEach(function(el) {{
          var key = el.getAttribute('data-i18n');
          var val = t(key);
          if (key.includes('ai_vision') || key.includes('ai_news') || key.includes('ai_answer')) {{
            console.log('[i18n]', key, '->', val, '| match:', val !== key);
          }}
          if (val !== key) el.textContent = val;
        }});
        document.querySelectorAll('[data-i18n-placeholder]').forEach(function(el) {{
          var key = el.getAttribute('data-i18n-placeholder');
          var val = t(key);
          if (val !== key) el.placeholder = val;
        }});
      }}
      fetch('/lang/' + currentLang + '.json').then(function(r) {{ return r.json(); }}).then(function(data) {{
        LANG = data;
        applyLang();
      }}).catch(function() {{}});
      var sel = document.getElementById('uiLangSelect');
      sel.value = currentLang;
      sel.onchange = function() {{
        localStorage.setItem('ms-lang', sel.value);
        location.reload();
      }};
      // Language chips - quick select
      document.querySelectorAll('.lang-chip').forEach(function(chip) {{
        if (chip.dataset.lang === currentLang) chip.classList.add('active');
        chip.onclick = function() {{
          localStorage.setItem('ms-lang', chip.dataset.lang);
          location.reload();
        }};
      }});
      // Tab switching - show one section at a time
      var sections = document.querySelectorAll('.pref-main section[id]');
      var navLinks = document.querySelectorAll('.pref-sidebar a');
      function showSection(id) {{
        sections.forEach(function(sec) {{
          sec.style.display = sec.id === id ? 'block' : 'none';
        }});
        navLinks.forEach(function(link) {{
          link.classList.remove('active');
          if (link.getAttribute('href') === '#' + id) {{
            link.classList.add('active');
          }}
        }});
      }}
      navLinks.forEach(function(link) {{
        link.addEventListener('click', function(e) {{
          e.preventDefault();
          var id = link.getAttribute('href').substring(1);
          showSection(id);
          history.replaceState(null, '', '#' + id);
        }});
      }});
      // Show section from URL hash or default to first
      var hash = window.location.hash.substring(1);
      showSection(hash || 'branding');
    }})();
  </script>
</div>
</body></html>"##,
        theme_attr = theme.attr(),
        theme_tools = theme_tools(theme, "/preferences"),
        styles = page_styles(),
        banner = banner,
        app_name = app_name,
        logo_url = logo_url,
        favicon_url = favicon_url,
        lang = escape(&settings.search.default_lang),
        search_lang = escape(&settings.search.default_language),
        s0 = sel(0),
        s1 = sel(1),
        s2 = sel(2),
        rpp = settings.server.max_results_per_engine,
        engine_sections = engine_sections,
        // AI settings
        ai_enabled_chk = if settings.ai.enabled { " checked" } else { "" },
        ai_base_url = escape(&settings.ai.base_url),
        ai_api_key = settings.ai.api_key.as_deref().map(|_| "••••••••").unwrap_or(""),
        ai_model = escape(&settings.ai.model),
        // Provider selection based on base_url
        provider_local_chk = if settings.ai.base_url.contains("localhost") || settings.ai.base_url.contains("127.0.0.1") { " checked" } else { "" },
        provider_local_sel = if settings.ai.base_url.contains("localhost") || settings.ai.base_url.contains("127.0.0.1") { " selected" } else { "" },
        provider_openai_chk = if settings.ai.base_url.contains("openai.com") { " checked" } else { "" },
        provider_openai_sel = if settings.ai.base_url.contains("openai.com") { " selected" } else { "" },
        provider_anthropic_chk = if settings.ai.base_url.contains("anthropic.com") { " checked" } else { "" },
        provider_anthropic_sel = if settings.ai.base_url.contains("anthropic.com") { " selected" } else { "" },
        provider_groq_chk = if settings.ai.base_url.contains("groq.com") { " checked" } else { "" },
        provider_groq_sel = if settings.ai.base_url.contains("groq.com") { " selected" } else { "" },
        provider_together_chk = if settings.ai.base_url.contains("together.xyz") { " checked" } else { "" },
        provider_together_sel = if settings.ai.base_url.contains("together.xyz") { " selected" } else { "" },
        provider_custom_chk = if !settings.ai.base_url.contains("localhost") && !settings.ai.base_url.contains("127.0.0.1") && !settings.ai.base_url.contains("openai.com") && !settings.ai.base_url.contains("anthropic.com") && !settings.ai.base_url.contains("groq.com") && !settings.ai.base_url.contains("together.xyz") { " checked" } else { "" },
        provider_custom_sel = if !settings.ai.base_url.contains("localhost") && !settings.ai.base_url.contains("127.0.0.1") && !settings.ai.base_url.contains("openai.com") && !settings.ai.base_url.contains("anthropic.com") && !settings.ai.base_url.contains("groq.com") && !settings.ai.base_url.contains("together.xyz") { " selected" } else { "" },
        // Cost tracking
        ai_track_usage_chk = if settings.ai.track_usage { " checked" } else { "" },
        ai_input_cost = settings.ai.input_cost_per_million,
        ai_output_cost = settings.ai.output_cost_per_million,
        ai_chat_retention_days = settings.ai.chat_retention_days,
        ai_article_model = escape(&settings.ai.article_model),
        ai_embedding_model = escape(&settings.ai.embedding_model),
        ai_vision_model = escape(&settings.ai.vision_model),
        ai_answer_top_n = settings.ai.answer_top_n,
        ai_timeout_secs = settings.ai.timeout_secs,
        ai_answer_chk = if settings.ai.answer { " checked" } else { "" },
        ai_expand_chk = if settings.ai.expand { " checked" } else { "" },
        ai_rerank_chk = if settings.ai.rerank { " checked" } else { "" },
        ai_cluster_chk = if settings.ai.cluster { " checked" } else { "" },
        ai_vision_chk = if settings.ai.vision { " checked" } else { "" },
        ai_news_prompt_ko = escape(&settings.ai.news_prompt_ko),
        ai_news_prompt_en = escape(&settings.ai.news_prompt_en),
        answer_lang_auto = if settings.ai.answer_language == "auto" || settings.ai.answer_language.is_empty() { " selected" } else { "" },
        answer_lang_en = if settings.ai.answer_language == "en" { " selected" } else { "" },
        answer_lang_ko = if settings.ai.answer_language == "ko" { " selected" } else { "" },
        answer_lang_ja = if settings.ai.answer_language == "ja" { " selected" } else { "" },
        answer_lang_zh = if settings.ai.answer_language == "zh" { " selected" } else { "" },
        answer_lang_es = if settings.ai.answer_language == "es" { " selected" } else { "" },
        answer_lang_fr = if settings.ai.answer_language == "fr" { " selected" } else { "" },
        answer_lang_de = if settings.ai.answer_language == "de" { " selected" } else { "" },
        answer_lang_pt = if settings.ai.answer_language == "pt" { " selected" } else { "" },
        answer_lang_it = if settings.ai.answer_language == "it" { " selected" } else { "" },
        answer_lang_ru = if settings.ai.answer_language == "ru" { " selected" } else { "" },
        answer_lang_vi = if settings.ai.answer_language == "vi" { " selected" } else { "" },
        answer_lang_th = if settings.ai.answer_language == "th" { " selected" } else { "" },
        answer_lang_ar = if settings.ai.answer_language == "ar" { " selected" } else { "" },
        // News settings
        news_per_source_cap = settings.search.news.per_source_cap,
        news_freshness_half_life = settings.search.news.freshness_half_life_hours,
        news_freshness_weight = settings.search.news.freshness_weight,
        news_dedup_similarity = settings.search.news.dedup_title_similarity,
        news_max_age_days = settings.search.news.max_age_days,
        news_cache_ttl = settings.search.news.cache_ttl_secs,
        news_enrich_max = settings.search.news.enrich_max,
        // Discover settings
        discover_articles_per_category = settings.search.news.discover_articles_per_category,
        discover_cat_news = if settings.search.news.discover_categories.is_empty() || settings.search.news.discover_categories.iter().any(|c| c == "news") { " checked" } else { "" },
        discover_cat_politics = if settings.search.news.discover_categories.iter().any(|c| c == "politics") { " checked" } else { "" },
        discover_cat_business = if settings.search.news.discover_categories.iter().any(|c| c == "business") { " checked" } else { "" },
        discover_cat_finance = if settings.search.news.discover_categories.iter().any(|c| c == "finance") { " checked" } else { "" },
        discover_cat_tech = if settings.search.news.discover_categories.iter().any(|c| c == "tech") { " checked" } else { "" },
        discover_cat_world = if settings.search.news.discover_categories.iter().any(|c| c == "world") { " checked" } else { "" },
        discover_cat_sports = if settings.search.news.discover_categories.iter().any(|c| c == "sports") { " checked" } else { "" },
        discover_cat_entertainment = if settings.search.news.discover_categories.iter().any(|c| c == "entertainment") { " checked" } else { "" },
        discover_cat_health = if settings.search.news.discover_categories.iter().any(|c| c == "health") { " checked" } else { "" },
        discover_cat_science = if settings.search.news.discover_categories.iter().any(|c| c == "science") { " checked" } else { "" },
        discover_cat_culture = if settings.search.news.discover_categories.iter().any(|c| c == "culture") { " checked" } else { "" },
        discover_cat_opinion = if settings.search.news.discover_categories.iter().any(|c| c == "opinion") { " checked" } else { "" },
        discover_cat_lifestyle = if settings.search.news.discover_categories.iter().any(|c| c == "lifestyle") { " checked" } else { "" },
        discover_cat_society = if settings.search.news.discover_categories.iter().any(|c| c == "auto") { " checked" } else { "" },
        // Server settings
        bind_address = escape(&settings.server.bind_address),
        port = settings.server.port,
        max_connections = settings.server.max_connections,
        image_proxy_chk = if settings.server.image_proxy { " checked" } else { "" },
        cache_memory = if settings.server.cache_backend == "memory" { " selected" } else { "" },
        cache_disk = if settings.server.cache_backend == "disk" { " selected" } else { "" },
        cache_redis = if settings.server.cache_backend == "redis" { " selected" } else { "" },
        cache_dir = escape(&settings.server.cache_dir),
        redis_url = escape(&settings.server.redis_url),
        cache_ttl_secs = settings.server.cache_ttl_secs,
        request_timeout_secs = settings.server.request_timeout_secs,
    )
}

fn category_description(cat: &str) -> &'static str {
    match cat {
        "general" => "Web search engines for general queries",
        "news" => "News sources, aggregators, and RSS feeds",
        "images" => "Image search engines and galleries",
        "videos" => "Video platforms, streaming, and search",
        "science" => "Academic papers, research databases, and scientific sources",
        "it" => "Developer tools, code repositories, and programming resources",
        "tech" => "Technology news, blogs, and product reviews",
        "books" => "Book search, libraries, and ebook platforms",
        "dictionary" => "Dictionaries, translators, and language references",
        "howto" => "Tutorials, guides, and Q&A platforms",
        "social" => "Social media and community platforms",
        "music" => "Music archives, streaming, and lyrics",
        "games" => "Game databases, reviews, and communities",
        "shopping" => "Product search and price comparison",
        "jobs" => "Job boards and career platforms",
        "finance" => "Financial news, stock data, and market info",
        "map" => "Map and location services",
        "files" => "File archives and document repositories",
        "other" => "Miscellaneous search engines",
        _ => "Search engines in this category",
    }
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// Warm Discover snapshot cache for common categories on server startup.
/// Runs in background so it doesn't block the server from accepting requests.
/// Spawns parallel workers for each language to cover all 7 languages simultaneously.
async fn warm_discover_cache(ctx: &Ctx) {
    use crate::news_digest::run_discover_snapshot;

    const LANGUAGES: &[(&str, &[(&str, &str)])] = &[
        ("ko-KR", &[
            ("top", "최신 뉴스 헤드라인 인기"),
            ("tech", "기술"), ("ai", "인공지능"), ("world", "세계"),
            ("politics", "정치"), ("business", "경제"), ("economy", "경제"), ("finance", "금융"),
            ("health", "건강"), ("climate", "기후"), ("sports", "스포츠 축구 야구"),
            ("entertainment", "연예"), ("art", "문화"),
        ]),
        ("en-US", &[
            ("top", "latest news headlines"),
            ("tech", "technology"), ("ai", "AI"), ("world", "international"),
            ("politics", "politics"), ("business", "business"), ("economy", "economy"), ("finance", "finance"),
            ("health", "health"), ("climate", "climate"), ("sports", "sports football soccer"),
            ("entertainment", "entertainment"), ("art", "culture"),
        ]),
        ("ja-JP", &[
            ("top", "最新ニュース"),
            ("tech", "テクノロジー"), ("ai", "AI"), ("world", "国際"),
            ("politics", "政治"), ("business", "経済"), ("economy", "経済"), ("finance", "金融"),
            ("health", "健康"), ("climate", "環境"), ("sports", "サッカー 野球 バスケ"),
            ("entertainment", "芸能"), ("art", "文化"),
        ]),
        ("zh-CN", &[
            ("top", "最新新闻"),
            ("tech", "科技"), ("ai", "AI"), ("world", "国际"),
            ("politics", "政治"), ("business", "经济"), ("economy", "经济"), ("finance", "金融"),
            ("health", "健康"), ("climate", "环境"), ("sports", "体育"),
            ("entertainment", "娱乐"), ("art", "文化"),
        ]),
        ("es-ES", &[
            ("top", "ultimas noticias"),
            ("tech", "tecnologia"), ("ai", "inteligencia artificial"), ("world", "internacional"),
            ("politics", "politica"), ("business", "economia"), ("economy", "economia"), ("finance", "finanzas"),
            ("health", "salud"), ("climate", "clima"), ("sports", "deportes"),
            ("entertainment", "entretenimiento"), ("art", "cultura"),
        ]),
        ("fr-FR", &[
            ("top", "actualites"),
            ("tech", "technologie"), ("ai", "intelligence artificielle"), ("world", "international"),
            ("politics", "politique"), ("business", "economie"), ("economy", "economie"), ("finance", "finance"),
            ("health", "sante"), ("climate", "climat"), ("sports", "sports"),
            ("entertainment", "divertissement"), ("art", "culture"),
        ]),
        ("de-DE", &[
            ("top", "aktuelle nachrichten"),
            ("tech", "Technologie"), ("ai", "KI"), ("world", "international"),
            ("politics", "Politik"), ("business", "Wirtschaft"), ("economy", "Wirtschaft"), ("finance", "Finanzen"),
            ("health", "Gesundheit"), ("climate", "Klima"), ("sports", "Sport"),
            ("entertainment", "Unterhaltung"), ("art", "Kultur"),
        ]),
        ("pt-BR", &[
            ("top", "ultimas noticias"), ("tech", "tecnologia"), ("ai", "inteligencia artificial"),
            ("world", "internacional"), ("politics", "politica"), ("business", "economia"),
            ("finance", "financas"), ("health", "saude"), ("sports", "esportes"),
            ("entertainment", "entretenimento"), ("art", "cultura"), ("climate", "clima"),
        ]),
        ("ru-RU", &[
            ("top", "последние новости"), ("tech", "технологии"), ("ai", "искусственный интеллект"),
            ("world", "мир"), ("politics", "политика"), ("business", "экономика"),
            ("finance", "финансы"), ("health", "здоровье"), ("sports", "спорт"),
            ("entertainment", "развлечения"), ("art", "культура"), ("climate", "климат"),
        ]),
        ("it-IT", &[
            ("top", "ultime notizie"), ("tech", "tecnologia"), ("ai", "intelligenza artificiale"),
            ("world", "mondo"), ("politics", "politica"), ("business", "economia"),
            ("finance", "finanza"), ("health", "salute"), ("sports", "sport"),
            ("entertainment", "intrattenimento"), ("art", "cultura"), ("climate", "clima"),
        ]),
        ("pl-PL", &[
            ("top", "najnowsze wiadomosci"), ("tech", "technologia"), ("ai", "sztuczna inteligencja"),
            ("world", "swiat"), ("politics", "polityka"), ("business", "ekonomia"),
            ("finance", "finanse"), ("health", "zdrowie"), ("sports", "sport"),
            ("entertainment", "rozrywka"), ("art", "kultura"), ("climate", "klimat"),
        ]),
        ("nl-NL", &[
            ("top", "laatste nieuws"), ("tech", "technologie"), ("ai", "kunstmatige intelligentie"),
            ("world", "wereld"), ("politics", "politiek"), ("business", "economie"),
            ("finance", "financien"), ("health", "gezondheid"), ("sports", "sport"),
            ("entertainment", "entertainment"), ("art", "cultuur"), ("climate", "klimaat"),
        ]),
        ("tr-TR", &[
            ("top", "son haberler"), ("tech", "teknoloji"), ("ai", "yapay zeka"),
            ("world", "dunya"), ("politics", "siyaset"), ("business", "ekonomi"),
            ("finance", "finans"), ("health", "saglik"), ("sports", "spor"),
            ("entertainment", "eglence"), ("art", "kultur"), ("climate", "iklim"),
        ]),
        ("vi-VN", &[
            ("top", "tin moi nhat"), ("tech", "cong nghe"), ("ai", "tri tue nhan tao"),
            ("world", "the gioi"), ("politics", "chinh tri"), ("business", "kinh te"),
            ("finance", "tai chinh"), ("health", "suc khoe"), ("sports", "the thao"),
            ("entertainment", "giai tri"), ("art", "van hoa"), ("climate", "khi hau"),
        ]),
        ("th-TH", &[
            ("top", "ข่าวล่าสุด"), ("tech", "เทคโนโลยี"), ("ai", "ปัญญาประดิษฐ์"),
            ("world", "โลก"), ("politics", "การเมือง"), ("business", "เศรษฐกิจ"),
            ("finance", "การเงิน"), ("health", "สุขภาพ"), ("sports", "กีฬา"),
            ("entertainment", "บันเทิง"), ("art", "วัฒนธรรม"), ("climate", "สภาพอากาศ"),
        ]),
        ("id-ID", &[
            ("top", "berita terbaru"), ("tech", "teknologi"), ("ai", "kecerdasan buatan"),
            ("world", "dunia"), ("politics", "politik"), ("business", "ekonomi"),
            ("finance", "keuangan"), ("health", "kesehatan"), ("sports", "olahraga"),
            ("entertainment", "hiburan"), ("art", "budaya"), ("climate", "iklim"),
        ]),
        ("ar-SA", &[
            ("top", "آخر الأخبار"), ("tech", "تكنولوجيا"), ("ai", "ذكاء اصطناعي"),
            ("world", "عالم"), ("politics", "سياسة"), ("business", "اقتصاد"),
            ("finance", "مالية"), ("health", "صحة"), ("sports", "رياضة"),
            ("entertainment", "ترفيه"), ("art", "ثقافة"), ("climate", "مناخ"),
        ]),
        ("lt-LT", &[
            ("top", "naujienos"), ("tech", "technologijos"), ("ai", "dirbtinis intelektas"),
            ("world", "pasaulis"), ("politics", "politika"), ("business", "ekonomika"),
            ("finance", "finansai"), ("health", "sveikata"), ("sports", "sportas"),
            ("entertainment", "pramogos"), ("art", "kultura"), ("climate", "klimatas"),
        ]),
    ];

    let total_tasks = LANGUAGES.iter().map(|(_, cats)| cats.len()).sum::<usize>();
    eprintln!("[metasearch] Warming Discover cache for {} languages x {} categories = {} tasks (parallel)...",
        LANGUAGES.len(), 12, total_tasks);
    let start = std::time::Instant::now();
    let settings = ctx.settings();
    let rt = ctx.rt.clone();

    // Limit concurrency to avoid overloading DB - 3 parallel language workers
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(3));
    let mut handles = Vec::new();
    for (lang, categories) in LANGUAGES {
        let settings = settings.clone();
        let rt = rt.clone();
        let lang = *lang;
        let categories: Vec<_> = categories.iter().map(|(c, s)| (*c, *s)).collect();
        let sem = semaphore.clone();

        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await;
            for (cat, seed) in categories {
                let _ = run_discover_snapshot(seed, cat, 30, Some(lang), None, &settings, &rt, true).await;
                // Small delay between categories to yield to user requests
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            lang
        }));
    }

    // Wait for all language workers to complete
    for handle in handles {
        if let Ok(lang) = handle.await {
            eprintln!("[metasearch] Discover cache warm: {} done", lang);
        }
    }

    eprintln!(
        "[metasearch] Discover cache warm complete ({} tasks in {:.1}s)",
        total_tasks,
        start.elapsed().as_secs_f32()
    );

    // Also warm popular search queries
    warm_popular_searches(ctx).await;
}

/// Pre-cache popular/trending search queries for faster response.
async fn warm_popular_searches(ctx: &Ctx) {
    use crate::search::{search_all, SearchParams};

    const POPULAR_QUERIES: &[&str] = &[
        "AI", "뉴스", "날씨", "주식", "bitcoin",
        "chatgpt", "technology", "Korea", "경제", "스포츠",
    ];

    eprintln!("[metasearch] Warming popular search cache...");
    let start = std::time::Instant::now();
    let settings = ctx.settings();

    for q in POPULAR_QUERIES {
        let params = SearchParams {
            query: q.to_string(),
            categories: vec!["general".to_string()],
            pageno: 1,
            language: None,
            time_range: None,
            safe_search: None,
            ai_answer: Some(false),
            context: None,
            rerank: None,
            deep: None,
            deep_subqueries: None,
            discover_category: None,
            country: None,
        };
        let _ = search_all(&params, &settings, &ctx.rt).await;
    }

    eprintln!(
        "[metasearch] Popular search cache warm complete ({} queries in {:.1}s)",
        POPULAR_QUERIES.len(),
        start.elapsed().as_secs_f32()
    );
}



// rebuild trigger 1781473598
// rebuild 1781474305
// rebuild 1781474750
// rebuild 1781474861
