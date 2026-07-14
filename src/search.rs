//! Search orchestration: parse the query, fan out to engines concurrently,
//! aggregate, then apply optional answerers and AI enhancements.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::aggregate::{
    aggregate, apply_domain_trust, apply_extended_ranking, apply_favicons, default_trusted_domains,
    merge_aggregated, parse_relative_time, RankingConfig,
};
use crate::cache::Cache;
use crate::config::Settings;
use crate::engines::{self, retry, EngineContext};
use crate::health::{FailureClass, HealthInfo, HealthTracker};
use crate::ratelimit::RateLimiter;
use crate::types::{Answer, Infobox, SearchResult};
use crate::{ai, answerers, obs, query};

/// Per-request search inputs (decoupled from any transport).
#[derive(Debug, Clone, Default)]
pub struct SearchParams {
    /// Raw query (may contain `!bang` / `:lang` tokens).
    pub query: String,
    pub categories: Vec<String>,
    pub pageno: usize,
    pub language: Option<String>,
    pub time_range: Option<String>,
    pub safe_search: Option<u8>,
    /// Override `ai.answer`: `Some(true)` forces AI synthesis on, `Some(false)` off.
    pub ai_answer: Option<bool>,
    /// Previous query for multi-turn conversational refinement. When set (and
    /// `ai.conversational` is on), the LLM rewrites `query` into a standalone
    /// query using this as context.
    pub context: Option<String>,
    /// Per-request semantic re-rank override (`?rerank=1`). When `Some(true)`,
    /// runs embedding rerank if `ai.enabled`; when `Some(false)`, skips even if
    /// `ai.rerank` is on in settings.
    pub rerank: Option<bool>,
    /// Multi-hop deep research (`?deep=1`): plan sub-queries, fan out each, merge
    /// and de-dupe (capped). Degrades to a single search when AI is unavailable.
    pub deep: Option<bool>,
    /// Pre-planned sub-queries (set by the transport layer to avoid double-planning
    /// when emitting SSE `plan` events before search).
    pub deep_subqueries: Option<Vec<String>>,
    /// Discover category filter (e.g., "sports", "tech") - for local_feeds DB query
    pub discover_category: Option<String>,
    /// Country filter (ISO 3166-1 alpha-2 code, e.g., "US", "KR", "JP")
    pub country: Option<String>,
}

impl SearchParams {
    pub fn new(query: impl Into<String>) -> Self {
        SearchParams {
            query: query.into(),
            pageno: 1,
            ..Default::default()
        }
    }
}

/// Top-level response, shaped to match the standard `format=json` output (plus a
/// few additive extras that search clients safely ignore).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    pub query: String,
    pub number_of_results: usize,
    pub pageno: usize,
    pub results: Vec<SearchResult>,
    pub answers: Vec<Answer>,
    pub corrections: Vec<String>,
    pub infoboxes: Vec<Infobox>,
    pub suggestions: Vec<String>,
    /// `[engine_name, reason]` for engines that errored or timed out.
    pub unresponsive_engines: Vec<(String, String)>,
    /// Per-engine timing for this request (extension; ms).
    pub timings: Vec<EngineTiming>,
    /// Sub-queries used for multi-hop deep research (empty when not deep).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub deep_subqueries: Vec<String>,
    /// Whether this response was served from cache.
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub cache_hit: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineTiming {
    pub engine: String,
    pub total_ms: u128,
    pub results: usize,
}

impl SearchResponse {
    pub(crate) fn empty(query: String, pageno: usize) -> Self {
        SearchResponse {
            query,
            number_of_results: 0,
            pageno,
            results: Vec::new(),
            answers: Vec::new(),
            corrections: Vec::new(),
            infoboxes: Vec::new(),
            suggestions: Vec::new(),
            unresponsive_engines: Vec::new(),
            timings: Vec::new(),
            deep_subqueries: Vec::new(),
            cache_hit: false,
        }
    }
}

/// Cumulative per-engine statistics (for `/stats`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct EngineStat {
    pub calls: u64,
    pub errors: u64,
    pub results: u64,
    pub total_ms: u128,
    /// Rolling window of recent per-call latencies (ms), newest last. Capped to
    /// [`LATENCY_WINDOW`] so `/stats` can show a latency trend sparkline.
    pub recent_ms: std::collections::VecDeque<u128>,
    /// Rolling window of recent outcomes (true = success), for a success-rate
    /// trend independent of the all-time average.
    pub recent_ok: std::collections::VecDeque<bool>,
}

/// How many recent samples to keep per engine for trend display.
pub const LATENCY_WINDOW: usize = 50;

impl EngineStat {
    /// All-time success rate in `0.0..=1.0`.
    pub fn success_rate(&self) -> f64 {
        if self.calls == 0 {
            return 1.0;
        }
        (self.calls - self.errors) as f64 / self.calls as f64
    }

    /// Success rate over the recent rolling window.
    pub fn recent_success_rate(&self) -> f64 {
        if self.recent_ok.is_empty() {
            return 1.0;
        }
        let ok = self.recent_ok.iter().filter(|b| **b).count();
        ok as f64 / self.recent_ok.len() as f64
    }

    /// Average latency over the recent rolling window (ms).
    pub fn recent_avg_ms(&self) -> f64 {
        if self.recent_ms.is_empty() {
            return 0.0;
        }
        self.recent_ms.iter().sum::<u128>() as f64 / self.recent_ms.len() as f64
    }
}

/// Shared, cheaply-clonable runtime: HTTP client + cache + rate limiter +
/// rolling stats. Built once from [`Settings`] and reused by every request
/// (both the standalone server and the in-app Tauri command).
pub struct Runtime {
    pub client: reqwest::Client,
    /// Clients bound to each configured proxy (for rotation on retry). Empty
    /// when no proxies are configured.
    pub proxy_clients: Vec<reqwest::Client>,
    /// Retry attempts for transiently-failing engines (bot-block resilience).
    pub max_retries: u32,
    pub cache: Cache,
    pub limiter: RateLimiter,
    /// Per-engine health tracker driving automatic cool-down / fallback.
    pub health: HealthTracker,
    /// Local engine-usage learner (when `search.personalization` is enabled).
    pub personalization: Option<crate::personalization::Personalization>,
    /// In-process LRU cache for embedding vectors (rerank/cluster). Keyed by
    /// `(model, sha256(text))` so repeated snippets aren't re-embedded.
    pub embed_cache: crate::ai::EmbeddingCache,
    /// TTL cache for fetched article HTML/text (full-page rewrite).
    pub article_cache: crate::article::ArticleCache,
    /// TTL cache for completed full-page article rewrites.
    pub article_rewrite_cache: crate::news_article::NewsArticleRewriteCache,
    /// Short-TTL cache for fully-built Discover/News digests, so re-visited
    /// categories return instantly (skips search fan-out + image enrichment).
    pub digest_cache: crate::news_digest::DigestCache,
    /// Daily Discover category snapshots with image-bearing curated cards.
    pub discover_snapshot_cache: crate::news_digest::DiscoverSnapshotCache,
    /// Short-TTL cache for asynchronous Discover/News image hydration.
    pub news_image_cache: crate::news_digest::NewsImageCache,
    stats: Mutex<HashMap<String, EngineStat>>,
}

impl Runtime {
    pub fn new(settings: &Settings) -> Self {
        let personalization = if settings.search.personalization {
            let path =
                std::path::Path::new(&settings.server.cache_dir).join("personalization.json");
            Some(crate::personalization::Personalization::load(path))
        } else {
            None
        };
        Runtime {
            client: build_client(),
            proxy_clients: build_proxy_clients(&settings.server.proxies),
            max_retries: settings.server.max_retries,
            cache: Cache::from_settings(&settings.server),
            limiter: RateLimiter::new(settings.server.engine_min_interval_ms),
            health: HealthTracker::new(
                settings.server.engine_failure_threshold,
                settings.server.engine_cooldown_secs,
            ),
            personalization,
            embed_cache: crate::ai::EmbeddingCache::new(512),
            article_cache: crate::article::ArticleCache::new(64),
            article_rewrite_cache: crate::news_article::NewsArticleRewriteCache::new(64),
            digest_cache: crate::news_digest::DigestCache::new(settings.search.news.cache_ttl_secs),
            discover_snapshot_cache: crate::news_digest::DiscoverSnapshotCache::new(
                settings.search.news.discover_cache_ttl_hours,
            ),
            news_image_cache: crate::news_digest::NewsImageCache::new(
                settings.search.news.cache_ttl_secs,
            ),
            stats: Mutex::new(HashMap::new()),
        }
    }

    /// Pick the HTTP client to use for retry `attempt` (0 = direct). Rotates
    /// through configured proxies on subsequent attempts.
    fn client_for(&self, attempt: u32) -> &reqwest::Client {
        if attempt == 0 || self.proxy_clients.is_empty() {
            &self.client
        } else {
            let idx = (attempt as usize - 1) % self.proxy_clients.len();
            &self.proxy_clients[idx]
        }
    }

    fn record(&self, engine: &str, ms: u128, results: usize, error: bool) {
        if let Ok(mut map) = self.stats.lock() {
            let s = map.entry(engine.to_string()).or_default();
            s.calls += 1;
            s.total_ms += ms;
            s.results += results as u64;
            if error {
                s.errors += 1;
            }
            s.recent_ms.push_back(ms);
            if s.recent_ms.len() > LATENCY_WINDOW {
                s.recent_ms.pop_front();
            }
            s.recent_ok.push_back(!error);
            if s.recent_ok.len() > LATENCY_WINDOW {
                s.recent_ok.pop_front();
            }
        }
    }

    /// Snapshot of cumulative engine statistics.
    pub fn stats(&self) -> HashMap<String, EngineStat> {
        self.stats.lock().map(|m| m.clone()).unwrap_or_default()
    }

    /// Snapshot of per-engine health / cool-down state (for `/stats`).
    pub fn health_snapshot(&self) -> HashMap<String, HealthInfo> {
        self.health.snapshot()
    }
}

/// Build the shared HTTP client used for all upstream requests.
///
/// Privacy note: no cookie store, no query persistence. We never log the user's
/// query — only engine names appear in diagnostics.
pub fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .pool_idle_timeout(std::time::Duration::from_secs(30))
        .pool_max_idle_per_host(4)
        .tcp_keepalive(std::time::Duration::from_secs(60))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Build one HTTP client per configured proxy URL (for rotation on retry).
/// Invalid proxy URLs are skipped with a warning. HTTP/HTTPS proxies work out
/// of the box; SOCKS proxies require building `reqwest` with its `socks`
/// feature.
pub fn build_proxy_clients(proxies: &[String]) -> Vec<reqwest::Client> {
    let mut clients = Vec::new();
    for url in proxies {
        let url = url.trim();
        if url.is_empty() {
            continue;
        }
        match reqwest::Proxy::all(url).and_then(|p| reqwest::Client::builder().proxy(p).build()) {
            Ok(c) => clients.push(c),
            Err(e) => obs::warn(format!("skipping invalid proxy: {e}")),
        }
    }
    clients
}

/// Run a full search: parse, fan out, aggregate, enhance.
pub async fn search_all(
    params: &SearchParams,
    settings: &Settings,
    rt: &Runtime,
) -> SearchResponse {
    if params.deep == Some(true) {
        return deep_search_all(params, settings, rt).await;
    }
    search_once(params, settings, rt).await
}

/// Single-hop search (no multi-query planning).
async fn search_once(params: &SearchParams, settings: &Settings, rt: &Runtime) -> SearchResponse {
    let pageno = params.pageno.max(1);

    // 1. Parse !bangs / :lang out of the raw query.
    let parsed = query::parse(params.query.trim());
    let mut query_text = parsed.query.trim().to_string();
    if query_text.is_empty() {
        return SearchResponse::empty(String::new(), pageno);
    }

    // 1b. Local personalization: learn explicitly-invoked engines (privacy-safe;
    //     only engine names are recorded, never the query).
    if let (Some(p), Some(engines)) = (&rt.personalization, parsed.engines.as_ref()) {
        p.record(engines);
    }

    // 1c. Conversational follow-up: rewrite into a standalone query using the
    //     previous query as context (opt-in; falls back to the raw follow-up).
    if settings.ai.enabled && settings.ai.conversational {
        if let Some(prev) = params.context.as_deref().filter(|c| !c.trim().is_empty()) {
            if let Some(refined) =
                ai::refine_query(&settings.ai, &rt.client, prev, &query_text).await
            {
                obs::debug("conversational query refined");
                query_text = refined;
            }
        }
    }

    // 2. Resolve effective categories / language / safe-search / engine set.
    let categories = if !params.categories.is_empty() {
        params.categories.clone()
    } else if !parsed.categories.is_empty() {
        parsed.categories.clone()
    } else {
        settings.search.default_categories.clone()
    };
    let language = resolve_language(
        params.language.clone().or(parsed.language.clone()),
        &query_text,
        &settings.search,
    );
    let safe_search = params.safe_search.unwrap_or(settings.search.safe_search);
    let time_range = params.time_range.clone();
    let ai_answer = params.ai_answer.unwrap_or(settings.ai.answer);
    let deep = params.deep.unwrap_or(false);
    let discover_category = params.discover_category.clone();
    let country = params.country.clone();

    // 3. Cache lookup.
    let cache_key = format!(
        "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{:?}\u{1f}{}\u{1f}{}",
        query_text,
        categories.join(","),
        pageno,
        language,
        safe_search,
        time_range.as_deref().unwrap_or(""),
        parsed.engines,
        ai_answer,
        deep,
    );
    if let Some(mut cached) = rt.cache.get(&cache_key) {
        obs::debug("cache hit");
        cached.cache_hit = true;
        return cached;
    }

    let default_timeout = Duration::from_secs(settings.server.request_timeout_secs.max(1));
    let max_results = settings.server.max_results_per_engine.max(1);
    let allow_private_urls = settings.server.allow_private_urls;

    // 3b. Drop engines that are currently cooling down (too many recent hard
    //     failures) so one persistently-blocked/timed-out engine can't keep
    //     burning a fan-out slot. They are surfaced in `unresponsive_engines`
    //     with a clear reason, and probed again once the window elapses.
    let mut cooled: Vec<(String, String)> = Vec::new();

    // 3c. For news category with explicit language, only use matching language engines
    //     e.g. language=ko → only google_news_ko, bing_news_ko, not google_news_en
    let lang_suffix = language.split('-').next().unwrap_or(&language);
    let is_news_search = categories.iter().any(|c| c == "news");

    let selected: Vec<&crate::config::EngineSettings> = settings
        .selected_engines(&categories, parsed.engines.as_deref())
        .into_iter()
        .filter(|e| {
            if rt.health.should_skip(&e.name) {
                if let Some(reason) = rt.health.cooldown_reason(&e.name) {
                    cooled.push((e.name.clone(), reason));
                }
                return false;
            }
            // For news searches with a specific language, filter language-specific engines
            if is_news_search && !lang_suffix.is_empty() && lang_suffix != "all" && lang_suffix != "auto" {
                // Only filter engines that have language suffix (google_news_XX, bing_news_XX)
                if e.name.starts_with("google_news_") || e.name.starts_with("bing_news_") {
                    let engine_lang = e.name.rsplit('_').next().unwrap_or("");
                    return engine_lang == lang_suffix;
                }
            }
            true
        })
        .collect();

    // 4. Fan out concurrently (rate-limited + per-engine timeout, with retry
    //    and optional proxy rotation for bot-block resilience).
    let max_retries = rt.max_retries;
    let tasks = selected.iter().map(|engine| {
        let timeout = engine
            .timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(default_timeout);
        let name = engine.name.clone();
        let base_url = engine.base_url.clone();
        let api_key = engine.api_key.clone();
        let extra = engine.extra.clone();
        let query_text = query_text.clone();
        let language = language.clone();
        let time_range = time_range.clone();
        let discover_category = discover_category.clone();
        let country = country.clone();
        async move {
            let started = Instant::now();
            let mut outcome = Ok(Err("not attempted".to_string()));
            for attempt in 0..=max_retries {
                if attempt > 0 {
                    tokio::time::sleep(retry::backoff(attempt)).await;
                }
                let ctx = EngineContext {
                    client: rt.client_for(attempt),
                    query: &query_text,
                    lang: &language,
                    safe_search,
                    timeout,
                    max_results,
                    pageno,
                    time_range: time_range.as_deref(),
                    base_url: base_url.as_deref(),
                    api_key: api_key.as_deref(),
                    extra: extra.as_deref(),
                    custom: None,
                    allow_private_urls,
                    category: discover_category.as_deref(),
                    country: country.as_deref(),
                };
                // The rate-limit wait is inside the per-engine timeout budget so
                // a backed-up/penalized limiter slot can never stall the fan-out
                // beyond this engine's own deadline (it just times out and drops
                // out as unresponsive).
                let key = engines::rate_limit_key(&name);
                outcome = tokio::time::timeout(timeout, async {
                    rt.limiter.acquire(key).await;
                    engines::run(&name, &ctx).await
                })
                .await;
                match &outcome {
                    // Success, or a non-retryable error: stop.
                    Ok(Ok(_)) => break,
                    Ok(Err(reason)) if !retry::is_retryable(reason) => break,
                    _ => {
                        if attempt < max_retries {
                            obs::debug(format!("retrying engine={name} attempt={}", attempt + 1));
                        }
                    }
                }
            }
            let ms = started.elapsed().as_millis();
            (name, outcome, ms)
        }
    });

    // 4b. Fan out config-driven custom engines (RSS/Atom, OpenSearch, JSON
    //     template) alongside the native engines. They participate in scoring,
    //     bangs and params identically — the only difference is dispatch goes
    //     through the generic adapter via `EngineContext::custom`.
    let custom_selected: Vec<&crate::config::CustomEngine> = settings
        .selected_custom_engines(&categories, parsed.engines.as_deref())
        .into_iter()
        .filter(|e| {
            if rt.health.should_skip(&e.name) {
                if let Some(reason) = rt.health.cooldown_reason(&e.name) {
                    cooled.push((e.name.clone(), reason));
                }
                return false;
            }
            // For news searches with a specific language, filter language-specific engines
            if is_news_search && !lang_suffix.is_empty() && lang_suffix != "all" && lang_suffix != "auto" {
                // Only filter engines that have language suffix (google_news_XX, bing_news_XX)
                if e.name.starts_with("google_news_") || e.name.starts_with("bing_news_") {
                    let engine_lang = e.name.rsplit('_').next().unwrap_or("");
                    return engine_lang == lang_suffix;
                }
                // Skip other non-localized news engines (they're in a single language)
                // unless they are clearly language-agnostic (ap_news, rsshub, hn_search)
                if e.categories.iter().any(|c| c == "news") {
                    let is_agnostic = e.name == "ap_news" || e.name.starts_with("rsshub_") || e.name == "hn_search";
                    if !is_agnostic {
                        return false;
                    }
                }
            }
            true
        })
        .collect();
    let custom_tasks = custom_selected.iter().map(|spec| {
        let timeout = spec
            .timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(default_timeout);
        let name = spec.name.clone();
        let spec = (*spec).clone();
        let query_text = query_text.clone();
        let language = language.clone();
        let time_range = time_range.clone();
        let discover_category = discover_category.clone();
        let country = country.clone();
        async move {
            let started = Instant::now();
            let mut outcome = Ok(Err("not attempted".to_string()));
            for attempt in 0..=max_retries {
                if attempt > 0 {
                    tokio::time::sleep(retry::backoff(attempt)).await;
                }
                let ctx = EngineContext {
                    client: rt.client_for(attempt),
                    query: &query_text,
                    lang: &language,
                    safe_search,
                    timeout,
                    max_results,
                    pageno,
                    time_range: time_range.as_deref(),
                    base_url: None,
                    api_key: spec.api_key.as_deref(),
                    extra: None,
                    custom: Some(&spec),
                    allow_private_urls,
                    category: discover_category.as_deref(),
                    country: country.as_deref(),
                };
                // Rate-limit wait inside the timeout budget (see native loop).
                let key = engines::rate_limit_key(&name);
                outcome = tokio::time::timeout(timeout, async {
                    rt.limiter.acquire(key).await;
                    engines::run(&name, &ctx).await
                })
                .await;
                match &outcome {
                    Ok(Ok(_)) => break,
                    Ok(Err(reason)) if !retry::is_retryable(reason) => break,
                    _ => {
                        if attempt < max_retries {
                            obs::debug(format!("retrying engine={name} attempt={}", attempt + 1));
                        }
                    }
                }
            }
            let ms = started.elapsed().as_millis();
            (name, outcome, ms)
        }
    });

    // Overall hard deadline as a safety net. Each engine task is already
    // individually bounded (rate-limit wait + request both sit inside the
    // per-engine timeout, times `max_retries + 1` plus backoff), so this should
    // never fire in practice — but it guarantees a search can never hang the UI
    // even if some future ignores its own timeout. On expiry we degrade to no
    // outcomes from that batch (results still come from whatever completed in
    // the other batch) rather than blocking forever.
    let per_attempt = settings.server.request_timeout_secs.max(1);
    let overall_deadline = Duration::from_secs(
        (per_attempt * (max_retries as u64 + 1))
            .saturating_add(15)
            .max(30),
    );
    let outcomes =
        match tokio::time::timeout(overall_deadline, futures_util::future::join_all(tasks)).await {
            Ok(v) => v,
            Err(_) => {
                obs::warn("search fan-out exceeded overall deadline");
                Vec::new()
            }
        };
    let custom_outcomes = match tokio::time::timeout(
        overall_deadline,
        futures_util::future::join_all(custom_tasks),
    )
    .await
    {
        Ok(v) => v,
        Err(_) => {
            obs::warn("custom-engine fan-out exceeded overall deadline");
            Vec::new()
        }
    };
    let outcomes = outcomes.into_iter().chain(custom_outcomes);

    let mut per_engine = Vec::new();
    let mut infoboxes: Vec<Infobox> = Vec::new();
    let mut suggestions: Vec<String> = Vec::new();
    let mut corrections: Vec<String> = Vec::new();
    let mut engine_answers: Vec<Answer> = Vec::new();
    let mut timings: Vec<EngineTiming> = Vec::new();

    let mut unresponsive = cooled;
    for (name, outcome, ms) in outcomes {
        match outcome {
            Ok(Ok(resp)) => {
                let count = resp.results.len();
                if count == 0
                    && settings.server.empty_result_tracking
                    && crate::engines::empty_result_sensitive(&name)
                {
                    let reason = "empty results (possible selector drift)".to_string();
                    rt.limiter.reward(engines::rate_limit_key(&name));
                    rt.health.record_failure(&name, FailureClass::EmptyResults);
                    rt.record(&name, ms, 0, true);
                    obs::engine_result(&name, "empty", 0, ms);
                    timings.push(EngineTiming {
                        engine: name.clone(),
                        total_ms: ms,
                        results: 0,
                    });
                    unresponsive.push((name, reason));
                    continue;
                }
                rt.limiter.reward(engines::rate_limit_key(&name));
                rt.health.record_success(&name);
                rt.record(&name, ms, count, false);
                obs::engine_result(&name, "ok", count, ms);
                timings.push(EngineTiming {
                    engine: name.clone(),
                    total_ms: ms,
                    results: count,
                });
                infoboxes.extend(resp.infoboxes);
                suggestions.extend(resp.suggestions);
                corrections.extend(resp.corrections);
                engine_answers.extend(resp.answers);
                per_engine.push((name, resp.results));
            }
            Ok(Err(reason)) => {
                if reason.contains("429") {
                    rt.limiter.penalize(engines::rate_limit_key(&name));
                }
                rt.health
                    .record_failure(&name, FailureClass::classify(&reason));
                rt.record(&name, ms, 0, true);
                obs::engine_result(&name, "error", 0, ms);
                unresponsive.push((name, reason));
            }
            Err(_) => {
                rt.health.record_failure(&name, FailureClass::Timeout);
                rt.record(&name, ms, 0, true);
                obs::engine_result(&name, "timeout", 0, ms);
                unresponsive.push((name, "timeout".to_string()));
            }
        }
    }

    // 5. Aggregate + score, then apply the optional domain-trust authority
    //    weighting (no-op with an empty trust map → pure positional order).
    //    Engine weights pick up the local personalization boost when enabled.
    let mut weights = settings.weights();
    if let Some(p) = &rt.personalization {
        p.apply_boost(&mut weights);
    }
    let mut results: Vec<SearchResult> = aggregate(per_engine, &weights);

    // 5a. Apply domain trust: use configured domains, or fall back to defaults.
    let trust = if settings.search.domain_trust.is_empty() {
        default_trusted_domains()
    } else {
        settings.search.domain_trust.clone()
    };
    apply_domain_trust(&mut results, &trust);

    // 5b. Extended ranking signals: title match boost, image boost, cross-domain
    //     dedup penalty, freshness for news. Pure, no-AI, cheap.
    let is_news = categories.iter().any(|c| c == "news");
    let ranking_config = RankingConfig::default();
    apply_extended_ranking(&mut results, &query_text, &ranking_config, is_news);

    // 5c. Source-highlight + summary metadata (no-AI, cheap): mark which query
    //     terms appear in each snippet and add a one-line extractive summary.
    annotate_results(&mut results, &query_text);

    // 6. Instant answers: offline widgets, then network-backed answerers
    //    (currency / dictionary), then any answers passed through by engines
    //    (e.g. an upstream search instance).
    let mut answers = answerers::answer(&query_text);
    let answer_timeout = Duration::from_secs(settings.server.request_timeout_secs.max(1));
    answers.extend(answerers::answer_online(&query_text, &rt.client, answer_timeout).await);
    answers.extend(engine_answers);

    // 7. Optional AI enhancements (opt-in; degrade gracefully).
    //
    // Answer synthesis has two ways in: the `ai.enabled` master switch (with
    // `ai.answer` or `?ai=1`), OR an *explicit* per-request `?ai=1` toggle. The
    // explicit toggle is itself the opt-in — a user clicking "✨ AI answer"
    // consents to contacting the configured (local, by default 127.0.0.1:11434)
    // model for that one request, so the button works out of the box even if an
    // admin left the master switch off. It still degrades to no answer card when
    // no model is reachable. Privacy is preserved: the only outbound AI
    // destination remains the user-configured `ai.base_url`, query text is never
    // logged, and nothing fires unless the user asked for it. The heavier,
    // always-on enhancements (rerank / cluster / expand / vision) stay strictly
    // behind the master switch.
    let explicit_answer = params.ai_answer == Some(true);
    let do_rerank = params.rerank.unwrap_or(settings.ai.rerank);
    if settings.ai.enabled {
        if do_rerank {
            ai::rerank(
                &settings.ai,
                &rt.client,
                &query_text,
                &mut results,
                Some(&rt.embed_cache),
            )
            .await;
            // Re-score positions are no longer meaningful after rerank; keep
            // existing scores but the order reflects semantic similarity.
        }
        if settings.ai.cluster {
            ai::cluster(
                &settings.ai,
                &rt.client,
                &mut results,
                Some(&rt.embed_cache),
            )
            .await;
        }
    }
    if ai_answer && (settings.ai.enabled || explicit_answer) {
        if let Some(a) =
            ai::synthesize_answer(&settings.ai, &rt.client, &query_text, &results).await
        {
            answers.insert(0, a);
        }
    }
    if settings.ai.enabled {
        if settings.ai.expand {
            for s in ai::expand_query(&settings.ai, &rt.client, &query_text).await {
                if !suggestions.iter().any(|x| x.eq_ignore_ascii_case(&s)) {
                    suggestions.push(s);
                }
            }
        }
        // Multimodal: caption the top few image results with a vision model.
        if settings.ai.vision {
            let mut captioned = 0;
            for r in results.iter_mut() {
                if captioned >= 3 {
                    break;
                }
                if r.template != "images.html" {
                    continue;
                }
                let src = if !r.img_src.is_empty() {
                    r.img_src.clone()
                } else {
                    r.thumbnail.clone()
                };
                if src.is_empty() {
                    continue;
                }
                if let Some(caption) = ai::caption_image(&settings.ai, &rt.client, &src).await {
                    r.summary = Some(caption);
                    captioned += 1;
                }
            }
        }
    }

    // 8. Favicons (extension).
    apply_favicons(&mut results, &settings.search.favicon_resolver);

    dedup_strings(&mut suggestions);
    dedup_strings(&mut corrections);

    // Filter stale news for time-sensitive queries (weather, stock).
    // Keep only very recent news (1-2 hours) for real-time queries
    if is_time_sensitive_query(&answers) {
        filter_stale_news(&mut results, 2.0);
    }

    let response = SearchResponse {
        number_of_results: results.len(),
        pageno,
        results,
        answers,
        corrections,
        infoboxes,
        suggestions,
        unresponsive_engines: unresponsive,
        timings,
        query: query_text,
        deep_subqueries: Vec::new(),
        cache_hit: false,
    };

    rt.cache.put(cache_key, response.clone());
    response
}

/// Multi-hop deep research: search the primary query plus 2–4 LLM-planned
/// sub-queries, merge/de-dupe results (cap 40), then run the usual AI layer once.
async fn deep_search_all(
    params: &SearchParams,
    settings: &Settings,
    rt: &Runtime,
) -> SearchResponse {
    const DEEP_CAP: usize = 40;

    let mut inner = params.clone();
    inner.deep = Some(false);
    inner.ai_answer = Some(false);
    inner.rerank = Some(false);

    let mut primary = search_once(&inner, settings, rt).await;

    let subqueries = if let Some(pre) = &params.deep_subqueries {
        pre.clone()
    } else if settings.ai.enabled {
        ai::plan_subqueries(&settings.ai, &rt.client, &primary.query).await
    } else {
        Vec::new()
    };
    primary.deep_subqueries = subqueries.clone();

    let mut lists = vec![primary.results.clone()];
    for sq in subqueries {
        if sq.eq_ignore_ascii_case(&primary.query) {
            continue;
        }
        let mut p = inner.clone();
        p.query = sq;
        let resp = search_once(&p, settings, rt).await;
        primary.timings.extend(resp.timings);
        for (eng, reason) in resp.unresponsive_engines {
            if !primary.unresponsive_engines.iter().any(|(e, _)| e == &eng) {
                primary.unresponsive_engines.push((eng, reason));
            }
        }
        primary.suggestions.extend(resp.suggestions);
        primary.corrections.extend(resp.corrections);
        lists.push(resp.results);
    }

    primary.results = merge_aggregated(lists, DEEP_CAP);
    primary.number_of_results = primary.results.len();

    // Apply domain trust: use configured domains, or fall back to defaults.
    let trust = if settings.search.domain_trust.is_empty() {
        default_trusted_domains()
    } else {
        settings.search.domain_trust.clone()
    };
    apply_domain_trust(&mut primary.results, &trust);

    // Extended ranking for deep search.
    let is_news = params
        .categories
        .iter()
        .any(|c| c == "news");
    let ranking_config = RankingConfig::default();
    apply_extended_ranking(&mut primary.results, &primary.query, &ranking_config, is_news);

    annotate_results(&mut primary.results, &primary.query);

    let ai_answer = params.ai_answer.unwrap_or(settings.ai.answer);
    let explicit_answer = params.ai_answer == Some(true);
    let do_rerank = params
        .rerank
        .unwrap_or(settings.ai.rerank || params.deep == Some(true));

    if settings.ai.enabled {
        if do_rerank {
            ai::rerank(
                &settings.ai,
                &rt.client,
                &primary.query,
                &mut primary.results,
                Some(&rt.embed_cache),
            )
            .await;
        }
        if settings.ai.cluster {
            ai::cluster(
                &settings.ai,
                &rt.client,
                &mut primary.results,
                Some(&rt.embed_cache),
            )
            .await;
        }
    }
    if ai_answer && (settings.ai.enabled || explicit_answer) {
        primary.answers.retain(|a| a.template != "answer/ai.html");
        if let Some(a) =
            ai::synthesize_answer(&settings.ai, &rt.client, &primary.query, &primary.results).await
        {
            primary.answers.insert(0, a);
        }
    }
    if settings.ai.enabled && settings.ai.expand {
        for s in ai::expand_query(&settings.ai, &rt.client, &primary.query).await {
            if !primary
                .suggestions
                .iter()
                .any(|x| x.eq_ignore_ascii_case(&s))
            {
                primary.suggestions.push(s);
            }
        }
    }

    apply_favicons(&mut primary.results, &settings.search.favicon_resolver);
    dedup_strings(&mut primary.suggestions);
    dedup_strings(&mut primary.corrections);

    primary
}

/// Fetch autocomplete suggestions for a query prefix.
pub async fn autocomplete(query: &str, settings: &Settings, rt: &Runtime) -> Vec<String> {
    let q = query.trim();
    if q.is_empty() || settings.search.autocomplete.is_empty() {
        return Vec::new();
    }
    let timeout = Duration::from_secs(settings.server.request_timeout_secs.max(1));
    engines::autocomplete(
        &settings.search.autocomplete,
        &rt.client,
        q,
        &settings.search.default_lang,
        timeout,
    )
    .await
}

/// Resolve the effective search **locale** for one request, honoring the
/// query's own language so a Korean query returns Korean / Korea-region results
/// without any explicit parameter.
///
/// Resolution order:
/// 1. An explicit request language — `?language=` or a `:lang` token — wins,
///    *unless* it is the standard sentinel `auto` (re-detect) or
///    `all`/`any` (no constraint).
/// 2. Otherwise [`SearchSettings::default_language`] decides: `auto` detects the
///    query script ([`crate::ai::detect_locale`]); `all`/`any` means no
///    constraint; any other value is a fixed locale.
///
/// `auto` with an inconclusive query, and `all`/`any`, both fall back to the
/// concrete [`SearchSettings::default_lang`] (e.g. `en`), which is what engines
/// use when they need a single language code.
pub(crate) fn resolve_language(
    explicit: Option<String>,
    query: &str,
    search: &crate::config::SearchSettings,
) -> String {
    // The directive is the explicit request language if present, else the
    // configured default. Both share the same `auto`/`all` vocabulary.
    let directive = explicit
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| search.default_language.clone());

    match directive.trim().to_ascii_lowercase().as_str() {
        "" | "all" | "any" | "global" => search.default_lang.clone(),
        "auto" => crate::ai::detect_locale(query)
            .map(str::to_string)
            .unwrap_or_else(|| search.default_lang.clone()),
        _ => directive.trim().to_string(),
    }
}

/// Annotate results with source-highlight metadata (which query terms appear in
/// the snippet) and a short extractive summary (the snippet's first sentence).
/// Pure, offline, and cheap; runs for every search regardless of AI settings.
fn annotate_results(results: &mut [SearchResult], query: &str) {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|t| {
            t.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|t| t.len() >= 3)
        .collect();
    for r in results.iter_mut() {
        let hay = format!("{} {}", r.title, r.content).to_lowercase();
        let mut hl: Vec<String> = Vec::new();
        for t in &terms {
            if hay.contains(t.as_str()) && !hl.contains(t) {
                hl.push(t.clone());
            }
        }
        r.highlights = hl;
        // Extractive one-line summary: first sentence of the snippet, when the
        // snippet is long enough to be worth condensing.
        let content = r.content.trim();
        if content.chars().count() > 140 {
            let first = content
                .split_inclusive(['.', '!', '?'])
                .next()
                .unwrap_or(content)
                .trim();
            if !first.is_empty() && first.chars().count() < content.chars().count() {
                r.summary = Some(first.to_string());
            }
        }
    }
}

fn dedup_strings(v: &mut Vec<String>) {
    let mut seen = Vec::new();
    v.retain(|s| {
        let k = s.to_lowercase();
        if seen.contains(&k) {
            false
        } else {
            seen.push(k);
            true
        }
    });
}

/// Check if the query is time-sensitive (weather, stock) based on instant answers.
fn is_time_sensitive_query(answers: &[Answer]) -> bool {
    for a in answers {
        let txt = &a.answer;
        // Weather: temperature + humidity pattern
        if txt.contains("°C") && txt.contains("km/h") {
            return true;
        }
        // Stock price indicators
        if txt.contains("KRW") || txt.contains("▲") || txt.contains("▼") {
            return true;
        }
    }
    false
}

/// Filter out news results older than `max_hours` based on published_date.
fn filter_stale_news(results: &mut Vec<SearchResult>, max_hours: f64) {
    results.retain(|r| {
        // Only filter news results (those with published_date)
        if let Some(ref date_str) = r.published_date {
            if let Some(age_hours) = parse_relative_time(date_str) {
                return age_hours <= max_hours;
            }
        }
        true // Keep results without published_date
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SearchSettings;

    fn search_settings(default_language: &str) -> SearchSettings {
        SearchSettings {
            default_language: default_language.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn explicit_language_always_wins() {
        let s = search_settings("auto");
        // Explicit `language=en` must keep English even for a Korean query.
        assert_eq!(resolve_language(Some("en".into()), "양자컴퓨팅", &s), "en");
        assert_eq!(resolve_language(Some("ko-KR".into()), "tesla", &s), "ko-KR");
    }

    #[test]
    fn auto_detects_korean_query() {
        let s = search_settings("auto");
        // No explicit language + Hangul query → Korean locale (the core fix).
        assert_eq!(resolve_language(None, "양자컴퓨팅", &s), "ko-KR");
        // Latin query is ambiguous → falls back to the concrete default.
        assert_eq!(resolve_language(None, "quantum computing", &s), "en");
    }

    #[test]
    fn all_and_explicit_auto_behave() {
        // `all` means "no constraint" → concrete default, even for Korean.
        let s = search_settings("all");
        assert_eq!(resolve_language(None, "양자컴퓨팅", &s), "en");
        // Explicit `language=all` overrides an auto config back to the default.
        let s = search_settings("auto");
        assert_eq!(resolve_language(Some("all".into()), "양자컴퓨팅", &s), "en");
        // Explicit `language=auto` re-detects even when config is fixed.
        let s = search_settings("de");
        assert_eq!(
            resolve_language(Some("auto".into()), "양자컴퓨팅", &s),
            "ko-KR"
        );
    }

    #[test]
    fn fixed_config_language_is_forced() {
        let s = search_settings("de");
        assert_eq!(resolve_language(None, "quantum computing", &s), "de");
    }
}
