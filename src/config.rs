//! Configuration, analogous to the standard `settings.yml`.
//!
//! Engines are enabled/disabled and weighted here. A missing config file is not
//! an error: [`Settings::load`] falls back to sane defaults (the keyless engine
//! set implemented in this crate). [`Settings::validate`] rejects obviously
//! broken configs (unknown engines, negative weights, port 0, an enabled
//! proxy engine with no base URL).

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Settings {
    /// Run mode: `full` (default) or `proxy`. Proxy mode serves external-engine
    /// search only — no feed polling, no embedding worker, and nothing written
    /// to disk (discover-cache persistence and disk caches are disabled too).
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default)]
    pub search: SearchSettings,
    #[serde(default)]
    pub server: ServerSettings,
    /// Optional LLM-backed features (answer synthesis, query expansion,
    /// semantic re-ranking, clustering). Entirely opt-in.
    #[serde(default)]
    pub ai: AiSettings,
    /// Branding customization (app name, logo, favicon).
    #[serde(default)]
    pub branding: BrandingSettings,
    /// Standalone RSS feeds settings.
    #[serde(default)]
    pub feeds: FeedsSettings,
    /// Engine definitions. When empty, [`Settings::ensure_defaults`] fills in
    /// the built-in default engines.
    #[serde(default)]
    pub engines: Vec<EngineSettings>,
    /// Config-driven generic engine adapters (RSS/Atom, OpenSearch, JSON-API
    /// templates) declared entirely in `settings.yml` — no Rust code per source.
    /// Empty by default, so default behaviour is unchanged. See
    /// [`CustomEngine`] and `docs/custom-engines.md`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_engines: Vec<CustomEngine>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SearchSettings {
    #[serde(default = "default_formats")]
    pub formats: Vec<String>,
    /// The locale used by engines when a request neither carries an explicit
    /// `language=` parameter nor a `:lang` token, and auto-detection is
    /// inconclusive. This is the *concrete* fallback language code (e.g. `en`).
    #[serde(default = "default_lang")]
    pub default_lang: String,
    /// How the per-request search locale is chosen when the request does not
    /// specify one (compatible `language` semantics):
    /// * `auto` (default) — detect the query's script and pick the matching
    ///   locale (Hangul → `ko-KR`, kana → `ja-JP`, …); fall back to
    ///   [`Self::default_lang`] when detection is inconclusive.
    /// * `all` / `any` — no language constraint; use [`Self::default_lang`].
    /// * a fixed locale (`ko`, `ko-KR`, `de`, …) — always force that locale.
    ///
    /// An explicit per-request `language=` (or `:lang` token) always overrides
    /// this. Keyless-first and gracefully degrading: a Korean query returns
    /// Korean results out of the box, with no UI or API key required.
    #[serde(default = "default_language")]
    pub default_language: String,
    /// 0 = off, 1 = moderate, 2 = strict (best-effort; passed to engines that support it).
    #[serde(default)]
    pub safe_search: u8,
    /// Categories searched when a request does not specify any.
    #[serde(default = "default_search_categories")]
    pub default_categories: Vec<String>,
    /// Engine used by the `/autocompleter` endpoint ("duckduckgo" | "wikipedia" | "" to disable).
    #[serde(default = "default_autocomplete")]
    pub autocomplete: String,
    /// Resolve favicons for result domains via this service ("" to disable).
    /// `{domain}` is substituted; default uses DuckDuckGo's icon service.
    #[serde(default = "default_favicon_resolver")]
    pub favicon_resolver: String,
    /// Per-domain authority/trust multipliers applied to result scores after
    /// the positional positional score. A no-AI ranking signal: results on
    /// trusted domains float up, low-trust domains sink. Empty by default
    /// (pure positional scoring, exactly standard).
    #[serde(default)]
    pub domain_trust: Vec<DomainTrust>,
    /// Local, privacy-preserving personalization: learn which engines you use
    /// most (via `!bang`s) and gently boost their ranking. Counts are stored
    /// crate-side on disk; query text is never recorded. Off by default.
    #[serde(default)]
    pub personalization: bool,
    /// Curation knobs for the Discover / News digest feed (freshness, source
    /// diversity, near-duplicate clustering). Pure, no-AI, no-network; degrades
    /// gracefully. See [`NewsSettings`].
    #[serde(default)]
    pub news: NewsSettings,
}

/// Tunable knobs for curating the Discover / News digest feed: a selection &
/// ranking layer applied on top of the base positional search score. Every knob
/// has a sensible default, and the whole feature is pure (no AI / no network),
/// so it never adds a hard dependency on Ollama or any engine.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NewsSettings {
    /// Max items from a single source/host allowed in the curated feed, so one
    /// outlet can't dominate the hero + cards. `0` disables the cap. Aggregator
    /// redirect hosts (e.g. Google News) are de-aggregated to the underlying
    /// publisher before counting, so diversity is by real source.
    #[serde(default = "default_news_per_source_cap")]
    pub per_source_cap: usize,
    /// Half-life (hours) of the recency decay applied to *dated* articles: an
    /// article this old keeps half of its freshness factor. Smaller = more
    /// aggressively favours breaking news.
    #[serde(default = "default_news_half_life_hours")]
    pub freshness_half_life_hours: f64,
    /// How strongly recency reweights the base score, in `0.0..=1.0`
    /// (`0` = ignore recency entirely, `1` = recency dominates ranking).
    #[serde(default = "default_news_freshness_weight")]
    pub freshness_weight: f64,
    /// Title token-overlap (Jaccard, `0.0..=1.0`) at/above which two items are
    /// treated as the same story and collapsed to a single card (cross-outlet
    /// near-duplicate clustering). `1.0` effectively disables title clustering.
    #[serde(default = "default_news_dedup_similarity")]
    pub dedup_title_similarity: f64,
    /// TTL (seconds) of the in-memory digest cache, so re-visited categories
    /// return instantly instead of re-running the search + image enrichment.
    /// `0` disables digest caching. Default 180 (3 min).
    #[serde(default = "default_news_cache_ttl_secs")]
    pub cache_ttl_secs: u64,
    /// TTL (hours) of the Discover category snapshot cache. This is intentionally
    /// much longer than `cache_ttl_secs`: Discover is curated once, kept stable
    /// for the day, and refreshed via `?refresh=1` or cache clear.
    #[serde(default = "default_discover_cache_ttl_hours")]
    pub discover_cache_ttl_hours: u64,
    /// Discover-specific freshness cutoff in hours. Snapshot feeds should be
    /// last-1-to-2-days news, not the broader News search fallback window.
    /// `0` disables the Discover cutoff. Default 48.
    #[serde(default = "default_discover_max_age_hours")]
    pub discover_max_age_hours: u64,
    /// Minimum number of image-bearing cards the snapshot builder should try to
    /// produce before falling back to small-card favicons in the UI.
    #[serde(default = "default_discover_min_image_count")]
    pub discover_min_image_count: usize,
    /// Max image-less cards to enrich with an `og:image` page fetch per request
    /// (bounds the slow part of a cold feed). `0` disables enrichment. Default 8
    /// so the whole visible feed (hero + grid) can get a real image, not just
    /// the first row.
    #[serde(default = "default_news_enrich_max")]
    pub enrich_max: usize,
    /// Max concurrent `og:image` page fetches. Bounds outbound load while still
    /// fetching the visible feed in parallel so a few slow publishers don't
    /// serialise the batch. Default 8.
    #[serde(default = "default_news_enrich_concurrency")]
    pub enrich_concurrency: usize,
    /// Strict per-fetch timeout (ms) for a single `og:image` enrichment fetch.
    /// Default 3500 — generous enough for slow publisher pages / aggregator
    /// redirects (e.g. Google News) without letting one page stall the batch.
    #[serde(default = "default_news_enrich_timeout_ms")]
    pub enrich_timeout_ms: u64,
    /// Total wall-clock budget (ms) for the whole enrichment batch, so a cold
    /// feed is bounded even when several article pages are slow. Fetches that
    /// finish within the budget are applied even if a few stragglers don't —
    /// the budget never discards already-completed results. Default 6000.
    #[serde(default = "default_news_enrich_budget_ms")]
    pub enrich_budget_ms: u64,
    /// Hard recency cutoff (days): news items whose trusted publish timestamp is
    /// older than this are DROPPED, and items we can't confidently date as
    /// recent (no date, or only an unreliable last-edit proxy) are treated as
    /// not-known-recent and excluded too. `0` disables the cutoff (soft
    /// freshness ranking only). Default 14 — two weeks reads as "recent news"
    /// while tolerating slower news cycles; the fallback below widens it when a
    /// query is too quiet to fill the feed.
    #[serde(default = "default_news_max_age_days")]
    pub max_age_days: u64,
    /// Fallback threshold: if the hard cutoff leaves fewer than this many recent
    /// items, relax and backfill with the best-available older/undated items
    /// (recent always first) rather than returning a near-empty feed. Default 3.
    #[serde(default = "default_news_min_results")]
    pub min_results: usize,
    /// Categories to show in Discover feed. Empty = use default categories.
    /// Available: news, politics, business, finance, tech, world, sports,
    /// entertainment, health, science, culture, opinion, lifestyle, auto
    #[serde(default)]
    pub discover_categories: Vec<String>,
    /// Number of articles per category in Discover feed. Default 8.
    #[serde(default = "default_discover_articles_per_category")]
    pub discover_articles_per_category: usize,
}

fn default_news_per_source_cap() -> usize {
    2
}
fn default_news_half_life_hours() -> f64 {
    18.0
}
fn default_news_freshness_weight() -> f64 {
    0.5
}
fn default_news_dedup_similarity() -> f64 {
    0.6
}
fn default_news_cache_ttl_secs() -> u64 {
    180
}
fn default_discover_cache_ttl_hours() -> u64 {
    2
}
fn default_discover_max_age_hours() -> u64 {
    48
}
fn default_discover_min_image_count() -> usize {
    5
}
fn default_news_enrich_max() -> usize {
    8
}
fn default_news_enrich_concurrency() -> usize {
    8
}
fn default_news_enrich_timeout_ms() -> u64 {
    3500
}
fn default_news_enrich_budget_ms() -> u64 {
    6000
}
fn default_news_max_age_days() -> u64 {
    14
}
fn default_discover_articles_per_category() -> usize {
    30
}
fn default_news_min_results() -> usize {
    3
}

impl Default for NewsSettings {
    fn default() -> Self {
        NewsSettings {
            per_source_cap: default_news_per_source_cap(),
            freshness_half_life_hours: default_news_half_life_hours(),
            freshness_weight: default_news_freshness_weight(),
            dedup_title_similarity: default_news_dedup_similarity(),
            cache_ttl_secs: default_news_cache_ttl_secs(),
            discover_cache_ttl_hours: default_discover_cache_ttl_hours(),
            discover_max_age_hours: default_discover_max_age_hours(),
            discover_min_image_count: default_discover_min_image_count(),
            enrich_max: default_news_enrich_max(),
            enrich_concurrency: default_news_enrich_concurrency(),
            enrich_timeout_ms: default_news_enrich_timeout_ms(),
            enrich_budget_ms: default_news_enrich_budget_ms(),
            max_age_days: default_news_max_age_days(),
            min_results: default_news_min_results(),
            discover_categories: Vec::new(),
            discover_articles_per_category: default_discover_articles_per_category(),
        }
    }
}

/// A single `domain -> weight` authority entry. `weight > 1.0` boosts a
/// domain's results; `0.0..1.0` demotes them. Matched against the result host
/// (and any parent domain, so `wikipedia.org` covers `en.wikipedia.org`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DomainTrust {
    pub domain: String,
    pub weight: f64,
}

/// Per-IP rate limiting settings.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RateLimitSettings {
    /// Enable rate limiting. Default true.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Max requests per minute to /search. Default 60.
    #[serde(default = "default_rate_limit_search")]
    pub search_requests_per_minute: u32,
    /// Max requests per minute to /preferences. Default 10.
    #[serde(default = "default_rate_limit_preferences")]
    pub preferences_requests_per_minute: u32,
    /// Window size in seconds for rate limit tracking. Default 60.
    #[serde(default = "default_rate_limit_window")]
    pub window_secs: u64,
}

fn default_rate_limit_search() -> u32 {
    60
}
fn default_rate_limit_preferences() -> u32 {
    10
}
fn default_rate_limit_window() -> u64 {
    60
}

impl Default for RateLimitSettings {
    fn default() -> Self {
        RateLimitSettings {
            enabled: true,
            search_requests_per_minute: default_rate_limit_search(),
            preferences_requests_per_minute: default_rate_limit_preferences(),
            window_secs: default_rate_limit_window(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerSettings {
    #[serde(default = "default_bind")]
    pub bind_address: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Per-IP rate limiting configuration.
    #[serde(default)]
    pub rate_limit: RateLimitSettings,
    /// Per-engine network timeout in seconds.
    #[serde(default = "default_timeout")]
    pub request_timeout_secs: u64,
    /// Cap on results requested from each engine.
    #[serde(default = "default_max_results")]
    pub max_results_per_engine: usize,
    /// Max concurrent client connections (back-pressure for the tiny server).
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,
    /// Drop a client connection whose request headers don't arrive within N secs.
    #[serde(default = "default_read_timeout")]
    pub read_timeout_secs: u64,
    /// Result cache time-to-live in seconds (0 disables caching).
    #[serde(default = "default_cache_ttl")]
    pub cache_ttl_secs: u64,
    /// Minimum interval between requests to the same engine, in milliseconds
    /// (politeness / rate limiting). 0 disables.
    #[serde(default = "default_rate_limit")]
    pub engine_min_interval_ms: u64,
    /// Proxy image thumbnails through `/image_proxy` so clients never hit upstreams.
    #[serde(default = "default_true")]
    pub image_proxy: bool,
    /// Cache backend: `"memory"` (default), `"disk"` (persistent, survives
    /// restarts), or `"redis"` (requires building with `--features redis`).
    #[serde(default = "default_cache_backend")]
    pub cache_backend: String,
    /// Directory for the disk cache backend.
    #[serde(default = "default_cache_dir")]
    pub cache_dir: String,
    /// Connection URL for the optional Redis cache backend.
    #[serde(default = "default_redis_url")]
    pub redis_url: String,
    /// Optional list of upstream HTTP/SOCKS proxy URLs. When non-empty, engine
    /// fetches rotate through these (config-driven proxy rotation) to spread
    /// load and dodge per-IP bot blocks. Empty = direct connections.
    #[serde(default)]
    pub proxies: Vec<String>,
    /// How many times to retry a failed/blocked engine fetch (with exponential
    /// backoff). 0 disables retries.
    #[serde(default = "default_retries")]
    pub max_retries: u32,
    /// Consecutive *hard* failures (bot-block / timeout / transport) after which
    /// an engine is automatically cooled down — skipped in fan-out — for
    /// [`Self::engine_cooldown_secs`], then probed again (probe-recover). A
    /// single successful response resets the counter. `0` disables the
    /// health/cool-down mechanism entirely. Default 5.
    #[serde(default = "default_failure_threshold")]
    pub engine_failure_threshold: u32,
    /// How long (seconds) a cooled-down engine is skipped before it is probed
    /// again. Default 60.
    #[serde(default = "default_cooldown_secs")]
    pub engine_cooldown_secs: u64,
    /// When true, general web scrapers that return HTTP 200 with **zero** results
    /// are treated as soft failures (selector drift / silent bot-block) and
    /// count toward engine cool-down. Reference/API engines that can legitimately
    /// return empty sets are exempt — see [`crate::engines::empty_result_sensitive`].
    #[serde(default = "default_true")]
    pub empty_result_tracking: bool,
    /// When true, custom-engine fetches may target private/loopback hosts.
    /// **Off by default** (SSRF-safe). Integration tests enable this for
    /// loopback fixtures only.
    #[serde(default)]
    pub allow_private_urls: bool,
}

/// LLM-backed enhancements. All default OFF; every feature degrades to the plain
/// engine behaviour when a model is unreachable.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AiSettings {
    /// Master switch. When false, no AI code path runs and no model is contacted.
    #[serde(default)]
    pub enabled: bool,
    /// Ollama-compatible base URL (`/api/generate`, `/api/embeddings`).
    #[serde(default = "default_ai_base")]
    pub base_url: String,
    /// Chat/instruct model for answer synthesis & query expansion.
    #[serde(default = "default_ai_model")]
    pub model: String,
    /// Chat/instruct model used specifically for full-page news article
    /// rewrites. Defaults to the same multilingual model as `model` so answer
    /// synthesis and article rewrites keep consistent structure and
    /// language/script fidelity. When empty, falls back to `model`. A
    /// per-request `?model=` override still wins. Like every AI feature, it
    /// degrades gracefully when the model is unavailable.
    #[serde(default = "default_ai_article_model")]
    pub article_model: String,
    /// Embeddings model for semantic re-ranking.
    #[serde(default = "default_ai_embed_model")]
    pub embedding_model: String,
    /// Synthesize a cited answer from the top results by default.
    #[serde(default)]
    pub answer: bool,
    /// LLM/heuristic query expansion feeding `suggestions`.
    #[serde(default)]
    pub expand: bool,
    /// Re-rank results by embedding similarity to the query.
    #[serde(default)]
    pub rerank: bool,
    /// Group results into topic clusters.
    #[serde(default)]
    pub cluster: bool,
    /// Multi-turn conversational refinement: rewrite a follow-up query into a
    /// standalone query using the previous query as context.
    #[serde(default)]
    pub conversational: bool,
    /// Caption/analyse image-category results with a vision model.
    #[serde(default)]
    pub vision: bool,
    /// Vision model name (Ollama-compatible, e.g. `llava`).
    #[serde(default = "default_ai_vision_model")]
    pub vision_model: String,
    /// Number of top results fed into answer synthesis.
    #[serde(default = "default_ai_top_n")]
    pub answer_top_n: usize,
    /// Per-request timeout for model calls (seconds).
    #[serde(default = "default_ai_timeout")]
    pub timeout_secs: u64,
    /// Custom prompt for Korean news article analysis. When empty, uses default.
    /// Placeholders: {title}, {excerpt}
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub news_prompt_ko: String,
    /// Custom prompt for English news article analysis. When empty, uses default.
    /// Placeholders: {title}, {excerpt}
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub news_prompt_en: String,
    /// Target language for AI responses (news rewrite, answers).
    /// "auto" = detect from content, or specify: "en", "ko", "ja", "zh", "es", "fr", "de", etc.
    #[serde(default = "default_answer_language")]
    pub answer_language: String,
    /// API key for OpenAI-compatible providers (Groq, Together, etc.).
    /// Used in Authorization header. Leave empty for local Ollama.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Cost per 1M input tokens (USD). Set to enable cost tracking.
    /// Examples: GPT-4o: 2.50, Claude 3.5: 3.00, Gemini 1.5 Pro: 1.25
    #[serde(default)]
    pub input_cost_per_million: f64,
    /// Cost per 1M output tokens (USD). Set to enable cost tracking.
    /// Examples: GPT-4o: 10.00, Claude 3.5: 15.00, Gemini 1.5 Pro: 5.00
    #[serde(default)]
    pub output_cost_per_million: f64,
    /// Track and display token usage statistics.
    #[serde(default)]
    pub track_usage: bool,
    /// Days to retain chat history (0 = no limit). Default 30.
    #[serde(default = "default_chat_retention_days")]
    pub chat_retention_days: u32,
}

fn default_chat_retention_days() -> u32 { 30 }

/// Branding customization for the UI: app name, logo, favicon.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BrandingSettings {
    /// Application name displayed in the UI header and page titles.
    #[serde(default = "default_app_name")]
    pub app_name: String,
    /// URL for the logo image (header). None uses the default SVG logo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo_url: Option<String>,
    /// URL for the favicon. None uses browser default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub favicon_url: Option<String>,
}

fn default_app_name() -> String {
    "Orgos".into()
}

impl Default for BrandingSettings {
    fn default() -> Self {
        BrandingSettings {
            app_name: default_app_name(),
            logo_url: None,
            favicon_url: None,
        }
    }
}

/// Standalone RSS feeds settings.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FeedsSettings {
    /// Enable built-in RSS feed polling (standalone mode).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Days to retain articles before auto-cleanup (default: 7).
    #[serde(default = "default_feeds_retention_days")]
    pub retention_days: u32,
    /// Poll interval in minutes (default: 15).
    #[serde(default = "default_feeds_poll_interval")]
    pub poll_interval_mins: u64,
    /// Languages to poll feeds for. Empty = poll all available.
    #[serde(default)]
    pub languages: Vec<String>,
    /// Disabled feed URLs (user can disable specific feeds from the pool).
    #[serde(default)]
    pub disabled_feeds: Vec<String>,
    /// Generate embeddings for indexed articles (needs a local Ollama server
    /// with `bge-m3`). Off by default: embedding whole feeds is heavy and only
    /// worth it on machines with headroom.
    #[serde(default)]
    pub embeddings: bool,
    /// Hard cap on the on-disk article index, in MiB. 0 = unlimited. Enforced
    /// during the hourly cleanup by evicting oldest articles first.
    #[serde(default)]
    pub max_disk_mb: u64,
}

fn default_feeds_retention_days() -> u32 { 7 }
fn default_feeds_poll_interval() -> u64 { 15 }
fn default_mode() -> String { "full".into() }

impl Default for FeedsSettings {
    fn default() -> Self {
        FeedsSettings {
            enabled: true,
            retention_days: default_feeds_retention_days(),
            poll_interval_mins: default_feeds_poll_interval(),
            languages: vec![],
            disabled_feeds: vec![],
            embeddings: false,
            max_disk_mb: 0,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EngineSettings {
    /// Engine identifier; dispatched in `engines::run`.
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Multiplies this engine's contribution to a result's score (standard).
    #[serde(default = "default_weight")]
    pub weight: f64,
    #[serde(default = "default_categories")]
    pub categories: Vec<String>,
    /// Base URL for engines that proxy another service (e.g. `mediawiki_*`).
    #[serde(default)]
    pub base_url: Option<String>,
    /// Optional per-engine timeout override (seconds).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Optional API key for key-based engines (Google, Bing, …). Read from
    /// config or, preferentially, an environment variable so secrets never sit
    /// in a committed file. Never logged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Optional secondary credential / parameter for key-based engines (e.g.
    /// Google Programmable Search Engine `cx` id). Never logged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<String>,
}

/// A config-driven generic engine adapter, instantiated entirely from
/// `settings.yml` (no Rust module per source). Three `type`s are supported:
///
/// * `rss` — fetch an RSS/Atom feed (or feed *search*) URL and parse its items.
/// * `opensearch` — query an OpenSearch endpoint, either by giving a templated
///   search URL directly or by pointing at an OpenSearch description document
///   (`description_url`) to auto-discover the search template.
/// * `json` — fetch a JSON API and map fields into results via simple,
///   dot/bracket JSONPath-like field paths.
///
/// `url_template` placeholders (all optional except `{query}` for `rss`/`json`):
/// `{query}` (URL-encoded), `{query_raw}`, `{lang}`, `{page}`, `{offset}`,
/// `{count}`, `{safe}`, `{api_key}`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CustomEngine {
    /// Unique engine identifier (must not collide with a native engine name).
    pub name: String,
    /// Adapter type: `rss` | `opensearch` | `json`.
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_weight")]
    pub weight: f64,
    #[serde(default = "default_categories")]
    pub categories: Vec<String>,
    /// Request URL with `{query}` (and other) placeholders. Required for `rss`
    /// and `json`; optional for `opensearch` when `description_url` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url_template: Option<String>,
    /// OpenSearch description document URL (`opensearch` only). When set, the
    /// adapter fetches it and discovers the search template automatically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description_url: Option<String>,
    /// JSON only: dot/bracket path to the result array (e.g. `data.items`,
    /// `hits`, `results[0].matches`). Empty/omitted means the body itself is the
    /// array.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_path: Option<String>,
    /// JSON only: per-result path to the link/URL field (required for `json`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url_field: Option<String>,
    /// JSON only: per-result path to the title field (required for `json`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_field: Option<String>,
    /// JSON only: per-result path to the snippet/content field (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_field: Option<String>,
    /// JSON only: per-result path to a thumbnail/image URL field (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail_field: Option<String>,
    /// JSON only: per-result path to a published-date field (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_field: Option<String>,
    /// Optional per-engine timeout override (seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// Optional API key, substituted into `url_template` via `{api_key}`. Never
    /// logged and skipped when serializing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

/// Adapter `type`s accepted in a [`CustomEngine`].
pub const CUSTOM_ENGINE_KINDS: &[&str] = &["rss", "opensearch", "json"];

fn default_formats() -> Vec<String> {
    vec!["html".into(), "json".into(), "rss".into(), "csv".into()]
}
fn default_lang() -> String {
    "en".into()
}
fn default_language() -> String {
    "auto".into()
}
fn default_search_categories() -> Vec<String> {
    vec!["general".into()]
}
fn default_autocomplete() -> String {
    "duckduckgo".into()
}
fn default_favicon_resolver() -> String {
    "https://icons.duckduckgo.com/ip3/{domain}.ico".into()
}
fn default_bind() -> String {
    "127.0.0.1".into()
}
fn default_port() -> u16 {
    8889
}
fn default_timeout() -> u64 {
    // 9s: a 5s global timeout was cutting off slow-but-healthy upstreams under
    // concurrent fan-out (the Wikimedia cluster intermittently failed and
    // Codeberg's body read got cut). 9s sits between the standard 3–10s range and
    // comfortably covers the observed tail latencies while still failing fast
    // enough that one slow engine can't stall a whole search. Override globally
    // (`server.request_timeout_secs`) or per-engine (`timeout_secs`).
    9
}
fn default_max_results() -> usize {
    10
}
fn default_max_connections() -> usize {
    64
}
fn default_read_timeout() -> u64 {
    10
}
fn default_cache_ttl() -> u64 {
    300
}
fn default_rate_limit() -> u64 {
    200
}
fn default_cache_backend() -> String {
    "memory".into()
}
fn default_cache_dir() -> String {
    ".metasearch-cache".into()
}
fn default_redis_url() -> String {
    "redis://127.0.0.1:6379".into()
}
fn default_retries() -> u32 {
    1
}
fn default_failure_threshold() -> u32 {
    5
}
fn default_cooldown_secs() -> u64 {
    60
}
fn default_true() -> bool {
    true
}
fn default_weight() -> f64 {
    1.0
}
fn default_categories() -> Vec<String> {
    vec!["general".into()]
}
fn default_ai_base() -> String {
    "http://127.0.0.1:11434".into()
}
fn default_ai_model() -> String {
    "gemma4:e4b".into()
}
fn default_ai_article_model() -> String {
    // Keep article rewrites aligned with answer synthesis by default; callers
    // can still set this empty to fall back to `model` explicitly.
    "gemma4:e4b".into()
}
fn default_ai_embed_model() -> String {
    "nomic-embed-text".into()
}
fn default_ai_vision_model() -> String {
    "llava".into()
}
fn default_ai_top_n() -> usize {
    5
}
fn default_ai_timeout() -> u64 {
    30
}
fn default_answer_language() -> String {
    "auto".into()
}

impl Default for SearchSettings {
    fn default() -> Self {
        SearchSettings {
            formats: default_formats(),
            default_lang: default_lang(),
            default_language: default_language(),
            safe_search: 0,
            default_categories: default_search_categories(),
            autocomplete: default_autocomplete(),
            favicon_resolver: default_favicon_resolver(),
            domain_trust: Vec::new(),
            personalization: false,
            news: NewsSettings::default(),
        }
    }
}

impl Default for ServerSettings {
    fn default() -> Self {
        ServerSettings {
            bind_address: default_bind(),
            port: default_port(),
            rate_limit: RateLimitSettings::default(),
            request_timeout_secs: default_timeout(),
            max_results_per_engine: default_max_results(),
            max_connections: default_max_connections(),
            read_timeout_secs: default_read_timeout(),
            cache_ttl_secs: default_cache_ttl(),
            engine_min_interval_ms: default_rate_limit(),
            image_proxy: true,
            cache_backend: default_cache_backend(),
            cache_dir: default_cache_dir(),
            redis_url: default_redis_url(),
            proxies: Vec::new(),
            max_retries: default_retries(),
            engine_failure_threshold: default_failure_threshold(),
            engine_cooldown_secs: default_cooldown_secs(),
            empty_result_tracking: true,
            allow_private_urls: false,
        }
    }
}

impl Default for AiSettings {
    fn default() -> Self {
        AiSettings {
            enabled: false,
            base_url: default_ai_base(),
            model: default_ai_model(),
            article_model: default_ai_article_model(),
            embedding_model: default_ai_embed_model(),
            answer: false,
            expand: false,
            rerank: false,
            cluster: false,
            conversational: false,
            vision: false,
            vision_model: default_ai_vision_model(),
            answer_top_n: default_ai_top_n(),
            timeout_secs: default_ai_timeout(),
            news_prompt_ko: String::new(),
            news_prompt_en: String::new(),
            answer_language: default_answer_language(),
            api_key: None,
            input_cost_per_million: 0.0,
            output_cost_per_million: 0.0,
            track_usage: false,
            chat_retention_days: default_chat_retention_days(),
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        let mut s = Settings {
            mode: default_mode(),
            search: SearchSettings::default(),
            server: ServerSettings::default(),
            ai: AiSettings::default(),
            branding: BrandingSettings::default(),
            feeds: FeedsSettings::default(),
            engines: Vec::new(),
            custom_engines: Vec::new(),
        };
        s.ensure_defaults();
        s.apply_env();
        s
    }
}

impl Settings {
    /// The built-in default engine set (keyless engines implemented here).
    pub fn default_engines() -> Vec<EngineSettings> {
        fn e(name: &str, cats: &[&str]) -> EngineSettings {
            EngineSettings {
                name: name.into(),
                enabled: true,
                weight: 1.0,
                categories: cats.iter().map(|c| c.to_string()).collect(),
                base_url: None,
                timeout_secs: None,
                api_key: None,
                extra: None,
            }
        }
        // Opt-in engine: present in the default set but disabled. Key-based or
        // bot-block-prone engines start off and are enabled via config/env.
        fn opt(name: &str, cats: &[&str]) -> EngineSettings {
            let mut s = e(name, cats);
            s.enabled = false;
            s
        }
        vec![
            e("wikipedia", &["general"]),
            e("wikidata", &["general"]),
            e("duckduckgo", &["general", "news"]),
            e("duckduckgo_lite", &["general"]),
            e("brave", &["general"]),
            e("naver", &["general"]),    // Korean web search (scraping)
            e("daum", &["general"]),     // Korean web search (Kakao/Daum)
            e("google_web", &["general"]), // Google web search (scraping)
            e("bing_web", &["general"]),   // Bing web search (scraping)
            // Mojeek's edge returns HTTP 403 to non-browser clients regardless
            // of User-Agent (it gates on a JS/browser challenge), so the scraper
            // can't reach results from a server IP. Shipped opt-in so it doesn't
            // spam the unresponsive-engines banner; enable it only behind a
            // browser-like proxy that can clear the challenge.
            opt("mojeek", &["general"]),
            e("github", &["it"]),
            e("stackexchange", &["it"]),
            e("arxiv", &["science"]),
            e("hackernews", &["it", "news"]),
            e("wikicommons", &["images"]),
            e("duckduckgo_images", &["images"]),
            e("duckduckgo_videos", &["videos"]),
            e("bing_images", &["images"]),
            e("brave_images", &["images"]),
            e("openverse", &["images"]),
            e("archive_music", &["music"]),
            // Code / dev
            e("gitlab", &["it"]),
            e("codeberg", &["it"]),
            e("crates_io", &["it"]),
            e("npm", &["it"]),
            e("packagist", &["it"]),
            e("rubygems", &["it"]),
            e("dockerhub", &["it"]),
            e("askubuntu", &["it"]),
            // Reference
            e("wiktionary", &["general"]),
            e("wikibooks", &["general"]),
            e("wikiquote", &["general"]),
            e("wikisource", &["general"]),
            e("openlibrary", &["general", "files"]),
            e("internetarchive", &["general", "files"]),
            // Science / academic
            e("openalex", &["science"]),
            e("crossref", &["science"]),
            e("europepmc", &["science"]),
            e("semanticscholar", &["science"]),
            e("doaj", &["science"]),
            // News - local RSS feeds only for fast response
            e("local_feeds", &["general", "news", "social"]),
            e("local_news", &["news"]),
            // External news sources (opt-in, slow)
            opt("googlenews", &["news"]),
            opt("wikinews", &["news"]),
            opt("gdelt", &["news"]),
            // Social / video / map
            e("lemmy", &["social"]),
            e("peertube", &["videos"]),
            e("openstreetmap", &["map"]),
            // --- Opt-in keyless web engines (HTML scrapers, bot-block-prone) ---
            opt("startpage", &["general"]),
            // Qwant JSON API: keyless but rejects generic clients (opt-in).
            opt("qwant", &["general"]),
            // --- Opt-in key-based engines (disabled until a key is supplied) ---
            // Google Programmable Search: needs GOOGLE_API_KEY + GOOGLE_CSE_ID.
            opt("google", &["general"]),
            // Bing Web Search v7: needs BING_API_KEY.
            opt("bing", &["general"]),
            // Brave Search API: needs BRAVE_API_KEY.
            opt("brave_api", &["general"]),
            // Yandex Search API (XML): needs YANDEX_API_KEY + YANDEX_FOLDER_ID.
            opt("yandex", &["general"]),
            // Bing News Search v7: reuses BING_API_KEY. News category.
            opt("bingnews", &["news"]),
            // --- Opt-in feature-gated engines (build with --features reddit/marginalia) ---
            opt("reddit", &["social"]),
            opt("marginalia", &["general"]),
        ]
    }

    /// Fill in default engines when none were configured.
    pub fn ensure_defaults(&mut self) {
        if self.engines.is_empty() {
            self.engines = Self::default_engines();
        }
        // Proxy mode promises zero disk writes — a disk result cache would
        // break that, so fall back to the in-memory backend.
        if self.is_proxy_only() && self.server.cache_backend == "disk" {
            self.server.cache_backend = "memory".into();
        }
    }

    /// True when running in proxy-only mode (external-engine search only, no
    /// feed index, no embeddings, no disk writes).
    pub fn is_proxy_only(&self) -> bool {
        self.mode.eq_ignore_ascii_case("proxy")
    }

    /// Overlay environment variables onto the config (called after load).
    ///
    /// * `METASEARCH_AI_BASE_URL` — Ollama-compatible base URL for AI features.
    /// * `OPENAI_API_KEY` or `METASEARCH_AI_API_KEY` — API key for AI features.
    /// * `METASEARCH_MODE` — `full` or `proxy` (see [`Settings::mode`]).
    pub fn apply_env(&mut self) {
        if let Ok(base) = std::env::var("METASEARCH_AI_BASE_URL") {
            let base = base.trim().to_string();
            if !base.is_empty() {
                self.ai.base_url = base;
            }
        }
        // AI API key: prefer OPENAI_API_KEY, fallback to METASEARCH_AI_API_KEY
        if let Ok(key) = std::env::var("OPENAI_API_KEY")
            .or_else(|_| std::env::var("METASEARCH_AI_API_KEY"))
        {
            let key = key.trim().to_string();
            if !key.is_empty() {
                self.ai.api_key = Some(key);
            }
        }
        // AI model override
        if let Ok(model) = std::env::var("METASEARCH_AI_MODEL") {
            let model = model.trim().to_string();
            if !model.is_empty() {
                self.ai.model = model;
            }
        }
        if let Ok(bind) = std::env::var("METASEARCH_BIND") {
            let bind = bind.trim().to_string();
            if !bind.is_empty() {
                self.server.bind_address = bind;
            }
        }
        if let Ok(port) = std::env::var("METASEARCH_PORT") {
            if let Ok(p) = port.trim().parse::<u16>() {
                if p != 0 {
                    self.server.port = p;
                }
            }
        }
        if let Ok(mode) = std::env::var("METASEARCH_MODE") {
            let mode = mode.trim().to_string();
            if !mode.is_empty() {
                self.mode = mode;
            }
        }
        // Re-apply the proxy-mode cache fallback: the env override above can
        // switch the mode after ensure_defaults() already ran.
        if self.is_proxy_only() && self.server.cache_backend == "disk" {
            self.server.cache_backend = "memory".into();
        }
        // Key-based engines: a key in the environment both supplies the
        // credential and auto-enables the engine (it stays off without one).
        self.apply_key_env("google", "GOOGLE_API_KEY", Some("GOOGLE_CSE_ID"));
        self.apply_key_env("bing", "BING_API_KEY", None);
        self.apply_key_env("brave_api", "BRAVE_API_KEY", None);
        self.apply_key_env("yandex", "YANDEX_API_KEY", Some("YANDEX_FOLDER_ID"));
        // Bing News reuses the same subscription key as the Bing web engine.
        self.apply_key_env("bingnews", "BING_API_KEY", None);
        // Comma/whitespace-separated proxy list for config-driven rotation.
        if let Ok(p) = std::env::var("METASEARCH_PROXIES") {
            let list: Vec<String> = p
                .split([',', ' ', '\n'])
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !list.is_empty() {
                self.server.proxies = list;
            }
        }
    }

    /// Read an engine's API key (and optional secondary value) from the
    /// environment, auto-enabling the engine when a key is present.
    fn apply_key_env(&mut self, engine: &str, key_var: &str, extra_var: Option<&str>) {
        let key = std::env::var(key_var).ok().filter(|s| !s.trim().is_empty());
        let Some(key) = key else { return };
        let extra = extra_var
            .and_then(|v| std::env::var(v).ok())
            .filter(|s| !s.trim().is_empty());
        if let Some(eng) = self.engines.iter_mut().find(|e| e.name == engine) {
            eng.api_key = Some(key.trim().to_string());
            if let Some(x) = extra {
                eng.extra = Some(x.trim().to_string());
            }
            eng.enabled = true;
        }
    }

    /// Validate the configuration. Returns a human-readable error on the first
    /// problem found.
    pub fn validate(&self) -> Result<(), String> {
        if self.server.port == 0 {
            return Err("server.port must be in 1..=65535".into());
        }
        if self.engines.is_empty() {
            return Err("no engines configured".into());
        }
        for e in &self.engines {
            if !crate::engines::is_known_engine(&e.name) {
                return Err(format!(
                    "unknown engine '{}' (known: {})",
                    e.name,
                    crate::engines::ENGINE_NAMES.join(", ")
                ));
            }
            if !(e.weight.is_finite() && e.weight >= 0.0) {
                return Err(format!("engine '{}' weight must be >= 0", e.name));
            }
            // Family instances that proxy a remote service need a base_url when
            // enabled (the canonical `lemmy` has a built-in default and is
            // exempt; a labelled `lemmy_<x>` / `mediawiki_<x>` does not).
            let needs_base = crate::engines::is_lemmy_instance(&e.name)
                || crate::engines::is_mediawiki_instance(&e.name);
            if needs_base && e.enabled && e.base_url.as_deref().unwrap_or("").is_empty() {
                return Err(format!(
                    "{} is enabled but has no base_url (set it in config)",
                    e.name
                ));
            }
        }
        if self.search.safe_search > 2 {
            return Err("search.safe_search must be 0, 1, or 2".into());
        }
        // Validate config-driven custom engine adapters.
        let native: std::collections::HashSet<&str> =
            self.engines.iter().map(|e| e.name.as_str()).collect();
        let mut custom_seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for c in &self.custom_engines {
            c.validate()?;
            if native.contains(c.name.as_str()) {
                return Err(format!(
                    "custom engine '{}' collides with a native engine name",
                    c.name
                ));
            }
            if !custom_seen.insert(c.name.as_str()) {
                return Err(format!("duplicate custom engine name '{}'", c.name));
            }
        }
        Ok(())
    }

    /// Load settings from a YAML file. Returns defaults when the file does not
    /// exist; returns an error only when the file exists but cannot be parsed
    /// or fails validation.
    pub fn load(path: impl AsRef<Path>) -> Result<Settings, String> {
        let path = path.as_ref();
        if !path.exists() {
            let mut s = Settings::default();
            s.ensure_defaults();
            s.apply_env();
            s.validate()?;
            return Ok(s);
        }
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        let mut settings: Settings = serde_yaml::from_str(&raw)
            .map_err(|e| format!("failed to parse {}: {e}", path.display()))?;
        settings.ensure_defaults();
        settings.apply_env();
        settings.validate()?;
        Ok(settings)
    }

    /// Infallible convenience for library/CLI callers: like [`Settings::load`]
    /// but never returns an error — a missing or unparseable file falls back to
    /// the built-in defaults (with env overlays still applied). Handy when you
    /// want the engine to "just run" without surfacing config errors.
    pub fn load_or_default(path: impl AsRef<Path>) -> Settings {
        Settings::load(path).unwrap_or_else(|_| Settings::default())
    }

    /// Persist the current settings to a YAML file, creating parent
    /// directories as needed. Validates before writing so a bad in-memory edit
    /// never clobbers a good file. This is how the standalone server's
    /// `/preferences` editor writes engine enable/weight + search defaults back
    /// to disk; the same file is read on the next [`Settings::load`].
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), String> {
        self.validate()?;
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
            }
        }
        let yaml = serde_yaml::to_string(self).map_err(|e| format!("failed to serialize: {e}"))?;
        std::fs::write(path, yaml).map_err(|e| format!("failed to write {}: {e}", path.display()))
    }

    /// Engines enabled for the given categories (or all enabled engines when
    /// `categories` is empty). When `only` is given, restrict to those engine
    /// names (used by `!bang` shortcuts).
    pub fn selected_engines(
        &self,
        categories: &[String],
        only: Option<&[String]>,
    ) -> Vec<&EngineSettings> {
        self.engines
            .iter()
            .filter(|e| e.enabled)
            .filter(|e| match only {
                Some(names) => names.iter().any(|n| n == &e.name),
                None => {
                    categories.is_empty()
                        || e.categories
                            .iter()
                            .any(|c| categories.iter().any(|q| q == c))
                }
            })
            .collect()
    }

    /// Custom (config-driven) engines enabled for the given categories, or
    /// exactly the `only` set when given (mirrors [`Settings::selected_engines`]).
    pub fn selected_custom_engines(
        &self,
        categories: &[String],
        only: Option<&[String]>,
    ) -> Vec<&CustomEngine> {
        self.custom_engines
            .iter()
            .filter(|e| e.enabled)
            .filter(|e| match only {
                Some(names) => names.iter().any(|n| n == &e.name),
                None => {
                    categories.is_empty()
                        || e.categories
                            .iter()
                            .any(|c| categories.iter().any(|q| q == c))
                }
            })
            .collect()
    }

    /// Weight lookup map (native + custom engines).
    pub fn weights(&self) -> std::collections::HashMap<String, f64> {
        self.engines
            .iter()
            .map(|e| (e.name.clone(), e.weight))
            .chain(
                self.custom_engines
                    .iter()
                    .map(|e| (e.name.clone(), e.weight)),
            )
            .collect()
    }

    /// All categories declared by enabled engines (native + custom), ordered
    /// for standard UI tabs.
    pub fn categories(&self) -> Vec<String> {
        const TAB_ORDER: &[&str] = &[
            "general", "images", "videos", "news", "science", "it", "map", "music", "social",
            "files",
        ];
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        let mut collect = |c: &str| {
            if seen.insert(c.to_string()) {
                out.push(c.to_string());
            }
        };
        for cat in TAB_ORDER {
            collect(cat);
        }
        let native = self
            .engines
            .iter()
            .filter(|e| e.enabled)
            .flat_map(|e| &e.categories);
        let custom = self
            .custom_engines
            .iter()
            .filter(|e| e.enabled)
            .flat_map(|e| &e.categories);
        for c in native.chain(custom) {
            collect(c);
        }
        out
    }
}

impl CustomEngine {
    /// Validate one config-driven adapter: known `type`, sane weight, and the
    /// type-specific fields needed to actually run it (with good messages).
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("custom engine has an empty name".into());
        }
        if crate::engines::is_known_engine(&self.name) {
            return Err(format!(
                "custom engine '{}' shadows a native/family engine name; choose another",
                self.name
            ));
        }
        if !CUSTOM_ENGINE_KINDS.contains(&self.kind.as_str()) {
            return Err(format!(
                "custom engine '{}' has unknown type '{}' (known: {})",
                self.name,
                self.kind,
                CUSTOM_ENGINE_KINDS.join(", ")
            ));
        }
        if !(self.weight.is_finite() && self.weight >= 0.0) {
            return Err(format!("custom engine '{}' weight must be >= 0", self.name));
        }
        let has_query = |t: &str| t.contains("{query}") || t.contains("{query_raw}");
        match self.kind.as_str() {
            "rss" => {
                let t = self.url_template.as_deref().unwrap_or("");
                if t.is_empty() {
                    return Err(format!(
                        "custom engine '{}' (rss) needs a url_template",
                        self.name
                    ));
                }
                if !has_query(t) {
                    return Err(format!(
                        "custom engine '{}' (rss) url_template must contain a {{query}} placeholder",
                        self.name
                    ));
                }
            }
            "json" => {
                let t = self.url_template.as_deref().unwrap_or("");
                if t.is_empty() {
                    return Err(format!(
                        "custom engine '{}' (json) needs a url_template",
                        self.name
                    ));
                }
                if !has_query(t) {
                    return Err(format!(
                        "custom engine '{}' (json) url_template must contain a {{query}} placeholder",
                        self.name
                    ));
                }
                if self.url_field.as_deref().unwrap_or("").is_empty() {
                    return Err(format!(
                        "custom engine '{}' (json) needs a url_field",
                        self.name
                    ));
                }
                if self.title_field.as_deref().unwrap_or("").is_empty() {
                    return Err(format!(
                        "custom engine '{}' (json) needs a title_field",
                        self.name
                    ));
                }
            }
            "opensearch" => {
                let has_template = self.url_template.as_deref().map(has_query).unwrap_or(false);
                let has_desc = !self.description_url.as_deref().unwrap_or("").is_empty();
                if !has_template && !has_desc {
                    return Err(format!(
                        "custom engine '{}' (opensearch) needs either a description_url or a url_template containing {{query}}",
                        self.name
                    ));
                }
            }
            _ => unreachable!("kind validated above"),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        Settings::default().validate().unwrap();
    }

    #[test]
    fn rejects_unknown_engine() {
        let mut s = Settings::default();
        s.engines.push(EngineSettings {
            name: "not_a_real_engine".into(),
            enabled: true,
            weight: 1.0,
            categories: vec!["general".into()],
            base_url: None,
            timeout_secs: None,
            api_key: None,
            extra: None,
        });
        assert!(s.validate().is_err());
    }

    #[test]
    fn rejects_negative_weight() {
        let mut s = Settings::default();
        s.engines[0].weight = -1.0;
        assert!(s.validate().is_err());
    }

    #[test]
    fn bang_only_overrides_category_selection() {
        let s = Settings::default();
        let only = vec!["wikipedia".to_string()];
        let sel = s.selected_engines(&[], Some(&only));
        assert_eq!(sel.len(), 1);
        assert_eq!(sel[0].name, "wikipedia");
    }

    #[test]
    fn save_then_load_round_trips_edits() {
        let mut s = Settings::default();
        // Mutate an engine + a search default the way the /preferences editor does.
        if let Some(e) = s.engines.iter_mut().find(|e| e.name == "brave") {
            e.enabled = false;
            e.weight = 2.5;
        }
        s.search.default_lang = "de".into();
        s.search.safe_search = 1;
        s.server.max_results_per_engine = 15;

        let dir = std::env::temp_dir().join(format!("ms-cfg-{}", uuid_like()));
        let path = dir.join("settings.yml");
        s.save(&path).unwrap();

        let loaded = Settings::load(&path).unwrap();
        let brave = loaded.engines.iter().find(|e| e.name == "brave").unwrap();
        assert!(!brave.enabled);
        assert_eq!(brave.weight, 2.5);
        assert_eq!(loaded.search.default_lang, "de");
        assert_eq!(loaded.search.safe_search, 1);
        assert_eq!(loaded.server.max_results_per_engine, 15);

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn custom(name: &str, kind: &str) -> CustomEngine {
        CustomEngine {
            name: name.into(),
            kind: kind.into(),
            enabled: true,
            weight: 1.0,
            categories: vec!["general".into()],
            url_template: None,
            description_url: None,
            result_path: None,
            url_field: None,
            title_field: None,
            content_field: None,
            thumbnail_field: None,
            published_field: None,
            timeout_secs: None,
            api_key: None,
        }
    }

    #[test]
    fn accepts_valid_custom_rss_engine() {
        let mut s = Settings::default();
        let mut c = custom("my_rss", "rss");
        c.url_template = Some("https://example.com/feed?q={query}".into());
        s.custom_engines.push(c);
        s.validate().unwrap();
        // Participates in weights + categories.
        assert!(s.weights().contains_key("my_rss"));
    }

    #[test]
    fn rejects_rss_without_query_placeholder() {
        let mut s = Settings::default();
        let mut c = custom("bad_rss", "rss");
        c.url_template = Some("https://example.com/feed".into());
        s.custom_engines.push(c);
        assert!(s.validate().is_err());
    }

    #[test]
    fn rejects_unknown_custom_type() {
        let mut s = Settings::default();
        s.custom_engines.push(custom("weird", "graphql"));
        assert!(s.validate().is_err());
    }

    #[test]
    fn rejects_json_without_mappings() {
        let mut s = Settings::default();
        let mut c = custom("bad_json", "json");
        c.url_template = Some("https://api.example.com/?q={query}".into());
        // url_field / title_field missing.
        s.custom_engines.push(c);
        assert!(s.validate().is_err());
    }

    #[test]
    fn accepts_valid_custom_json_engine() {
        let mut s = Settings::default();
        let mut c = custom("my_json", "json");
        c.url_template = Some("https://api.example.com/?q={query}".into());
        c.result_path = Some("results".into());
        c.url_field = Some("url".into());
        c.title_field = Some("title".into());
        s.custom_engines.push(c);
        s.validate().unwrap();
    }

    #[test]
    fn rejects_opensearch_without_template_or_desc() {
        let mut s = Settings::default();
        s.custom_engines.push(custom("bad_os", "opensearch"));
        assert!(s.validate().is_err());
    }

    #[test]
    fn accepts_opensearch_with_description_url() {
        let mut s = Settings::default();
        let mut c = custom("my_os", "opensearch");
        c.description_url = Some("https://example.com/opensearch.xml".into());
        s.custom_engines.push(c);
        s.validate().unwrap();
    }

    #[test]
    fn rejects_custom_name_colliding_with_native() {
        let mut s = Settings::default();
        let mut c = custom("wikipedia", "rss");
        c.url_template = Some("https://example.com/feed?q={query}".into());
        s.custom_engines.push(c);
        assert!(s.validate().is_err());
    }

    #[test]
    fn rejects_duplicate_custom_names() {
        let mut s = Settings::default();
        let mk = || {
            let mut c = custom("dupe", "rss");
            c.url_template = Some("https://example.com/feed?q={query}".into());
            c
        };
        s.custom_engines.push(mk());
        s.custom_engines.push(mk());
        assert!(s.validate().is_err());
    }

    #[test]
    fn accepts_family_engines() {
        let mut s = Settings::default();
        // Extra Stack Exchange site (no base_url needed; site is in the name).
        s.engines.push(EngineSettings {
            name: "stackexchange_superuser".into(),
            enabled: true,
            weight: 1.0,
            categories: vec!["it".into()],
            base_url: None,
            timeout_secs: None,
            api_key: None,
            extra: None,
        });
        // Custom MediaWiki wiki (needs base_url = api.php URL).
        s.engines.push(EngineSettings {
            name: "mediawiki_archwiki".into(),
            enabled: true,
            weight: 1.0,
            categories: vec!["it".into()],
            base_url: Some("https://wiki.archlinux.org/api.php".into()),
            timeout_secs: None,
            api_key: None,
            extra: Some("https://wiki.archlinux.org/title/".into()),
        });
        // Extra Lemmy instance (needs base_url).
        s.engines.push(EngineSettings {
            name: "lemmy_lemmyml".into(),
            enabled: true,
            weight: 1.0,
            categories: vec!["social".into()],
            base_url: Some("https://lemmy.ml".into()),
            timeout_secs: None,
            api_key: None,
            extra: None,
        });
        s.validate().unwrap();
    }

    #[test]
    fn rejects_enabled_mediawiki_instance_without_base_url() {
        let mut s = Settings::default();
        s.engines.push(EngineSettings {
            name: "mediawiki_archwiki".into(),
            enabled: true,
            weight: 1.0,
            categories: vec!["it".into()],
            base_url: None,
            timeout_secs: None,
            api_key: None,
            extra: None,
        });
        assert!(s.validate().is_err());
    }

    /// Tiny unique-ish suffix for the temp path (avoids a dev-dependency).
    fn uuid_like() -> u128 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }
}
