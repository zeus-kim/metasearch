//! Pluggable upstream engine adapters.
//!
//! Each engine lives in its own module and exposes an `async` search function
//! returning results in relevance order. New engines are added by writing a
//! module and registering it in [`run`] / [`ENGINE_NAMES`] — mirroring
//! the standard `engines/` directory. No engine here requires an API key.

use std::time::Duration;

use serde_json::Value;

use crate::config::CustomEngine;
use crate::types::{Answer, EngineResult, Infobox};

pub mod archive_music;
pub mod arxiv;
pub mod askubuntu;
pub mod bing;
pub mod bing_images;
pub mod bingnews;
pub mod brave;
pub mod brave_api;
pub mod brave_images;
pub mod codeberg;
pub mod crates_io;
pub mod crossref;
pub mod custom;
pub mod doaj;
pub mod dockerhub;
pub mod duckduckgo;
pub mod duckduckgo_images;
pub mod duckduckgo_videos;
pub mod europepmc;
pub mod gdelt;
pub mod github;
pub mod gitlab;
pub mod google;
pub mod googlenews;
pub mod hackernews;
pub mod internetarchive;
pub mod lemmy;
pub mod local_feeds;
pub mod naver_scrape;
pub mod daum_scrape;
pub mod google_scrape;
pub mod bing_scrape;
pub mod marginalia;
pub mod mojeek;
pub mod npm;
pub mod openalex;
pub mod openlibrary;
pub mod openstreetmap;
pub mod openverse;
pub mod packagist;
pub mod peertube;
pub mod qwant;
pub mod reddit;
pub mod retry;
pub mod rubygems;
pub mod semanticscholar;
pub mod stackexchange;
pub mod startpage;
pub mod wikibooks;
pub mod wikicommons;
pub mod wikidata;
pub mod wikinews;
pub mod wikipedia;
pub mod wikiquote;
pub mod wikisource;
pub mod wiktionary;
pub mod yandex;

/// Everything an engine needs to perform one search.
pub struct EngineContext<'a> {
    pub client: &'a reqwest::Client,
    pub query: &'a str,
    /// Resolved 2-letter language (or region tag), e.g. `en`, `de`, `en-us`.
    pub lang: &'a str,
    /// 0 = off, 1 = moderate, 2 = strict.
    pub safe_search: u8,
    pub timeout: Duration,
    pub max_results: usize,
    /// 1-indexed page number.
    pub pageno: usize,
    /// `day` | `week` | `month` | `year`, if the request constrained recency.
    pub time_range: Option<&'a str>,
    /// Base URL for proxy-style engines.
    pub base_url: Option<&'a str>,
    /// API key for key-based engines (Google, Bing). `None` for keyless engines.
    pub api_key: Option<&'a str>,
    /// Secondary credential/param for key-based engines (e.g. Google `cx`).
    pub extra: Option<&'a str>,
    /// When set, this engine is a config-driven generic adapter (RSS/Atom,
    /// OpenSearch, JSON template); [`run`] dispatches to [`custom::search`] with
    /// this spec instead of a named native engine.
    pub custom: Option<&'a CustomEngine>,
    /// When false (default), custom-engine fetch URLs must pass SSRF checks.
    pub allow_private_urls: bool,
    /// Category filter for feeds/discover (e.g., "sports", "tech")
    pub category: Option<&'a str>,
    /// Country filter (ISO 3166-1 alpha-2, e.g., "US", "KR")
    pub country: Option<&'a str>,
}

impl EngineContext<'_> {
    /// 0-based result offset implied by the current page.
    pub fn offset(&self) -> usize {
        self.pageno.saturating_sub(1) * self.max_results
    }

    /// Just the language part of `lang` (drops any region suffix).
    pub fn lang_code(&self) -> &str {
        let base = self.lang.split('-').next().unwrap_or(self.lang);
        if base.is_empty() {
            "en"
        } else {
            base
        }
    }
}

/// Every engine name this crate knows how to dispatch. Used for config
/// validation and the `/config` surface.
pub const ENGINE_NAMES: &[&str] = &[
    // Local feeds (standalone mode)
    "local_feeds",
    "local_news",
    // External engines
    "wikipedia",
    "wikidata",
    "duckduckgo",
    "duckduckgo_lite",
    "brave",
    "naver",
    "daum",
    "google_web",
    "bing_web",
    "mojeek",
    "github",
    "stackexchange",
    "arxiv",
    "hackernews",
    "wikicommons",
    "duckduckgo_images",
    "duckduckgo_videos",
    "bing_images",
    "brave_images",
    "openverse",
    "archive_music",
    // Code / dev
    "gitlab",
    "codeberg",
    "crates_io",
    "npm",
    "packagist",
    "rubygems",
    "dockerhub",
    "askubuntu",
    // Reference
    "wiktionary",
    "wikibooks",
    "wikiquote",
    "wikisource",
    "openlibrary",
    "internetarchive",
    // Science / academic
    "openalex",
    "crossref",
    "europepmc",
    "semanticscholar",
    "doaj",
    // News
    "googlenews",
    "gdelt",
    "wikinews",
    // Social / video / map
    "lemmy",
    "peertube",
    "openstreetmap",
    // Opt-in keyless web
    "startpage",
    "qwant",
    // Opt-in key-based
    "google",
    "bing",
    "brave_api",
    "yandex",
    "bingnews",
    // Opt-in feature-gated (compiled in only with the matching cargo feature)
    "reddit",
    "marginalia",
];

/// Whether `name` is a parameterized Stack Exchange *site* instance, e.g.
/// `stackexchange_superuser` → site `superuser`. All dispatch to the shared
/// Stack Exchange adapter ([`stackexchange::search_site`]).
pub fn is_stackexchange_site(name: &str) -> bool {
    stackexchange_site(name).is_some()
}

/// The Stack Exchange site key for a `stackexchange_<site>` engine name.
pub fn stackexchange_site(name: &str) -> Option<&str> {
    name.strip_prefix("stackexchange_")
        .filter(|s| !s.is_empty())
}

/// Whether `name` is a labelled Lemmy instance, e.g. `lemmy_lemmyml`. These
/// dispatch to the Lemmy adapter using the per-engine `base_url`.
pub fn is_lemmy_instance(name: &str) -> bool {
    name.strip_prefix("lemmy_").is_some_and(|s| !s.is_empty())
}

/// Whether `name` is a labelled MediaWiki wiki, e.g. `mediawiki_archwiki`.
/// These dispatch to the generalized MediaWiki adapter using the per-engine
/// `base_url` (the wiki's `api.php` endpoint).
pub fn is_mediawiki_instance(name: &str) -> bool {
    name.strip_prefix("mediawiki_")
        .is_some_and(|s| !s.is_empty())
}

/// Whether `name` is one of the Wikimedia projects served from shared Wikimedia
/// infrastructure (Wikipedia, Wiktionary, Wikibooks, Wikiquote, Wikisource,
/// Wikinews, Wikidata, Wikimedia Commons). These sit behind the same edge and
/// enforce a *per-client-IP* concurrent-connection limit, so hammering several
/// of them at once (as the default fan-out does) is what triggers their
/// `429 Too Many Requests`. Note: arbitrary `mediawiki_<label>` instances point
/// at non-Wikimedia wikis (Arch, Gentoo, …) and are intentionally *not*
/// included here.
pub fn is_wikimedia_engine(name: &str) -> bool {
    matches!(
        name,
        "wikipedia"
            | "wikidata"
            | "wikibooks"
            | "wikiquote"
            | "wikisource"
            | "wiktionary"
            | "wikinews"
            | "wikicommons"
    )
}

/// The rate-limiter slot an engine should share. Most engines get their own
/// slot (their name), but engines that hit a *shared* upstream must share one
/// slot so the limiter serializes them instead of fanning out concurrently and
/// tripping that upstream's per-IP limits. All Wikimedia projects collapse to a
/// single `"wikimedia"` slot — this is the primary fix for the intermittent
/// `wikibooks`/`wikiquote` 429s under concurrent fan-out.
pub fn rate_limit_key(name: &str) -> &str {
    if is_wikimedia_engine(name) {
        "wikimedia"
    } else {
        name
    }
}

/// Whether `name` is a parameterized engine *family* instance (one adapter,
/// many config-declared instances), as opposed to a single named engine.
pub fn is_family_engine(name: &str) -> bool {
    is_stackexchange_site(name)
        || is_lemmy_instance(name)
        || is_mediawiki_instance(name)
}

/// Whether `name` is a dispatchable engine (a named native engine or a
/// parameterized family instance).
pub fn is_known_engine(name: &str) -> bool {
    ENGINE_NAMES.contains(&name) || is_family_engine(name)
}

/// General web **scrapers** whose HTTP-200 + zero-result responses usually mean
/// selector drift or a silent bot-block (not a legitimately empty topic).
/// Reference/API engines (Wikipedia, GitHub, arXiv, …) are intentionally omitted
/// — they can return empty sets on obscure queries without being "broken".
pub fn empty_result_sensitive(name: &str) -> bool {
    matches!(
        name,
        "duckduckgo"
            | "duckduckgo_lite"
            | "duckduckgo_images"
            | "duckduckgo_videos"
            | "bing_images"
            | "brave_images"
            | "brave"
            | "mojeek"
            | "startpage"
            | "qwant"
            | "marginalia"
            | "reddit"
            | "bing"
            | "google"
            | "brave_api"
            | "yandex"
            | "bingnews"
    )
}

/// What a single engine contributes to one search. Most engines only fill
/// `results`; richer engines may also surface infoboxes, suggestions or
/// query corrections (just standard engines do).
#[derive(Debug, Default, Clone)]
pub struct EngineResponse {
    pub results: Vec<EngineResult>,
    pub infoboxes: Vec<Infobox>,
    pub suggestions: Vec<String>,
    pub corrections: Vec<String>,
    /// Instant answers contributed directly by an engine. Most engines leave this empty.
    pub answers: Vec<Answer>,
}

impl From<Vec<EngineResult>> for EngineResponse {
    fn from(results: Vec<EngineResult>) -> Self {
        EngineResponse {
            results,
            ..Default::default()
        }
    }
}

/// Dispatch to the named engine. Unknown names are a (recoverable) error so the
/// orchestrator can mark them unresponsive without failing the whole search.
pub async fn run(name: &str, ctx: &EngineContext<'_>) -> Result<EngineResponse, String> {
    // Config-driven generic adapters take priority: when the orchestrator marks
    // this engine custom, dispatch by its declared `type`, not by name.
    if let Some(spec) = ctx.custom {
        return custom::search(ctx, spec).await;
    }
    match name {
        "local_feeds" => local_feeds::search(ctx).await.map(Into::into),
        "local_news" => local_feeds::search_news(ctx).await.map(Into::into),
        "naver" => naver_scrape::search(ctx).await.map(Into::into),
        "daum" => daum_scrape::search(ctx).await.map(Into::into),
        "google_web" => google_scrape::search(ctx).await.map(Into::into),
        "bing_web" => bing_scrape::search(ctx).await.map(Into::into),
        "wikipedia" => wikipedia::search(ctx).await,
        "wikidata" => wikidata::search(ctx).await,
        "duckduckgo" => duckduckgo::search_instant(ctx).await,
        "duckduckgo_lite" => duckduckgo::search_lite(ctx).await.map(Into::into),
        "brave" => brave::search(ctx).await.map(Into::into),
        "mojeek" => mojeek::search(ctx).await.map(Into::into),
        "github" => github::search(ctx).await.map(Into::into),
        "stackexchange" => stackexchange::search(ctx).await.map(Into::into),
        "arxiv" => arxiv::search(ctx).await.map(Into::into),
        "hackernews" => hackernews::search(ctx).await.map(Into::into),
        "wikicommons" => wikicommons::search(ctx).await.map(Into::into),
        "duckduckgo_images" => duckduckgo_images::search(ctx).await.map(Into::into),
        "duckduckgo_videos" => duckduckgo_videos::search(ctx).await.map(Into::into),
        "bing_images" => bing_images::search(ctx).await.map(Into::into),
        "brave_images" => brave_images::search(ctx).await.map(Into::into),
        "openverse" => openverse::search(ctx).await.map(Into::into),
        "archive_music" => archive_music::search(ctx).await.map(Into::into),
        "gitlab" => gitlab::search(ctx).await.map(Into::into),
        "codeberg" => codeberg::search(ctx).await.map(Into::into),
        "crates_io" => crates_io::search(ctx).await.map(Into::into),
        "npm" => npm::search(ctx).await.map(Into::into),
        "packagist" => packagist::search(ctx).await.map(Into::into),
        "rubygems" => rubygems::search(ctx).await.map(Into::into),
        "dockerhub" => dockerhub::search(ctx).await.map(Into::into),
        "askubuntu" => askubuntu::search(ctx).await.map(Into::into),
        "wiktionary" => wiktionary::search(ctx).await.map(Into::into),
        "wikibooks" => wikibooks::search(ctx).await.map(Into::into),
        "wikiquote" => wikiquote::search(ctx).await.map(Into::into),
        "wikisource" => wikisource::search(ctx).await.map(Into::into),
        "openlibrary" => openlibrary::search(ctx).await.map(Into::into),
        "internetarchive" => internetarchive::search(ctx).await.map(Into::into),
        "openalex" => openalex::search(ctx).await.map(Into::into),
        "crossref" => crossref::search(ctx).await.map(Into::into),
        "europepmc" => europepmc::search(ctx).await.map(Into::into),
        "semanticscholar" => semanticscholar::search(ctx).await.map(Into::into),
        "doaj" => doaj::search(ctx).await.map(Into::into),
        "googlenews" => googlenews::search(ctx).await.map(Into::into),
        "gdelt" => gdelt::search(ctx).await.map(Into::into),
        "wikinews" => wikinews::search(ctx).await.map(Into::into),
        "lemmy" => lemmy::search(ctx).await.map(Into::into),
        "peertube" => peertube::search(ctx).await.map(Into::into),
        "openstreetmap" => openstreetmap::search(ctx).await.map(Into::into),
        "startpage" => startpage::search(ctx).await.map(Into::into),
        "qwant" => qwant::search(ctx).await.map(Into::into),
        "google" => google::search(ctx).await.map(Into::into),
        "bing" => bing::search(ctx).await.map(Into::into),
        "brave_api" => brave_api::search(ctx).await.map(Into::into),
        "yandex" => yandex::search(ctx).await.map(Into::into),
        "bingnews" => bingnews::search(ctx).await.map(Into::into),
        "reddit" => reddit::search(ctx).await.map(Into::into),
        "marginalia" => marginalia::search(ctx).await.map(Into::into),
        // Parameterized Stack Exchange site family: site key is in the name.
        name if is_stackexchange_site(name) => {
            let site = stackexchange_site(name).unwrap_or("stackoverflow");
            stackexchange::search_site(ctx, site).await.map(Into::into)
        }
        // Labelled Lemmy instance family (uses per-engine base_url).
        name if is_lemmy_instance(name) => lemmy::search(ctx).await.map(Into::into),
        // Labelled MediaWiki wiki family (base_url = the wiki's api.php). An
        // optional `extra` overrides the article URL base (default
        // `{scheme}://{host}/wiki/`), since some wikis use a different path.
        name if is_mediawiki_instance(name) => {
            let api = ctx
                .base_url
                .ok_or("mediawiki instance requires `base_url` (the wiki's api.php URL)")?;
            let article_base = mediawiki_article_base(api, ctx.extra);
            mediawiki_search_api(ctx, api, &article_base, "general")
                .await
                .map(Into::into)
        }
        other => Err(format!("unknown engine: {other}")),
    }
}

/// Fetch autocomplete suggestions for `query` from the named backend.
pub async fn autocomplete(
    backend: &str,
    client: &reqwest::Client,
    query: &str,
    lang: &str,
    timeout: Duration,
) -> Vec<String> {
    match backend {
        "duckduckgo" => duckduckgo::autocomplete(client, query, timeout).await,
        "wikipedia" => wikipedia::autocomplete(client, query, lang, timeout).await,
        _ => Vec::new(),
    }
}

/// Shared MediaWiki `list=search` engine used by the Wikimedia sister projects
/// (Wiktionary, Wikibooks, …). `site` is the bare domain suffix (e.g.
/// `wiktionary.org`); the language code is prepended to form the host.
pub(crate) async fn mediawiki_search(
    ctx: &EngineContext<'_>,
    site: &str,
    category: &str,
) -> Result<Vec<EngineResult>, String> {
    let lang = ctx.lang_code();
    let host = format!("{lang}.{site}");
    let url = format!("https://{host}/w/api.php");
    let offset = ctx.offset().to_string();
    let limit = ctx.max_results.to_string();

    let resp = ctx
        .client
        .get(&url)
        .header("User-Agent", WIKIMEDIA_USER_AGENT)
        .query(&[
            ("action", "query"),
            ("list", "search"),
            ("srsearch", ctx.query),
            ("srlimit", &limit),
            ("sroffset", &offset),
            ("format", "json"),
        ])
        .timeout(ctx.timeout)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let body: Value = resp.json().await.map_err(|e| body_error(&e))?;
    Ok(parse_mediawiki(&body, &host, category))
}

/// Parse a MediaWiki `list=search` response for the given `host`. Pure for
/// fixture testing.
pub(crate) fn parse_mediawiki(body: &Value, host: &str, category: &str) -> Vec<EngineResult> {
    parse_mediawiki_base(body, &format!("https://{host}/wiki/"), category)
}

/// Generalized MediaWiki `list=search` engine for *any* wiki, identified by its
/// full `api.php` URL (e.g. `https://wiki.archlinux.org/api.php`). Powers the
/// `mediawiki_<label>` family so users can declare arbitrary wikis (Arch,
/// Gentoo, Wikivoyage, fandom, …) in config without new Rust code.
pub(crate) async fn mediawiki_search_api(
    ctx: &EngineContext<'_>,
    api_url: &str,
    article_base: &str,
    category: &str,
) -> Result<Vec<EngineResult>, String> {
    let offset = ctx.offset().to_string();
    let limit = ctx.max_results.to_string();

    let resp = ctx
        .client
        .get(api_url)
        .header("User-Agent", WIKIMEDIA_USER_AGENT)
        .query(&[
            ("action", "query"),
            ("list", "search"),
            ("srsearch", ctx.query),
            ("srlimit", &limit),
            ("sroffset", &offset),
            ("format", "json"),
        ])
        .timeout(ctx.timeout)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let body: Value = resp.json().await.map_err(|e| body_error(&e))?;
    Ok(parse_mediawiki_base(&body, article_base, category))
}

/// Derive the article URL base (`{scheme}://{host}/wiki/`) from a wiki's
/// `api.php` URL, unless an explicit override is supplied (some wikis use a
/// different article path, e.g. Arch's `/title/`).
pub(crate) fn mediawiki_article_base(api_url: &str, article_override: Option<&str>) -> String {
    if let Some(base) = article_override.filter(|s| !s.is_empty()) {
        let mut base = base.to_string();
        if !base.ends_with('/') {
            base.push('/');
        }
        return base;
    }
    if let Ok(u) = url::Url::parse(api_url) {
        if let Some(host) = u.host_str() {
            let port = u.port().map(|p| format!(":{p}")).unwrap_or_default();
            return format!("{}://{host}{port}/wiki/", u.scheme());
        }
    }
    // Fallback: strip a trailing api.php path and append /wiki/.
    let trimmed = api_url
        .trim_end_matches("/w/api.php")
        .trim_end_matches("/api.php")
        .trim_end_matches('/');
    format!("{trimmed}/wiki/")
}

/// Parse a MediaWiki `list=search` response, building result URLs from an
/// explicit `article_base` (e.g. `https://wiki.archlinux.org/title/`). Pure.
pub(crate) fn parse_mediawiki_base(
    body: &Value,
    article_base: &str,
    category: &str,
) -> Vec<EngineResult> {
    let hits = match body["query"]["search"].as_array() {
        Some(h) => h,
        None => return Vec::new(),
    };
    let mut results = Vec::new();
    for hit in hits {
        let title = hit["title"].as_str().unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        let snippet = strip_html(hit["snippet"].as_str().unwrap_or_default());
        let page = title.replace(' ', "_");
        let url = format!("{article_base}{page}");
        let mut r = EngineResult::new(url, title, snippet);
        r.template = Some("default.html".into());
        r.category = Some(category.to_string());
        results.push(r);
    }
    results
}

/// A desktop-browser-like User-Agent. Some upstreams reject the default
/// `reqwest` agent; common practice.
pub(crate) const USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0";

/// A descriptive, policy-compliant User-Agent for the Wikimedia APIs.
///
/// The Wikimedia User-Agent policy
/// (<https://meta.wikimedia.org/wiki/User-Agent_policy>) requires automated
/// clients to send an *informative* agent identifying the tool and a way to
/// reach the operator, and warns that generic browser strings (like
/// [`USER_AGENT`]) may be throttled or blocked. Sending this — together with
/// serializing Wikimedia requests through one rate-limit slot
/// (see [`rate_limit_key`]) — is what keeps Wikipedia/Wikibooks/Wikiquote/…
/// from intermittently returning `429 Too Many Requests`.
pub(crate) const WIKIMEDIA_USER_AGENT: &str = concat!(
    "metasearch/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/ai-studio/metasearch; privacy-respecting metasearch) reqwest"
);

/// Classify an error raised while reading or decoding a response body so the
/// surfaced label is accurate (and correctly retry-classifiable).
///
/// `reqwest`'s `Response::json()` collapses two very different failures into the
/// same opaque message — `"error decoding response body"` — whether the body
/// read *timed out* mid-stream or whether bytes arrived fine but failed to
/// parse as JSON. The old `format!("bad json: {e}")` therefore mislabelled a
/// timeout (e.g. Codeberg under fan-out) as a JSON-parse error, which also hid
/// it from [`retry::is_retryable`]. We branch on
/// [`reqwest::Error::is_timeout`] / [`reqwest::Error::is_decode`] so a timeout
/// reads as a timeout (and is retried) and only genuine parse failures are
/// reported as `"bad json"`.
pub(crate) fn body_error(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        format!("timeout reading response body: {e}")
    } else if e.is_connect() || e.is_request() {
        format!("connection error reading body: {e}")
    } else if e.is_decode() {
        format!("parse error (bad json): {e}")
    } else {
        format!("transport error reading body: {e}")
    }
}

/// Classify an error from *sending* a request (`client.get(..).send()`), so a
/// connect/send timeout reads as a timeout rather than a generic transport
/// failure. Like [`body_error`] this branches on the `reqwest` error kind
/// because the timeout `Display` text does not reliably contain the word
/// "timeout". Used by the config-driven adapters; native engines that emit a
/// plain `"request failed: {e}"` are still correctly treated as a hard
/// transport failure by the health tracker.
pub(crate) fn request_error(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        format!("request timeout: {e}")
    } else if e.is_connect() {
        format!("connection error: {e}")
    } else {
        format!("request failed: {e}")
    }
}

/// Classify an error from reading a *raw* response body (`resp.text()` /
/// `resp.bytes()`), used by the HTML/XML scrapers and any engine that parses
/// the body itself. Like [`body_error`] this branches on the `reqwest` error
/// kind so a mid-stream timeout reads as a *timeout* (and is both retried and
/// counted as a hard failure by the engine-health tracker) rather than being
/// masked as a generic "bad body". There is no JSON-decode case here because
/// the caller decodes the text/bytes separately.
pub(crate) fn body_read_error(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        format!("timeout reading response body: {e}")
    } else if e.is_connect() || e.is_request() {
        format!("connection error reading body: {e}")
    } else {
        format!("transport error reading body: {e}")
    }
}

/// Map a resolved locale (`ko`, `ko-KR`, `de`, …) to a DuckDuckGo `kl` region
/// token. DuckDuckGo's region codes are `<region>-<lang>` (e.g. `kr-kr`,
/// `jp-jp`, `us-en`); `wt-wt` means "no region" (worldwide), which we return for
/// `all`/unknown locales so an undetected query keeps the previous worldwide
/// behaviour rather than guessing wrong.
pub(crate) fn ddg_region(lang: &str) -> String {
    let mut parts = lang.split('-');
    let l = parts.next().unwrap_or("").to_ascii_lowercase();
    let region = parts.next().map(|r| r.to_ascii_lowercase());
    match l.as_str() {
        "" | "all" | "any" | "global" => "wt-wt".to_string(),
        // English carries a region (default to US when none was supplied).
        "en" => format!("{}-en", region.as_deref().unwrap_or("us")),
        "ko" => "kr-kr".to_string(),
        "ja" => "jp-jp".to_string(),
        "zh" => "cn-zh".to_string(),
        "de" => "de-de".to_string(),
        "fr" => "fr-fr".to_string(),
        "es" => "es-es".to_string(),
        "it" => "it-it".to_string(),
        "ru" => "ru-ru".to_string(),
        "pt" => "br-pt".to_string(),
        "nl" => "nl-nl".to_string(),
        "pl" => "pl-pl".to_string(),
        "tr" => "tr-tr".to_string(),
        "ar" => "xa-ar".to_string(),
        "el" => "gr-el".to_string(),
        "he" => "il-he".to_string(),
        "th" => "th-th".to_string(),
        "hi" => "in-en".to_string(),
        // Unknown language: stay worldwide rather than emit an invalid region.
        _ => "wt-wt".to_string(),
    }
}

/// Map our 0/1/2 safe-search level to a DuckDuckGo `kp` value.
pub(crate) fn ddg_safe(level: u8) -> &'static str {
    match level {
        2 => "1",
        1 => "-1",
        _ => "-2",
    }
}

/// Map our 0/1/2 safe-search level to DuckDuckGo Images' `p` parameter.
pub(crate) fn ddg_images_safe(level: u8) -> &'static str {
    match level {
        2 => "1",
        1 => "0",
        _ => "-1",
    }
}

/// Extract a DuckDuckGo `vqd` bot-protection token from an HTML page.
pub(crate) fn extract_vqd(html: &str) -> Option<String> {
    for (prefix, suffix) in [("vqd=\"", "\""), ("vqd='", "'")] {
        if let Some(start) = html.find(prefix) {
            let rest = &html[start + prefix.len()..];
            if let Some(end) = rest.find(suffix) {
                let token = rest[..end].trim();
                if !token.is_empty() {
                    return Some(token.to_string());
                }
            }
        }
    }
    None
}

/// Publisher site icon (tiny; not used for news card `img_src`).
#[allow(dead_code)]
pub(crate) fn publisher_favicon_thumbnail(page_url: &str) -> Option<String> {
    let url = url::Url::parse(page_url.trim()).ok()?;
    let host = url.host_str()?;
    let domain = host.strip_prefix("www.").unwrap_or(host);
    if domain.is_empty() {
        return None;
    }
    Some(format!("https://icons.duckduckgo.com/ip3/{domain}.ico"))
}

/// True when `url` points at a direct image asset (common on HN/Lemmy links).
pub(crate) fn looks_like_image_url(url: &str) -> bool {
    let u = url.trim();
    if u.is_empty() {
        return false;
    }
    let lower = u.to_ascii_lowercase();
    lower.contains("i.redd.it/")
        || lower.contains("i.imgur.com/")
        || lower.ends_with(".jpg")
        || lower.ends_with(".jpeg")
        || lower.ends_with(".png")
        || lower.ends_with(".gif")
        || lower.ends_with(".webp")
        || lower.ends_with(".avif")
}

/// Pull the first http(s) image `src` from RSS/HTML description markup.
pub(crate) fn extract_img_src_from_html(html: &str) -> Option<String> {
    for marker in ["src=\"", "src='"] {
        if let Some(start) = html.find(marker) {
            let rest = &html[start + marker.len()..];
            let end_char = if marker.ends_with('"') { '"' } else { '\'' };
            if let Some(end) = rest.find(end_char) {
                let src = rest[..end].trim();
                if src.starts_with("http")
                    && looks_like_image_url(src)
                    && crate::thumbnail::is_usable_thumbnail_url(src)
                {
                    return Some(src.to_string());
                }
            }
        }
    }
    None
}

/// Strip HTML tags and decode a handful of common entities. Good enough for the
/// short snippets engines return; intentionally dependency-free.
pub(crate) fn strip_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;
    for ch in input.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    let mut result = out
        .replace("&quot;", "\"")
        .replace("&#039;", "'")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");
    // Remove Google News URL artifacts (malformed link text without proper tags)
    if let Some(idx) = result.find("a href=\"https://news.google") {
        result.truncate(idx);
    }
    result.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wikimedia_engines_share_one_rate_limit_slot() {
        // All Wikimedia projects collapse to the shared "wikimedia" slot so the
        // limiter serializes them instead of tripping the per-IP 429.
        for e in [
            "wikipedia",
            "wikidata",
            "wikibooks",
            "wikiquote",
            "wikisource",
            "wiktionary",
            "wikinews",
            "wikicommons",
        ] {
            assert!(is_wikimedia_engine(e), "{e} should be a Wikimedia engine");
            assert_eq!(rate_limit_key(e), "wikimedia", "{e} shares the slot");
        }
    }

    #[test]
    fn non_wikimedia_engines_keep_their_own_slot() {
        for e in ["brave", "duckduckgo", "github", "mojeek", "lemmy"] {
            assert!(!is_wikimedia_engine(e));
            assert_eq!(rate_limit_key(e), e);
        }
        // A non-Wikimedia `mediawiki_<label>` wiki (e.g. Arch) must NOT be
        // grouped with the Wikimedia cluster.
        assert!(!is_wikimedia_engine("mediawiki_archwiki"));
        assert_eq!(rate_limit_key("mediawiki_archwiki"), "mediawiki_archwiki");
    }

    #[test]
    fn publisher_favicon_uses_site_domain() {
        let icon = publisher_favicon_thumbnail("https://www.washingtonpost.com/foo").unwrap();
        assert!(icon.contains("washingtonpost.com"));
    }

    #[test]
    fn ddg_region_maps_locale_to_kl() {
        assert_eq!(ddg_region("ko-KR"), "kr-kr");
        assert_eq!(ddg_region("ko"), "kr-kr");
        assert_eq!(ddg_region("ja-JP"), "jp-jp");
        assert_eq!(ddg_region("en"), "us-en");
        assert_eq!(ddg_region("en-GB"), "gb-en");
        // `all` / unknown locales stay worldwide (the previous behaviour).
        assert_eq!(ddg_region("all"), "wt-wt");
        assert_eq!(ddg_region("xx"), "wt-wt");
    }

    #[test]
    fn looks_like_image_url_detects_common_hosts() {
        assert!(looks_like_image_url("https://i.redd.it/abc.jpg"));
        assert!(looks_like_image_url("https://example.com/x.png"));
        assert!(!looks_like_image_url("https://example.com/article"));
    }
}
