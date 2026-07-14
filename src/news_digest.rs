#![allow(dead_code)] // Some functions reserved for future features

//! Discover / News tab digest: news search + RSS teasers only (no Ollama on load).
//!
//! Selection & ranking ([`curate_news`]) is a pure, no-AI, no-network layer on
//! top of the base positional search score. It applies four pragmatic signals
//! so the feed shows a *diverse, fresh, de-duplicated* set rather than whatever
//! the engines happened to rank first:
//!
//! 1. **Quality filtering** — drop empty/junk titles and non-HTTP links.
//! 2. **Recency weighting** — parse each engine's publish timestamp (RFC822 /
//!    RFC3339 / GDELT compact) and decay stale items; undated items get a
//!    neutral mid-freshness so they're neither buried nor allowed to bury fresh
//!    reporting.
//! 3. **Topical relevance** — gently down-rank items that match none of the
//!    seed query terms (never a hard filter — generic pills like "world news
//!    international" rarely appear verbatim in headlines).
//! 4. **Near-duplicate clustering + source diversity** — collapse the same
//!    story reported by multiple outlets (title-token Jaccard) and cap how many
//!    items any one source contributes, de-aggregating redirect hosts (Google
//!    News) to the underlying publisher first.
//!
//! Items with a usable image get a small boost so the hero slot tends to have a
//! picture, without making an image a hard requirement. Everything degrades
//! gracefully: no timestamps, no engines, or no AI never breaks selection.

use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::article::fetch_og_image;
use crate::config::{NewsSettings, Settings};
use crate::search::{search_all, Runtime, SearchParams};
use crate::thumbnail::{is_large_thumbnail, thumbnail_quality, ThumbnailQuality};
use crate::types::SearchResult;
use crate::url_safety::is_safe_public_url;

const MAX_DIGEST: usize = 100;

/// Valid news section IDs per §2.3 contract.
/// Core is SSOT — 8 unified categories.
const VALID_SECTIONS: &[&str] = &[
    "top", "politics", "economy", "sports", "world",
    "entertainment", "tech", "culture", "health",
];

/// Map fallback category to §2.3 section ID.
/// Must return values from VALID_SECTIONS only.
fn map_to_section_id(category: &str) -> &'static str {
    match category {
        "sports" => "sports",
        "tech" => "tech",
        "ai" => "tech",           // ai → tech
        "health" => "health",
        "finance" => "economy",   // finance → economy
        "business" => "economy",  // business → economy
        "politics" => "politics",
        "climate" => "tech",      // climate → tech (science news under tech)
        "entertainment" => "entertainment",
        "art" => "culture",       // art → culture
        "culture" => "culture",
        "world" => "world",
        "economy" => "economy",
        _ => "top",
    }
}

/// Classify news article into category based on title keywords (fallback only).
fn classify_category(title: &str) -> &'static str {
    let t = title.to_lowercase();

    // Sports (check first - specific names)
    if t.contains("스포츠") || t.contains("축구") || t.contains("야구") || t.contains("농구")
        || t.contains("골프") || t.contains("테니스") || t.contains("올림픽") || t.contains("월드컵")
        || t.contains("배구") || t.contains("배드민턴") || t.contains("수영") || t.contains("육상")
        || t.contains("kbo") || t.contains("kbl") || t.contains("k리그") || t.contains("프로야구")
        || t.contains("이정후") || t.contains("손흥민") || t.contains("류현진") || t.contains("오타니")
        || t.contains("안세영") || t.contains("김민석") || t.contains("승부차기") || t.contains("시구")
        || t.contains("엔트리") || t.contains("era ") || t.contains("홈런") || t.contains("안타")
        || t.contains("sports") || t.contains("football") || t.contains("baseball") || t.contains("nba")
        || t.contains("スポーツ") || t.contains("体育") || t.contains("サッカー") || t.contains("野球")
    {
        return "sports";
    }
    // AI (check before tech)
    if t.contains("ai") || t.contains("인공지능") || t.contains("chatgpt") || t.contains("gpt")
        || t.contains("llm") || t.contains("머신러닝") || t.contains("딥러닝") || t.contains("젠슨 황")
        || t.contains("nvidia") || t.contains("엔비디아") || t.contains("openai") || t.contains("anthropic")
        || t.contains("人工知能") || t.contains("人工智能") || t.contains("künstliche intelligenz")
    {
        return "ai";
    }
    // Tech & Science
    if t.contains("기술") || t.contains("과학") || t.contains("테크") || t.contains("연구")
        || t.contains("개발") || t.contains("특허") || t.contains("반도체") || t.contains("칩")
        || t.contains("바이러스") || t.contains("백신") || t.contains("dna") || t.contains("유전자")
        || t.contains("시뮬레이션") || t.contains("빙상") || t.contains("해수면") || t.contains("동토층")
        || t.contains("tech") || t.contains("science") || t.contains("research")
        || t.contains("テクノロジー") || t.contains("科技") || t.contains("wissenschaft")
    {
        return "tech";
    }
    // Health
    if t.contains("건강") || t.contains("의료") || t.contains("병원") || t.contains("질병")
        || t.contains("암") || t.contains("심장") || t.contains("당뇨") || t.contains("혈압")
        || t.contains("정신건강") || t.contains("우울") || t.contains("수면") || t.contains("식단")
        || t.contains("헬스조선") || t.contains("메디") || t.contains("의사") || t.contains("환자")
        || t.contains("health") || t.contains("medical") || t.contains("doctor") || t.contains("cancer")
        || t.contains("健康") || t.contains("医療") || t.contains("醫療") || t.contains("病院")
    {
        return "health";
    }
    // Finance
    if t.contains("금융") || t.contains("주식") || t.contains("증시") || t.contains("코스피")
        || t.contains("코스닥") || t.contains("비트코인") || t.contains("암호화폐") || t.contains("투자")
        || t.contains("매수") || t.contains("매도") || t.contains("레버리지") || t.contains("펀드")
        || t.contains("부동산") || t.contains("월세") || t.contains("전세") || t.contains("아파트")
        || t.contains("금리") || t.contains("환율") || t.contains("통화스와프") || t.contains("달러")
        || t.contains("finance") || t.contains("stock") || t.contains("invest") || t.contains("bitcoin")
        || t.contains("金融") || t.contains("株式") || t.contains("市場") || t.contains("投資")
    {
        return "finance";
    }
    // Business
    if t.contains("기업") || t.contains("비즈니스") || t.contains("사업") || t.contains("경영")
        || t.contains("삼성") || t.contains("현대") || t.contains("lg") || t.contains("sk")
        || t.contains("네이버") || t.contains("카카오") || t.contains("쿠팡") || t.contains("배민")
        || t.contains("ceo") || t.contains("회장") || t.contains("대표") || t.contains("인수")
        || t.contains("business") || t.contains("company") || t.contains("corporate")
        || t.contains("ビジネス") || t.contains("商业") || t.contains("企業") || t.contains("会社")
    {
        return "business";
    }
    // Politics
    if t.contains("정치") || t.contains("국회") || t.contains("대통령") || t.contains("선거")
        || t.contains("여당") || t.contains("야당") || t.contains("총리") || t.contains("장관")
        || t.contains("국민의힘") || t.contains("민주당") || t.contains("전당대회") || t.contains("출마")
        || t.contains("제재") || t.contains("탄핵") || t.contains("의원") || t.contains("법안")
        || t.contains("politics") || t.contains("government") || t.contains("election")
        || t.contains("政治") || t.contains("選挙") || t.contains("大統領") || t.contains("总统")
    {
        return "politics";
    }
    // Climate
    if t.contains("기후") || t.contains("환경") || t.contains("온난화") || t.contains("탄소")
        || t.contains("해빙") || t.contains("북극") || t.contains("남극") || t.contains("지진")
        || t.contains("태풍") || t.contains("홍수") || t.contains("가뭄") || t.contains("산불")
        || t.contains("climate") || t.contains("environment") || t.contains("earthquake")
        || t.contains("気候") || t.contains("環境") || t.contains("气候") || t.contains("地震")
    {
        return "climate";
    }
    // Entertainment
    if t.contains("연예") || t.contains("엔터테인먼트") || t.contains("아이돌") || t.contains("드라마")
        || t.contains("영화") || t.contains("가수") || t.contains("배우") || t.contains("뮤직")
        || t.contains("bts") || t.contains("블랙핑크") || t.contains("뉴진스") || t.contains("에스파")
        || t.contains("넷플릭스") || t.contains("디즈니") || t.contains("웨이브") || t.contains("티빙")
        || t.contains("entertainment") || t.contains("celebrity") || t.contains("movie") || t.contains("drama")
        || t.contains("芸能") || t.contains("娱乐") || t.contains("映画") || t.contains("电影")
    {
        return "entertainment";
    }
    // Art & Culture
    if t.contains("문화") || t.contains("예술") || t.contains("전시") || t.contains("공연")
        || t.contains("박물관") || t.contains("미술관") || t.contains("갤러리") || t.contains("콘서트")
        || t.contains("오페라") || t.contains("발레") || t.contains("뮤지컬") || t.contains("클래식")
        || t.contains("culture") || t.contains("art") || t.contains("museum") || t.contains("exhibition")
        || t.contains("文化") || t.contains("芸術") || t.contains("艺术") || t.contains("美術館")
    {
        return "art";
    }
    // World / International (broader matching)
    if t.contains("속보") || t.contains("세계") || t.contains("국제") || t.contains("외교")
        || t.contains("미국") || t.contains("중국") || t.contains("일본") || t.contains("유럽")
        || t.contains("러시아") || t.contains("북한") || t.contains("이스라엘") || t.contains("이란")
        || t.contains("우크라이나") || t.contains("nato") || t.contains("un") || t.contains("유엔")
        || t.contains("시진핑") || t.contains("트럼프") || t.contains("바이든") || t.contains("푸틴")
        || t.contains("world") || t.contains("international") || t.contains("global")
        || t.contains("世界") || t.contains("国際") || t.contains("國際") || t.contains("外交")
    {
        return "world";
    }
    // Default to world for unclassified
    "world"
}

/// Fetch news using native search engines (RSS feeds, Google News, etc.)
/// Used when Core API is unavailable or returns empty results.
async fn fetch_news_fallback(
    query: &str,
    _limit: usize,
    language: Option<&str>,
    category: Option<&str>,
    country: Option<&str>,
    settings: &Settings,
    rt: &Runtime,
) -> Vec<SearchResult> {
    let params = SearchParams {
        query: query.to_string(),
        categories: vec!["news".to_string()],
        pageno: 1,
        language: language.map(|s| s.to_string()),
        time_range: Some("day".to_string()), // Recent news only
        safe_search: None,
        ai_answer: Some(false), // No AI synthesis for fallback
        context: None,
        rerank: Some(false),
        deep: Some(false),
        deep_subqueries: None,
        discover_category: category.map(|s| s.to_string()),
        country: country.map(|s| s.to_string()),
    };

    let response = search_all(&params, settings, rt).await;
    response.results
}


/// Check if text contains any Hangul characters (Korean script).
fn contains_hangul(text: &str) -> bool {
    text.chars().any(|c| ('\u{AC00}'..='\u{D7AF}').contains(&c) || ('\u{1100}'..='\u{11FF}').contains(&c))
}

/// Strong boost for results with images - pushes imageless articles to the back.
const IMAGE_BOOST: f64 = 3.0;
/// Penalty for articles without images - ensures they rank much lower.
const NO_IMAGE_PENALTY: f64 = 0.2;

/// Boost for results from our own index (orgos_news, local_feeds) so they
/// appear ahead of aggregator links (e.g. Google News).
const ORGOS_NEWS_BOOST: f64 = 1.5;
const LOCAL_FEEDS_BOOST: f64 = 1.4;

/// Freshness factor assigned to items that expose no parseable publish time
/// (e.g. Wikinews). The midpoint keeps undated items mid-pack: a genuinely
/// fresh dated story can edge ahead, but undated reporting isn't buried.
const UNDATED_FRESHNESS: f64 = 0.5;

/// Hosts that are link aggregators / redirectors: every item shares the same
/// host even though the underlying publisher differs, so per-source diversity
/// must look through them to the real outlet.
const AGGREGATOR_HOSTS: &[&str] = &["news.google.com"];

/// Cap on the number of cached digests (bounds memory). Each category/query the
/// user visits is one entry, so this comfortably covers the Discover pills plus
/// ad-hoc News searches.
const DIGEST_CACHE_MAX_ENTRIES: usize = 128;

/// A small TTL'd in-memory cache for fully-built (curated + enriched) digest
/// responses, so re-visiting a Discover category within the TTL is instant
/// instead of re-running the search fan-out and the og:image enrichment.
///
/// Privacy: like the search cache, the query lives only in RAM (never written
/// to disk or logs). Any lock failure degrades to a cache miss, so the feed
/// always works even if the cache is contended/poisoned.
pub struct DigestCache {
    ttl: Duration,
    map: std::sync::Mutex<HashMap<String, (std::time::Instant, NewsDigestResponse)>>,
}

impl DigestCache {
    /// Build a cache with the given TTL (seconds). `0` disables it.
    pub fn new(ttl_secs: u64) -> Self {
        DigestCache {
            ttl: Duration::from_secs(ttl_secs),
            map: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Whether caching is active (non-zero TTL).
    pub fn enabled(&self) -> bool {
        !self.ttl.is_zero()
    }

    /// Fetch a fresh (non-expired) entry, evicting it if stale.
    fn get(&self, key: &str) -> Option<NewsDigestResponse> {
        if !self.enabled() {
            return None;
        }
        let mut map = self.map.lock().ok()?;
        if let Some((inserted, value)) = map.get(key) {
            if inserted.elapsed() < self.ttl {
                return Some(value.clone());
            }
            map.remove(key);
        }
        None
    }

    /// Drop all cached digests (e.g. after deploy or explicit cache bust).
    pub fn clear(&self) {
        if let Ok(mut map) = self.map.lock() {
            map.clear();
        }
    }

    /// Insert an entry, bounding total size (drop expired first, then clear if
    /// still at capacity — same strategy as the search cache).
    fn put(&self, key: String, value: NewsDigestResponse) {
        if !self.enabled() {
            return;
        }
        if let Ok(mut map) = self.map.lock() {
            if map.len() >= DIGEST_CACHE_MAX_ENTRIES {
                map.retain(|_, (inserted, _)| inserted.elapsed() < self.ttl);
                if map.len() >= DIGEST_CACHE_MAX_ENTRIES {
                    map.clear();
                }
            }
            map.insert(key, (std::time::Instant::now(), value));
        }
    }
}

const DISCOVER_SNAPSHOT_CACHE_MAX_ENTRIES: usize = 64;

#[derive(Debug, Clone)]
struct CachedDiscoverSnapshot {
    inserted: std::time::Instant,
    updated_at_unix: i64,
    value: DiscoverSnapshotResponse,
}

/// Long-TTL cache for daily Discover snapshots. Unlike the short digest cache,
/// this is intentionally stable for hours so category tabs do not refetch on
/// every visit.
pub struct DiscoverSnapshotCache {
    ttl: Duration,
    map: std::sync::RwLock<HashMap<String, CachedDiscoverSnapshot>>,
}

impl DiscoverSnapshotCache {
    pub fn new(ttl_hours: u64) -> Self {
        DiscoverSnapshotCache {
            ttl: Duration::from_secs(ttl_hours.saturating_mul(3600)),
            map: std::sync::RwLock::new(HashMap::new()),
        }
    }

    pub fn enabled(&self) -> bool {
        !self.ttl.is_zero()
    }

    fn get(&self, key: &str) -> Option<DiscoverSnapshotResponse> {
        if !self.enabled() {
            return None;
        }
        // Use read lock for concurrent reads
        let map = self.map.read().ok()?;
        if let Some(entry) = map.get(key) {
            if entry.inserted.elapsed() < self.ttl {
                let mut value = entry.value.clone();
                value.cached = true;
                value.updated_at_unix = entry.updated_at_unix;
                value.updated_at = rfc3339_utc(entry.updated_at_unix);
                return Some(value);
            }
            // Entry expired - need write lock to remove, but don't block read
            // Just return None and let it be cleaned up on next write
        }
        None
    }

    fn put(&self, key: String, mut value: DiscoverSnapshotResponse, updated_at_unix: i64) {
        if !self.enabled() {
            return;
        }
        value.cached = false;
        value.updated_at_unix = updated_at_unix;
        value.updated_at = rfc3339_utc(updated_at_unix);
        if let Ok(mut map) = self.map.write() {
            if map.len() >= DISCOVER_SNAPSHOT_CACHE_MAX_ENTRIES {
                map.retain(|_, entry| entry.inserted.elapsed() < self.ttl);
                if map.len() >= DISCOVER_SNAPSHOT_CACHE_MAX_ENTRIES {
                    map.clear();
                }
            }
            map.insert(
                key,
                CachedDiscoverSnapshot {
                    inserted: std::time::Instant::now(),
                    updated_at_unix,
                    value,
                },
            );
        }
    }

    pub fn clear(&self) {
        if let Ok(mut map) = self.map.write() {
            map.clear();
        }
    }

    /// Save cache to disk for persistence across restarts
    pub fn save_to_disk(&self, path: &str) {
        let Ok(map) = self.map.read() else { return };
        let entries: Vec<_> = map.iter()
            .filter(|(_, e)| e.inserted.elapsed() < self.ttl)
            .map(|(k, e)| {
                let age_secs = e.inserted.elapsed().as_secs();
                (k.clone(), e.updated_at_unix, age_secs, e.value.clone())
            })
            .collect();
        drop(map);
        if let Ok(json) = serde_json::to_string(&entries) {
            let _ = std::fs::write(path, json);
            eprintln!("[metasearch] Discover cache saved: {} entries", entries.len());
        }
    }

    /// Load cache from disk on startup
    pub fn load_from_disk(&self, path: &str) {
        let Ok(data) = std::fs::read_to_string(path) else { return };
        let Ok(entries): Result<Vec<(String, i64, u64, DiscoverSnapshotResponse)>, _> = serde_json::from_str(&data) else { return };
        let Ok(mut map) = self.map.write() else { return };
        let now = std::time::Instant::now();
        let mut loaded = 0;
        for (key, updated_at_unix, age_secs, value) in entries {
            // Skip if too old
            if age_secs > self.ttl.as_secs() { continue; }
            // Reconstruct approximate insert time
            let inserted = now.checked_sub(Duration::from_secs(age_secs)).unwrap_or(now);
            map.insert(key, CachedDiscoverSnapshot { inserted, updated_at_unix, value });
            loaded += 1;
        }
        eprintln!("[metasearch] Discover cache loaded: {} entries from disk", loaded);
    }
}

const NEWS_IMAGE_CACHE_MAX_ENTRIES: usize = 128;

/// Short-TTL cache for asynchronous Discover image hydration. This is separate
/// from the digest cache so the text-first feed can stay fast while a colder,
/// slower media pass is reused on category revisits.
pub struct NewsImageCache {
    ttl: Duration,
    map: std::sync::Mutex<HashMap<String, (std::time::Instant, NewsImagesResponse)>>,
}

impl NewsImageCache {
    pub fn new(ttl_secs: u64) -> Self {
        NewsImageCache {
            ttl: Duration::from_secs(ttl_secs),
            map: std::sync::Mutex::new(HashMap::new()),
        }
    }

    pub fn enabled(&self) -> bool {
        !self.ttl.is_zero()
    }

    fn get(&self, key: &str) -> Option<NewsImagesResponse> {
        if !self.enabled() {
            return None;
        }
        let mut map = self.map.lock().ok()?;
        if let Some((inserted, value)) = map.get(key) {
            if inserted.elapsed() < self.ttl {
                return Some(value.clone());
            }
            map.remove(key);
        }
        None
    }

    fn put(&self, key: String, value: NewsImagesResponse) {
        if !self.enabled() {
            return;
        }
        if let Ok(mut map) = self.map.lock() {
            if map.len() >= NEWS_IMAGE_CACHE_MAX_ENTRIES {
                map.retain(|_, (inserted, _)| inserted.elapsed() < self.ttl);
                if map.len() >= NEWS_IMAGE_CACHE_MAX_ENTRIES {
                    map.clear();
                }
            }
            map.insert(key, (std::time::Instant::now(), value));
        }
    }

    pub fn clear(&self) {
        if let Ok(mut map) = self.map.lock() {
            map.clear();
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DigestArticle {
    pub title: String,
    pub url: String,
    pub engine: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub source: String,
    /// 1–2 sentence teaser from RSS/search snippet — not a full rewrite.
    pub teaser: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub image_url_large: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub publisher_url: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub published_date: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub favicon_url: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub category: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewsDigestResponse {
    pub query: String,
    pub articles: Vec<DigestArticle>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoverSnapshotResponse {
    pub query: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub category: String,
    pub cached: bool,
    pub updated_at_unix: i64,
    pub updated_at: String,
    pub ttl_hours: u64,
    pub max_age_hours: u64,
    pub image_count: usize,
    pub real_image_count: usize,
    pub fallback_image_count: usize,
    pub articles: Vec<DigestArticle>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewsImagesRequest {
    pub query: String,
    #[serde(default)]
    pub limit: usize,
    #[serde(default)]
    pub articles: Vec<DigestArticle>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewsImageCandidate {
    pub image_url: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub article_url: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub title: String,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewsImagesResponse {
    pub query: String,
    pub cached: bool,
    pub candidates: Vec<NewsImageCandidate>,
}

/// `GET /api/v1/news_digest?q=&limit=5&refresh=1`
pub async fn run_news_digest(
    query: &str,
    limit: usize,
    settings: &Settings,
    rt: &Runtime,
    refresh: bool,
) -> NewsDigestResponse {
    let q = query.trim();
    if q.is_empty() {
        return NewsDigestResponse {
            query: String::new(),
            articles: Vec::new(),
        };
    }

    let lim = limit.clamp(1, MAX_DIGEST);
    let news_cfg = &settings.search.news;

    // Warm-path fast return: a fully-built (curated + enriched) digest for this
    // (query, limit, locale) served straight from the short-TTL cache. The
    // locale component keeps Korean and English variants of the same query from
    // colliding even though the query string already implies the script.
    let cache_key = digest_cache_key(q, lim, &settings.search.default_language);
    if !refresh {
        if let Some(hit) = rt.digest_cache.get(&cache_key) {
            return hit;
        }
    }

    let mut params = SearchParams::new(q);
    params.categories = vec!["news".into()];
    params.ai_answer = Some(false);

    let response = search_all(&params, settings, rt).await;
    // Curate the full aggregated set (freshness / diversity / dedup / quality)
    // *before* slicing to `lim`, so selection happens across all candidates
    // rather than just whatever the base score ranked into the first few slots.
    let now = now_unix();
    let selected = curate_news(response.results, q, news_cfg, now, lim);
    let mut articles: Vec<DigestArticle> = selected.iter().map(digest_from_result).collect();

    prepare_for_image_enrichment(&mut articles);
    enrich_with_og_images(&mut articles, news_cfg, settings, rt).await;

    let out = NewsDigestResponse {
        query: q.to_string(),
        articles,
    };
    rt.digest_cache.put(cache_key, out.clone());
    out
}

/// `GET /api/v1/discover_snapshot?q=&category=&limit=8&language=&country=&refresh=1`
pub async fn run_discover_snapshot(
    query: &str,
    category: &str,
    limit: usize,
    language: Option<&str>,
    country: Option<&str>,
    settings: &Settings,
    rt: &Runtime,
    refresh: bool,
) -> DiscoverSnapshotResponse {
    let q = query.trim();
    if q.is_empty() {
        return empty_discover_snapshot(category, settings);
    }

    let lim = limit.clamp(1, MAX_DIGEST);
    let news_cfg = &settings.search.news;
    let ttl_hours = news_cfg.discover_cache_ttl_hours;
    let max_age_hours = news_cfg.discover_max_age_hours;
    let lang = language.unwrap_or(&settings.search.default_language);
    // Normalize locale for cache key: ko-KR -> ko, en-US -> en, etc.
    let lang_base = lang.split('-').next().unwrap_or(lang);
    let cache_key = discover_snapshot_cache_key(
        q,
        category,
        lim,
        lang_base,
        max_age_hours,
    );
    if !refresh {
        if let Some(hit) = rt.discover_snapshot_cache.get(&cache_key) {
            return hit;
        }
    }

    let lang_code = match lang.split('-').next().unwrap_or(lang) {
        "ko" => Some("ko"),
        "ja" => Some("ja"),
        "zh" => Some("zh"),
        "es" => Some("es"),
        "fr" => Some("fr"),
        "de" => Some("de"),
        "en" => Some("en"),
        "ar" => Some("ar"),
        "pt" => Some("pt"),
        "ru" => Some("ru"),
        "it" => Some("it"),
        "nl" => Some("nl"),
        "pl" => Some("pl"),
        "tr" => Some("tr"),
        "vi" => Some("vi"),
        "th" => Some("th"),
        "id" => Some("id"),
        "hi" => Some("hi"),
        "te" => Some("te"),
        "ta" => Some("ta"),
        "bn" => Some("bn"),
        "ml" => Some("ml"),
        "kn" => Some("kn"),
        "gu" => Some("gu"),
        "pa" => Some("pa"),
        "mr" => Some("mr"),
        "he" => Some("he"),
        "sv" => Some("sv"),
        "no" => Some("no"),
        "da" => Some("da"),
        "fi" => Some("fi"),
        "el" => Some("el"),
        "cs" => Some("cs"),
        "hu" => Some("hu"),
        "ro" => Some("ro"),
        "uk" => Some("uk"),
        // Support all other languages
        other => Some(other),
    };

    // Use language-specific search queries
    // But if the query is a specific search term (not a generic news query), use it directly
    let is_specific_query = !q.is_empty()
        && !matches!(q.to_lowercase().as_str(),
            "news" | "뉴스" | "latest" | "최신" | "ニュース" | "新闻" |
            "최신 뉴스" | "속보" | "최신 뉴스 헤드라인 인기" |
            "latest news" | "breaking news" | "top stories")
        && q != category;
    let search_queries = discover_seed_queries(category, q, lang_code);
    let search_query = if is_specific_query {
        q.to_string()
    } else {
        search_queries.first().cloned().unwrap_or_else(|| q.to_string())
    };

    // Standalone mode: use native RSS feeds search with category filter
    // "top" is a meta-category meaning "all top stories", not a DB category
    let category_filter = if category.is_empty() || category == "all" || category == "news" || category == "top" {
        None
    } else {
        Some(category)
    };
    let all_results = fetch_news_fallback(&search_query, 200, lang_code, category_filter, country, settings, rt).await;

    // Language filter helper - requires significant presence of target language
    fn title_matches_lang(title: &str, lang: &str) -> bool {
        let lang_code = lang.split('-').next().unwrap_or(lang);
        match lang_code {
            "ko" => {
                // Korean: require majority Korean (50%+) and first 10 chars should have Korean
                let hangul_count = title.chars().filter(|c| ('\u{AC00}'..='\u{D7AF}').contains(c)).count();
                let total_letters = title.chars().filter(|c| c.is_alphabetic()).count();
                let first_part: String = title.chars().take(15).collect();
                let first_has_korean = first_part.chars().any(|c| ('\u{AC00}'..='\u{D7AF}').contains(&c));
                first_has_korean && hangul_count >= 3 && (total_letters == 0 || hangul_count * 100 / total_letters.max(1) >= 50)
            }
            "ja" => title.chars().any(|c|
                ('\u{3040}'..='\u{309F}').contains(&c) || // Hiragana
                ('\u{30A0}'..='\u{30FF}').contains(&c) || // Katakana
                ('\u{4E00}'..='\u{9FFF}').contains(&c)    // CJK
            ),
            "zh" => title.chars().any(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c)), // CJK
            "ar" => title.chars().any(|c| ('\u{0600}'..='\u{06FF}').contains(&c)), // Arabic
            "he" => title.chars().any(|c| ('\u{0590}'..='\u{05FF}').contains(&c)), // Hebrew
            "ru" | "uk" => title.chars().any(|c| ('\u{0400}'..='\u{04FF}').contains(&c)), // Cyrillic
            "hi" => title.chars().any(|c| ('\u{0900}'..='\u{097F}').contains(&c)), // Devanagari (Hindi)
            "bn" => title.chars().any(|c| ('\u{0980}'..='\u{09FF}').contains(&c)), // Bengali
            "te" => title.chars().any(|c| ('\u{0C00}'..='\u{0C7F}').contains(&c)), // Telugu
            "ta" => title.chars().any(|c| ('\u{0B80}'..='\u{0BFF}').contains(&c)), // Tamil
            "ml" => title.chars().any(|c| ('\u{0D00}'..='\u{0D7F}').contains(&c)), // Malayalam
            "kn" => title.chars().any(|c| ('\u{0C80}'..='\u{0CFF}').contains(&c)), // Kannada
            "gu" => title.chars().any(|c| ('\u{0A80}'..='\u{0AFF}').contains(&c)), // Gujarati
            "pa" => title.chars().any(|c| ('\u{0A00}'..='\u{0A7F}').contains(&c)), // Punjabi (Gurmukhi)
            "mr" => title.chars().any(|c| ('\u{0900}'..='\u{097F}').contains(&c)), // Marathi (Devanagari)
            "th" => title.chars().any(|c| ('\u{0E00}'..='\u{0E7F}').contains(&c)), // Thai
            "el" => title.chars().any(|c| ('\u{0370}'..='\u{03FF}').contains(&c)), // Greek
            "en" => {
                // English: reject if title has non-Latin scripts
                !title.chars().any(|c|
                    ('\u{0400}'..='\u{04FF}').contains(&c) || // Cyrillic
                    ('\u{0600}'..='\u{06FF}').contains(&c) || // Arabic
                    ('\u{0590}'..='\u{05FF}').contains(&c) || // Hebrew
                    ('\u{0900}'..='\u{097F}').contains(&c) || // Devanagari
                    ('\u{0980}'..='\u{09FF}').contains(&c) || // Bengali
                    ('\u{0E00}'..='\u{0E7F}').contains(&c) || // Thai
                    ('\u{AC00}'..='\u{D7AF}').contains(&c) || // Hangul
                    ('\u{3040}'..='\u{30FF}').contains(&c) || // Japanese kana
                    ('\u{4E00}'..='\u{9FFF}').contains(&c)    // CJK
                )
            },
            _ => true, // Other languages: no filter
        }
    }

    // Category matching helper - strict matching for specific categories
    fn category_matches(result_cat: &str, target: &str, _title: &str) -> bool {
        // No filter for empty/all/news/top targets (top = all top stories)
        if target.is_empty() || target == "all" || target == "news" || target == "top" {
            return true;
        }
        let rc = result_cat.to_lowercase();
        let tc = target.to_lowercase();

        // Direct match
        if rc == tc {
            return true;
        }

        // Aliases for related categories
        match (rc.as_str(), tc.as_str()) {
            ("culture", "art") | ("art", "culture") => true,
            ("business", "economy") | ("economy", "business") | ("finance", "business") | ("finance", "economy") => true,
            ("technology", "tech") | ("tech", "technology") => true,
            ("tech", "ai") | ("ai", "tech") => true,  // AI often classified as tech
            ("politics", "world") | ("world", "politics") => true,  // Political news often in world
            ("society", "politics") | ("politics", "society") => true,
            ("entertainment", "culture") | ("culture", "entertainment") => true,
            _ => false,
        }
        // Note: "news"/"general" no longer match specific categories
        // This ensures sports/tech/etc requests return only those categories
    }

    let results: Vec<SearchResult> = all_results
        .into_iter()
        .filter(|r| {
            // Language filter:
            // - local_feeds already filtered by DB language field → trust it
            // - Other engines: use title script matching
            let lang_ok = if r.engine == "local_feeds" {
                true  // DB already filtered by feed's language tag
            } else {
                title_matches_lang(&r.title, lang)
            };
            // Category filter:
            // - local_feeds already filtered by DB category query → trust it
            // - Other engines: use category matching
            let cat_ok = if r.engine == "local_feeds" {
                true  // DB already filtered by category
            } else {
                category_matches(&r.category, category, &r.title)
            };
            lang_ok && cat_ok
        })
        .collect();

    let now = now_unix();
    let mut snapshot_cfg = news_cfg.clone();
    snapshot_cfg.enrich_max = snapshot_cfg.enrich_max.max(lim);
    snapshot_cfg.min_results = 1;
    // Use regular curate_news which allows undated items (search results often lack dates)
    let selected = curate_news(results, q, &snapshot_cfg, now, lim);
    let mut articles: Vec<DigestArticle> = selected.iter().map(digest_from_result).collect();

    // Try OG image enrichment for articles
    prepare_for_image_enrichment(&mut articles);
    enrich_with_og_images(&mut articles, &snapshot_cfg, settings, rt).await;

    apply_favicon_fallbacks(&mut articles, settings);
    let real_image_count = articles
        .iter()
        .filter(|a| has_real_digest_image(&a.image_url_large))
        .count();
    let fallback_image_count = articles
        .iter()
        .filter(|a| !has_real_digest_image(&a.image_url_large) && !a.favicon_url.trim().is_empty())
        .count();
    let image_count = real_image_count + fallback_image_count;
    let out = DiscoverSnapshotResponse {
        query: q.to_string(),
        category: category.trim().to_string(),
        cached: false,
        updated_at_unix: now,
        updated_at: rfc3339_utc(now),
        ttl_hours,
        max_age_hours,
        image_count,
        real_image_count,
        fallback_image_count,
        articles,
    };
    rt.discover_snapshot_cache.put(cache_key, out.clone(), now);
    out
}

fn empty_discover_snapshot(category: &str, settings: &Settings) -> DiscoverSnapshotResponse {
    DiscoverSnapshotResponse {
        query: String::new(),
        category: category.trim().to_string(),
        cached: false,
        updated_at_unix: 0,
        updated_at: String::new(),
        ttl_hours: settings.search.news.discover_cache_ttl_hours,
        max_age_hours: settings.search.news.discover_max_age_hours,
        image_count: 0,
        real_image_count: 0,
        fallback_image_count: 0,
        articles: Vec::new(),
    }
}

fn discover_snapshot_cache_key(
    query: &str,
    category: &str,
    _limit: usize,  // Ignored - cache shared regardless of limit
    locale: &str,
    max_age_hours: u64,
) -> String {
    format!(
        "{}\u{1f}{}\u{1f}{}\u{1f}{}",
        category.trim(),
        query.trim(),
        locale,
        max_age_hours
    )
}

fn discover_seed_queries(category: &str, query: &str, lang: Option<&str>) -> Vec<String> {
    let q = query.trim();
    let lang_code = lang.and_then(|l| l.split('-').next()).unwrap_or("en");

    // For non-English languages, start with native language keywords
    let mut seeds: Vec<String> = Vec::new();

    let extras: Vec<&str> = match (category.trim(), lang_code) {
        // Hindi (hi)
        ("tech", "hi") => vec!["प्रौद्योगिकी समाचार", "टेक न्यूज़", "तकनीक"],
        ("world", "hi") => vec!["विश्व समाचार", "अंतरराष्ट्रीय समाचार", "विदेश"],
        ("politics", "hi") => vec!["राजनीति समाचार", "सरकार", "चुनाव"],
        ("economy", "hi") => vec!["अर्थव्यवस्था समाचार", "व्यापार", "बिजनेस"],
        ("sports", "hi") => vec!["खेल समाचार", "क्रिकेट", "फुटबॉल"],
        ("entertainment", "hi") => vec!["मनोरंजन समाचार", "बॉलीवुड", "सिनेमा"],
        ("health", "hi") => vec!["स्वास्थ्य समाचार", "चिकित्सा", "स्वास्थ्य"],
        ("culture", "hi") => vec!["संस्कृति समाचार", "कला", "साहित्य"],
        (_, "hi") => vec!["ताजा समाचार", "ब्रेकिंग न्यूज"],

        // Telugu (te)
        ("tech", "te") => vec!["టెక్నాలజీ వార్తలు", "సాంకేతికత", "టెక్"],
        ("world", "te") => vec!["ప్రపంచ వార్తలు", "అంతర్జాతీయ", "విదేశీ"],
        ("politics", "te") => vec!["రాజకీయ వార్తలు", "ప్రభుత్వం", "ఎన్నికలు"],
        ("economy", "te") => vec!["ఆర్థిక వార్తలు", "వ్యాపారం", "బిజినెస్"],
        ("sports", "te") => vec!["క్రీడల వార్తలు", "క్రికెట్", "ఫుట్‌బాల్"],
        ("entertainment", "te") => vec!["వినోద వార్తలు", "టాలీవుడ్", "సినిమా"],
        ("health", "te") => vec!["ఆరోగ్య వార్తలు", "వైద్యం", "ఆరోగ్యం"],
        ("culture", "te") => vec!["సంస్కృతి వార్తలు", "కళ", "సాహిత్యం"],
        (_, "te") => vec!["తాజా వార్తలు", "బ్రేకింగ్ న్యూస్"],

        // Tamil (ta)
        ("tech", "ta") => vec!["தொழில்நுட்ப செய்திகள்", "டெக்", "தொழில்நுட்பம்"],
        ("sports", "ta") => vec!["விளையாட்டு செய்திகள்", "கிரிக்கெட்", "ஃபுட்பால்"],
        ("entertainment", "ta") => vec!["பொழுதுபோக்கு செய்திகள்", "கோலிவுட்", "சினிமா"],
        (_, "ta") => vec!["சமீபத்திய செய்திகள்", "பிரேக்கிங் நியூஸ்"],

        // Bengali (bn)
        ("tech", "bn") => vec!["প্রযুক্তি সংবাদ", "টেক", "প্রযুক্তি"],
        ("sports", "bn") => vec!["খেলার সংবাদ", "ক্রিকেট", "ফুটবল"],
        ("entertainment", "bn") => vec!["বিনোদন সংবাদ", "বলিউড", "সিনেমা"],
        (_, "bn") => vec!["সাম্প্রতিক সংবাদ", "ব্রেকিং নিউজ"],

        // Slovenian (sl)
        ("tech", "sl") => vec!["tehnologija", "tehnološke novice", "IT"],
        ("world", "sl") => vec!["svet", "mednarodne novice", "tujina"],
        ("politics", "sl") => vec!["politika", "vlada", "volitve"],
        ("economy", "sl") => vec!["gospodarstvo", "podjetja", "ekonomija"],
        ("sports", "sl") => vec!["šport", "nogomet", "košarka"],
        ("entertainment", "sl") => vec!["zabava", "film", "glasba"],
        ("health", "sl") => vec!["zdravje", "medicina", "zdravstvo"],
        ("culture", "sl") => vec!["kultura", "umetnost", "razstave"],
        (_, "sl") => vec!["novice", "zadnje novice", "aktualno"],

        // Korean (ko) - avoid generic "뉴스" to prevent FTS over-matching
        ("tech", "ko") => vec!["IT", "기술", "테크", "스마트폰", "반도체"],
        ("ai", "ko") => vec!["AI", "인공지능", "챗GPT", "머신러닝", "딥러닝"],
        ("world", "ko") => vec!["국제", "해외", "세계", "외교", "미국"],
        ("politics", "ko") => vec!["정치", "국회", "대통령", "여당", "야당"],
        ("economy", "ko") => vec!["경제", "기업", "산업", "무역", "수출"],
        ("finance", "ko") => vec!["금융", "증시", "주식", "코스피", "환율"],
        ("sports", "ko") => vec!["스포츠", "축구", "야구", "농구", "올림픽"],
        ("entertainment", "ko") => vec!["연예", "K-pop", "드라마", "아이돌", "배우"],
        ("health", "ko") => vec!["건강", "의료", "병원", "질병", "치료"],
        ("climate", "ko") => vec!["기후", "환경", "탄소", "온실가스", "날씨"],
        ("science", "ko") => vec!["과학", "연구", "우주", "발견", "실험"],
        ("art", "ko") | ("culture", "ko") => vec!["문화", "예술", "공연", "전시", "박물관"],
        (_, "ko") => vec!["최신 뉴스", "속보"],

        // Japanese (ja)
        ("tech", "ja") => vec!["テクノロジーニュース", "技術", "IT"],
        ("world", "ja") => vec!["国際ニュース", "世界", "海外"],
        ("politics", "ja") => vec!["政治ニュース", "国会", "政府"],
        ("economy", "ja") => vec!["経済ニュース", "ビジネス", "企業"],
        ("sports", "ja") => vec!["スポーツニュース", "野球", "サッカー"],
        ("entertainment", "ja") => vec!["芸能ニュース", "エンタメ", "映画"],
        (_, "ja") => vec!["最新ニュース", "速報"],

        // Chinese (zh)
        ("tech", "zh") => vec!["科技新闻", "技术", "IT"],
        ("world", "zh") => vec!["国际新闻", "世界", "海外"],
        ("politics", "zh") => vec!["政治新闻", "政府", "国会"],
        ("economy", "zh") => vec!["经济新闻", "商业", "企业"],
        ("sports", "zh") => vec!["体育新闻", "足球", "篮球"],
        ("entertainment", "zh") => vec!["娱乐新闻", "电影", "音乐"],
        (_, "zh") => vec!["最新新闻", "突发新闻"],

        // Default English
        ("tech", _) => vec!["technology news", "tech news", "IT news"],
        ("ai", _) => vec!["AI news", "artificial intelligence", "machine learning", "ChatGPT"],
        ("world", _) => vec!["world news", "international news", "global affairs"],
        ("politics", _) => vec!["politics news", "government election", "policy news"],
        ("business", _) | ("economy", _) => vec!["business news", "economy news", "companies news"],
        ("finance", _) => vec!["finance news", "markets news", "stocks news"],
        ("health", _) => vec!["health news", "medicine news", "public health news"],
        ("climate", _) => vec!["climate news", "environment news", "weather", "carbon"],
        ("science", _) => vec!["science news", "research news", "space news", "discovery"],
        ("sports", _) => vec!["sports news", "football news", "basketball news"],
        ("entertainment", _) => vec!["entertainment news", "movies news", "music news"],
        ("art", _) | ("culture", _) => vec!["art news", "culture news", "museum news"],
        _ => vec![],
    };

    for seed in extras {
        if !seeds.iter().any(|s| s == seed) {
            seeds.push(seed.to_string());
        }
    }
    // Add original query as fallback
    if !seeds.iter().any(|s| s == q) {
        seeds.push(q.to_string());
    }
    // Ensure at least one query
    if seeds.is_empty() {
        seeds.push(q.to_string());
    }
    seeds
}

/// Cache key for a digest: `query \u{1f} limit \u{1f} locale`. The unit
/// separator can't appear in a query, so distinct inputs never collide.
fn digest_cache_key(query: &str, limit: usize, locale: &str) -> String {
    format!("{query}\u{1f}{limit}\u{1f}{locale}")
}

pub async fn run_news_images(
    req: NewsImagesRequest,
    settings: &Settings,
    rt: &Runtime,
    refresh: bool,
) -> NewsImagesResponse {
    let q = req.query.trim();
    if q.is_empty() {
        return NewsImagesResponse {
            query: String::new(),
            cached: false,
            candidates: Vec::new(),
        };
    }

    let limit = req.limit.clamp(1, MAX_DIGEST).max(1);
    let mut articles: Vec<DigestArticle> = req
        .articles
        .into_iter()
        .filter(|a| !has_real_digest_image(&a.image_url_large))
        .take(limit)
        .collect();
    prepare_for_image_enrichment(&mut articles);

    let cache_key = news_image_cache_key(q, limit, &settings.search.default_language, &articles);
    if !refresh {
        if let Some(mut hit) = rt.news_image_cache.get(&cache_key) {
            hit.cached = true;
            return hit;
        }
    }

    // Only use OG images from actual articles - no keyword search (returns irrelevant images)
    let candidates = collect_article_image_candidates(&articles, settings, rt).await;

    let out = NewsImagesResponse {
        query: q.to_string(),
        cached: false,
        candidates,
    };
    rt.news_image_cache.put(cache_key, out.clone());
    out
}

fn news_image_cache_key(
    query: &str,
    limit: usize,
    locale: &str,
    articles: &[DigestArticle],
) -> String {
    let mut parts = vec![query.to_string(), limit.to_string(), locale.to_string()];
    for a in articles {
        parts.push(a.url.clone());
        parts.push(headline_for_search(&a.title));
    }
    parts.join("\u{1f}")
}

async fn collect_article_image_candidates(
    articles: &[DigestArticle],
    settings: &Settings,
    rt: &Runtime,
) -> Vec<NewsImageCandidate> {
    if articles.is_empty() {
        return Vec::new();
    }
    let cfg = &settings.search.news;
    let per_fetch = Duration::from_millis(cfg.enrich_timeout_ms.clamp(900, 6000));
    let total_budget = Duration::from_millis(cfg.enrich_budget_ms.clamp(1500, 12000));
    let concurrency = cfg.enrich_concurrency.max(1);
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(concurrency));
    let mut in_flight = futures_util::stream::FuturesUnordered::new();

    for article in articles.iter().cloned() {
        let sem = semaphore.clone();
        in_flight.push(async move {
            let _permit = sem.acquire_owned().await.ok();
            let image = tokio::time::timeout(
                per_fetch,
                fetch_digest_og_image(&article, per_fetch, settings, rt),
            )
            .await
            .ok()
            .flatten();
            (article, image)
        });
    }

    let mut out = Vec::new();
    let deadline = tokio::time::sleep(total_budget);
    tokio::pin!(deadline);
    loop {
        if in_flight.is_empty() {
            break;
        }
        tokio::select! {
            biased;
            next = futures_util::StreamExt::next(&mut in_flight) => {
                match next {
                    Some((article, Some(image_url))) if is_news_image_candidate(&image_url) => {
                        out.push(NewsImageCandidate {
                            image_url,
                            article_url: article.url,
                            title: article.title,
                            source: "article_og".into(),
                        });
                    }
                    Some(_) => {}
                    None => break,
                }
            }
            _ = &mut deadline => break,
        }
    }
    dedupe_image_candidates(out, articles.len())
}

async fn topic_image_candidates(
    query: &str,
    limit: usize,
    settings: &Settings,
    rt: &Runtime,
) -> Vec<NewsImageCandidate> {
    if limit == 0 {
        return Vec::new();
    }
    let q = format!("!bing_images {}", topic_image_query(query));
    let mut params = SearchParams::new(&q);
    params.categories = vec!["images".into()];
    params.ai_answer = Some(false);
    let response = tokio::time::timeout(
        Duration::from_millis(4000),
        search_all(&params, settings, rt),
    )
    .await
    .ok();
    let Some(response) = response else {
        return Vec::new();
    };
    // Skip term filtering for now - just return relevant images
    let candidates = response.results.into_iter().filter_map(|r| {
        let image_url = large_image(&r);
        if !is_news_image_candidate(&image_url) {
            return None;
        }
        Some(NewsImageCandidate {
            image_url,
            article_url: String::new(),
            title: r.title,
            source: format!("topic_image:{}", r.engine),
        })
    });
    dedupe_image_candidates(candidates.collect(), limit)
}

async fn ensure_snapshot_images(
    _query: &str,
    articles: &mut [DigestArticle],
    min_image_count: usize,
    settings: &Settings,
    rt: &Runtime,
) {
    if articles.is_empty() {
        return;
    }
    let target = min_image_count.min(articles.len());
    let have = articles
        .iter()
        .filter(|a| has_real_digest_image(&a.image_url_large))
        .count();
    if have >= target {
        return;
    }

    let missing: Vec<DigestArticle> = articles
        .iter()
        .filter(|a| !has_real_digest_image(&a.image_url_large))
        .cloned()
        .collect();
    // Only use OG images from actual articles - skip generic image search
    // (keyword search returns irrelevant images)
    let candidates = collect_article_image_candidates(&missing, settings, rt).await;
    let candidates = dedupe_image_candidates(candidates, articles.len());
    let mut by_article = HashMap::new();
    let mut topic = Vec::new();
    for c in candidates {
        if !is_news_image_candidate(&c.image_url) {
            continue;
        }
        if c.article_url.trim().is_empty() {
            topic.push(c.image_url);
        } else {
            by_article.insert(c.article_url, c.image_url);
        }
    }

    let mut used: BTreeSet<String> = articles
        .iter()
        .filter_map(|a| {
            let u = a.image_url_large.trim();
            (!u.is_empty()).then(|| u.to_string())
        })
        .collect();
    let mut have_count = have;
    for article in articles.iter_mut() {
        if has_real_digest_image(&article.image_url_large) {
            continue;
        }
        let image = by_article
            .remove(&article.url)
            .or_else(|| topic.pop())
            .filter(|u| used.insert(u.clone()));
        if let Some(image) = image {
            article.image_url_large = image;
            have_count += 1;
        }
        if have_count >= target {
            break;
        }
    }
}

fn apply_favicon_fallbacks(articles: &mut [DigestArticle], settings: &Settings) {
    for article in articles.iter_mut() {
        if has_real_digest_image(&article.image_url_large) || !article.favicon_url.trim().is_empty()
        {
            continue;
        }
        article.favicon_url = publisher_favicon_url(article, settings);
    }
}

fn publisher_favicon_url(article: &DigestArticle, settings: &Settings) -> String {
    let host = url::Url::parse(article.publisher_url.trim())
        .or_else(|_| url::Url::parse(article.url.trim()))
        .ok()
        .and_then(|u| {
            u.host_str()
                .map(|h| h.trim_start_matches("www.").to_string())
        })
        .unwrap_or_default();
    // If Google News, try to extract publisher from title suffix
    if host.is_empty() || host == "news.google.com" || host.ends_with(".google.com") {
        let publisher = extract_publisher_from_title(&article.title);
        if publisher.is_empty() {
            return String::new();
        }
        // Use DuckDuckGo favicon service with publisher name
        return format!("https://icons.duckduckgo.com/ip3/{}.ico", publisher);
    }
    let resolver = settings.search.favicon_resolver.trim();
    if resolver.is_empty() {
        return String::new();
    }
    resolver.replace("{domain}", &host)
}

/// Extract publisher name from title suffix (e.g., "News Title - Publisher")
fn extract_publisher_from_title(title: &str) -> String {
    for sep in [" - ", " | ", " · ", " — "] {
        if let Some(idx) = title.rfind(sep) {
            let pub_name = title[idx + sep.len()..].trim();
            if !pub_name.is_empty() && pub_name.chars().count() < 30 {
                // Convert Korean publisher names to rough domain guesses
                let domain = publisher_to_domain(pub_name);
                if !domain.is_empty() {
                    return domain;
                }
            }
        }
    }
    String::new()
}

/// Map common Korean publisher names to domain
fn publisher_to_domain(name: &str) -> String {
    let lower = name.to_lowercase();
    match lower.as_str() {
        "연합뉴스" | "yonhapnews" => "yonhapnews.co.kr".into(),
        "조선일보" | "chosun" => "chosun.com".into(),
        "중앙일보" | "joongang" => "joongang.co.kr".into(),
        "동아일보" | "donga" => "donga.com".into(),
        "한겨레" | "hani" => "hani.co.kr".into(),
        "경향신문" => "khan.co.kr".into(),
        "한국경제" | "한경" => "hankyung.com".into(),
        "매일경제" | "매경" => "mk.co.kr".into(),
        "서울경제" => "sedaily.com".into(),
        "jtbc" => "jtbc.co.kr".into(),
        "mbc" => "imnews.imbc.com".into(),
        "kbs" => "news.kbs.co.kr".into(),
        "sbs" => "news.sbs.co.kr".into(),
        "ytn" => "ytn.co.kr".into(),
        "머니투데이" => "mt.co.kr".into(),
        "뉴시스" => "newsis.com".into(),
        "뉴스1" => "news1.kr".into(),
        "서울신문" => "seoul.co.kr".into(),
        "세계일보" => "segye.com".into(),
        "국민일보" => "kmib.co.kr".into(),
        "문화일보" => "munhwa.com".into(),
        "헤럴드경제" => "heraldcorp.com".into(),
        "아시아경제" => "asiae.co.kr".into(),
        "이데일리" => "edaily.co.kr".into(),
        "디지털타임스" => "dt.co.kr".into(),
        "전자신문" => "etnews.com".into(),
        "zdnet korea" => "zdnet.co.kr".into(),
        "테크레시피" => "techrecipe.co.kr".into(),
        _ => {
            // For unknown publishers, just return empty (no valid domain guess)
            String::new()
        }
    }
}

fn topic_image_query(query: &str) -> String {
    let q = query.trim();
    // Clean query for image search - remove common suffixes and special chars
    let clean: String = q
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect();
    let clean = clean.trim();
    if clean.is_empty() {
        return String::new();
    }
    format!("{clean} news photo")
}

fn is_news_image_candidate(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    if lower.is_empty()
        || is_generic_google_placeholder(&lower)
        || lower.contains("upload.wikimedia.org")
        || lower.contains("wikimedia.org")
        || lower.contains("wikinews/")
        || lower.contains("zootube")
        || lower.contains("porn")
        || lower.contains("sex")
        || lower.contains("adult")
    {
        return false;
    }
    crate::thumbnail::is_usable_thumbnail_url(url)
}

fn dedupe_image_candidates(
    candidates: Vec<NewsImageCandidate>,
    limit: usize,
) -> Vec<NewsImageCandidate> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for c in candidates {
        if c.image_url.trim().is_empty() || !seen.insert(c.image_url.clone()) {
            continue;
        }
        out.push(c);
        if out.len() >= limit {
            break;
        }
    }
    out
}

/// Google News RSS assigns the same generic thumbnail to unrelated stories.
pub(crate) fn is_generic_google_placeholder(url: &str) -> bool {
    crate::thumbnail::is_google_news_branding_url(url)
}

fn has_real_digest_image(url: &str) -> bool {
    !url.trim().is_empty() && !is_generic_google_placeholder(url)
}

fn prepare_for_image_enrichment(articles: &mut [DigestArticle]) {
    for a in articles.iter_mut() {
        if is_generic_google_placeholder(&a.image_url_large) {
            a.image_url_large.clear();
        }
    }
}

/// For curated cards that lack a real image, fetch `og:image` from the publisher
/// page (GN redirect URLs are decoded first).
async fn enrich_with_og_images(
    articles: &mut [DigestArticle],
    cfg: &NewsSettings,
    settings: &Settings,
    rt: &Runtime,
) {
    if cfg.enrich_max == 0 {
        return;
    }
    let targets: Vec<usize> = articles
        .iter()
        .enumerate()
        .filter(|(_, a)| a.image_url_large.trim().is_empty() && is_safe_public_url(&a.url))
        .map(|(i, _)| i)
        .take(cfg.enrich_max)
        .collect();
    if targets.is_empty() {
        return;
    }

    let fetch_timeout = Duration::from_millis(cfg.enrich_timeout_ms.max(1));
    let total_budget = Duration::from_millis(cfg.enrich_budget_ms.max(1));
    let concurrency = cfg.enrich_concurrency.max(1);
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(concurrency));

    let mut in_flight = futures_util::stream::FuturesUnordered::new();
    for i in targets {
        let article = articles[i].clone();
        let sem = semaphore.clone();
        in_flight.push(async move {
            let _permit = sem.acquire_owned().await.ok();
            (
                i,
                fetch_digest_og_image(&article, fetch_timeout, settings, rt).await,
            )
        });
    }

    let deadline = tokio::time::sleep(total_budget);
    tokio::pin!(deadline);
    loop {
        if in_flight.is_empty() {
            break;
        }
        tokio::select! {
            biased;
            next = futures_util::StreamExt::next(&mut in_flight) => {
                match next {
                    Some((i, Some(u))) => {
                        if has_real_digest_image(&u)
                            && thumbnail_quality(&u) != ThumbnailQuality::Small
                        {
                            articles[i].image_url_large = u;
                        }
                    }
                    Some((_, None)) => {}
                    None => break,
                }
            }
            _ = &mut deadline => {
                // Drop pending fetches. Completed images are already applied;
                // slow publishers or image-search fallbacks must not hold up
                // the first paint of Discover/News.
                break;
            }
        }
    }
}

/// Decode GN redirects, then pull publisher `og:image`.
async fn fetch_digest_og_image(
    article: &DigestArticle,
    timeout: Duration,
    settings: &Settings,
    rt: &Runtime,
) -> Option<String> {
    let gn_url = article.url.trim();
    if gn_url.is_empty() || !is_safe_public_url(gn_url) {
        return None;
    }
    if crate::googlenews_decode::is_google_news_article_url(gn_url) {
        let resolve_budget = timeout.min(Duration::from_millis(2500));
        if let Ok(Some(resolved)) = tokio::time::timeout(
            resolve_budget,
            crate::googlenews_decode::resolve_publisher_url(gn_url),
        )
        .await
        {
            if is_safe_public_url(&resolved) {
                return fetch_og_image(&resolved, timeout)
                    .await
                    .filter(|u| has_real_digest_image(u));
            }
        }
        if let Some(found) = find_publisher_article_url(article, settings, rt).await {
            if let Some(img) = fetch_og_image(&found, timeout)
                .await
                .filter(|u| has_real_digest_image(u))
            {
                return Some(img);
            }
        }
        // Never use the GN interstitial/branding og:image as a card thumbnail.
        return None;
    }
    if let Some(img) = fetch_og_image(gn_url, timeout)
        .await
        .filter(|u| has_real_digest_image(u))
    {
        return Some(img);
    }
    None
}

async fn find_publisher_article_url(
    article: &DigestArticle,
    settings: &Settings,
    rt: &Runtime,
) -> Option<String> {
    let host = url::Url::parse(article.publisher_url.trim())
        .ok()
        .and_then(|u| {
            u.host_str()
                .map(|h| h.trim_start_matches("www.").to_string())
        })?;
    let headline = headline_for_search(&article.title);
    if headline.chars().count() < 6 {
        return None;
    }
    let q = format!("{headline} site:{host}");
    let mut params = SearchParams::new(&q);
    params.ai_answer = Some(false);
    let response = search_all(&params, settings, rt).await;
    pick_publisher_hit(response, &host, &headline)
}

fn pick_publisher_hit(
    response: crate::search::SearchResponse,
    host: &str,
    headline: &str,
) -> Option<String> {
    let tokens: Vec<String> = headline
        .split_whitespace()
        .filter(|w| w.chars().count() >= 2)
        .map(|w| w.to_lowercase())
        .collect();
    let mut best: Option<(i32, String)> = None;
    for r in response.results {
        if crate::googlenews_decode::is_google_news_article_url(&r.url) {
            continue;
        }
        if !(r.url.contains(host) || r.parsed_url.get(1).is_some_and(|h| h.contains(host))) {
            continue;
        }
        let hay = format!("{} {}", r.title, r.url).to_lowercase();
        let score = tokens.iter().filter(|t| hay.contains(t.as_str())).count() as i32;
        if score == 0 && tokens.len() > 2 {
            continue;
        }
        if best.as_ref().is_none_or(|(s, _)| score > *s) {
            best = Some((score, r.url));
        }
    }
    best.map(|(_, u)| u)
}

fn headline_for_search(title: &str) -> String {
    let mut t = title.trim().to_string();
    for sep in [" - ", " | ", " · "] {
        if let Some(idx) = t.rfind(sep) {
            let suffix = t[idx + sep.len()..].trim();
            if suffix.chars().count() < 48 {
                t = t[..idx].trim().to_string();
                break;
            }
        }
    }
    t
}

fn digest_from_result(r: &SearchResult) -> DigestArticle {
    DigestArticle {
        title: r.title.clone(),
        url: r.url.clone(),
        engine: r.engine.clone(),
        source: result_host(r),
        teaser: teaser_from_snippet(r),
        image_url_large: large_image(r),
        publisher_url: r.publisher_url.clone(),
        published_date: r.published_date.clone().unwrap_or_default(),
        favicon_url: String::new(),
        category: if r.category.is_empty() { None } else { Some(r.category.clone()) },
    }
}

/// Strip HTML tags and Google News junk from content.
fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        if c == '<' { in_tag = true; continue; }
        if c == '>' { in_tag = false; continue; }
        if !in_tag { out.push(c); }
    }
    // Remove Google News URL artifacts like 'a href="https://news.google...'
    if let Some(idx) = out.find("a href=\"https://news.google") {
        out.truncate(idx);
    }
    out.trim().to_string()
}

/// First 1–2 sentences from the RSS/search snippet (max ~220 chars).
pub fn teaser_from_snippet(r: &SearchResult) -> String {
    let mut c = strip_html(r.content.trim()).trim().to_string();
    if c.is_empty() || is_publisher_only_snippet(&c, &r.title) {
        return headline_teaser(&r.title, &c, 220);
    }
    // Drop common "Publisher · " prefixes.
    if let Some(idx) = c.find('·') {
        let prefix = c[..idx].trim();
        if prefix.chars().count() <= 48 {
            c = c[idx + '·'.len_utf8()..].trim().to_string();
        }
    }
    if c.is_empty() || is_publisher_only_snippet(&c, &r.title) {
        return headline_teaser(&r.title, "", 220);
    }
    two_sentences(&c, 220)
}

/// Google News stores only the outlet name in `content`; detect that case.
fn is_publisher_only_snippet(content: &str, title: &str) -> bool {
    let c = content.trim();
    if c.is_empty() {
        return true;
    }
    if title.ends_with(&format!(" - {c}"))
        || title.ends_with(&format!(" | {c}"))
        || title.ends_with(&format!(" · {c}"))
    {
        return true;
    }
    // Short label with no sentence punctuation → publisher name, not a teaser.
    c.chars().count() < 48 && !c.contains('.') && !c.contains('!') && !c.contains('?')
}

/// Derive a card teaser from the headline, stripping a trailing outlet suffix.
fn headline_teaser(title: &str, publisher_hint: &str, max_chars: usize) -> String {
    let mut t = title.trim().to_string();
    for sep in [" - ", " | ", " · "] {
        if let Some(idx) = t.rfind(sep) {
            let suffix = t[idx + sep.len()..].trim();
            if suffix.eq_ignore_ascii_case(publisher_hint)
                || (publisher_hint.is_empty() && suffix.chars().count() < 48)
            {
                t = t[..idx].trim().to_string();
                break;
            }
        }
    }
    if t.is_empty() {
        return title.trim().to_string();
    }
    two_sentences(&t, max_chars)
}

fn two_sentences(text: &str, max_chars: usize) -> String {
    let t = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut end = 0;
    let mut sentences = 0;
    for (i, ch) in t.char_indices() {
        if ch == '.' || ch == '!' || ch == '?' {
            sentences += 1;
            end = i + ch.len_utf8();
            if sentences >= 2 {
                break;
            }
        }
    }
    let truncated = if end > 0 && sentences >= 1 {
        t[..end].trim().to_string()
    } else {
        t.clone()
    };
    if truncated.chars().count() <= max_chars {
        truncated
    } else {
        truncated.chars().take(max_chars).collect::<String>() + "…"
    }
}

fn large_image(r: &SearchResult) -> String {
    for u in [&r.img_src, &r.thumbnail] {
        let u = u.trim();
        if u.is_empty() || is_generic_google_placeholder(u) {
            continue;
        }
        // Accept any non-small image
        let quality = thumbnail_quality(u);
        if quality != ThumbnailQuality::Small || is_large_thumbnail(u) {
            return u.to_string();
        }
    }
    String::new()
}

// ---------------------------------------------------------------------------
// News selection & ranking (pure, no-AI, no-network — fully unit-tested).
// ---------------------------------------------------------------------------

/// Additive score tier that floats any genuinely-recent item above every
/// not-known-recent item, used only on the rare fallback path so "recent first"
/// is guaranteed when we backfill a too-quiet query with older items.
const RECENT_TIER_BONUS: f64 = 1e9;

/// Curate the aggregated news results into a diverse, fresh, de-duplicated feed
/// of at most `limit` items, applying a HARD recency cutoff.
///
/// Pipeline (all signals are pure functions of the result + the wall clock):
/// 1. drop low-quality items ([`is_quality_result`]);
/// 2. compute a composite score = `base × freshness × relevance × image-boost`;
/// 3. **hard recency cutoff**: keep only items with a *trusted* publish
///    timestamp within `max_age_days`. Items with no date, or only an
///    unreliable last-edit proxy, are treated as not-known-recent and excluded;
/// 4. rank + collapse near-duplicate stories + enforce per-source diversity;
/// 5. **fallback**: if fewer than `min_results` recent items survive, relax and
///    backfill with the best-available older/undated items — recent always
///    first — so a quiet query never yields a near-empty feed.
///
/// When `max_age_days == 0` the hard cutoff is disabled (soft freshness ranking
/// only), preserving the previous behaviour.
///
/// `now` is the current unix time in seconds (injected for deterministic tests).
pub(crate) fn curate_news(
    results: Vec<SearchResult>,
    query: &str,
    cfg: &NewsSettings,
    now: i64,
    limit: usize,
) -> Vec<SearchResult> {
    let cutoff = (cfg.max_age_days > 0).then(|| (cfg.max_age_days as i64).saturating_mul(86_400));
    curate_news_with_cutoff(results, query, cfg, now, limit, cutoff, true)
}

fn curate_news_strict_recent(
    results: Vec<SearchResult>,
    query: &str,
    cfg: &NewsSettings,
    now: i64,
    limit: usize,
    max_age_secs: i64,
) -> Vec<SearchResult> {
    curate_news_with_cutoff(
        results,
        query,
        cfg,
        now,
        limit,
        Some(max_age_secs.max(1)),
        false,
    )
}

fn curate_news_with_cutoff(
    results: Vec<SearchResult>,
    query: &str,
    cfg: &NewsSettings,
    now: i64,
    limit: usize,
    cutoff_secs: Option<i64>,
    allow_stale_backfill: bool,
) -> Vec<SearchResult> {
    let terms = query_terms(query);
    let half_life = cfg.freshness_half_life_hours.max(0.1);
    let fw = cfg.freshness_weight.clamp(0.0, 1.0);
    let cutoff_enabled = cutoff_secs.is_some();
    let max_age_secs = cutoff_secs.unwrap_or(0);

    // Score every quality item once, tagging whether it is confidently recent.
    let scored: Vec<(bool, f64, SearchResult)> = results
        .into_iter()
        .filter(is_quality_result)
        .map(|r| {
            let ts = trusted_publish_time(&r);
            let fresh = freshness_factor(ts, now, half_life);
            // Blend freshness with the base score per `freshness_weight`: at
            // w=0 it's a no-op (×1), at w=1 freshness fully scales the score.
            let fresh_mult = (1.0 - fw) + fw * fresh;
            let rel = relevance_factor(&terms, &r.title, &r.content);
            let img = if has_usable_image(&r) {
                IMAGE_BOOST
            } else {
                NO_IMAGE_PENALTY
            };
            // Prefer our own index over external aggregators.
            let own_index_boost = if r.engine == "orgos_news" {
                ORGOS_NEWS_BOOST
            } else if r.engine == "local_feeds" || r.engine == "local_news" {
                LOCAL_FEEDS_BOOST
            } else {
                1.0
            };
            // Guard against a zero base score (single-engine, position 1 still
            // yields a positive score, but be defensive) so the multipliers
            // still order items relative to each other.
            let base = if r.score > 0.0 { r.score } else { 1e-4 };
            let recent = is_recent(ts, now, max_age_secs);
            (recent, base * fresh_mult * rel * img * own_index_boost, r)
        })
        .collect();

    // Cutoff disabled → rank everything together (legacy soft-freshness path).
    if !cutoff_enabled {
        let all: Vec<(f64, SearchResult)> = scored.into_iter().map(|(_, s, r)| (s, r)).collect();
        return rank_dedup_diversify(all, cfg, limit)
            .into_iter()
            .take(limit)
            .collect();
    }

    // Primary feed: only confidently-recent items.
    let recent_input: Vec<(f64, SearchResult)> = scored
        .iter()
        .filter(|(recent, _, _)| *recent)
        .map(|(_, s, r)| (*s, r.clone()))
        .collect();
    let ranked_recent = rank_dedup_diversify(recent_input, cfg, limit);

    if ranked_recent.len() >= cfg.min_results || !allow_stale_backfill {
        // Enough recent news — show ONLY recent items (no old news), even if
        // that means fewer than `limit`.
        return ranked_recent.into_iter().take(limit).collect();
    }

    // Fallback: too few recent items would leave a near-empty feed. Backfill
    // with the best-available older/undated items, but keep recent ones first
    // via a large additive tier bonus so they always sort ahead.
    let all_input: Vec<(f64, SearchResult)> = scored
        .into_iter()
        .map(|(recent, s, r)| (if recent { s + RECENT_TIER_BONUS } else { s }, r))
        .collect();
    rank_dedup_diversify(all_input, cfg, limit)
        .into_iter()
        .take(limit)
        .collect()
}

/// Rank a scored list (descending), collapse cross-outlet near-duplicate
/// stories, then enforce per-source diversity. Shared by the recent-only and
/// fallback passes of [`curate_news`].
fn rank_dedup_diversify(
    mut scored: Vec<(f64, SearchResult)>,
    cfg: &NewsSettings,
    limit: usize,
) -> Vec<SearchResult> {
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    // Near-duplicate clustering: keep the first (highest-scored) representative
    // of each story; later items whose title tokens overlap enough are dropped,
    // donating their image to the representative if it lacks one.
    let thr = cfg.dedup_title_similarity.clamp(0.0, 1.0);
    let mut kept_tokens: Vec<(BTreeSet<String>, usize)> = Vec::new();
    let mut deduped: Vec<SearchResult> = Vec::new();
    for (_, r) in scored {
        let toks = title_tokens(&r.title);
        let dup_idx = kept_tokens
            .iter()
            .find(|(t, _)| !toks.is_empty() && title_similarity(&toks, t) >= thr)
            .map(|(_, i)| *i);
        if let Some(idx) = dup_idx {
            if !has_usable_image(&deduped[idx]) && has_usable_image(&r) {
                deduped[idx].img_src = r.img_src.clone();
                deduped[idx].thumbnail = r.thumbnail.clone();
            }
            continue;
        }
        kept_tokens.push((toks, deduped.len()));
        deduped.push(r);
    }

    cap_per_source(deduped, cfg.per_source_cap, limit)
}

/// Whether an item is confidently recent: it has a trusted publish timestamp no
/// older than `max_age_secs`. A future timestamp (clock/timezone skew) counts
/// as recent; a missing/untrusted timestamp never does.
fn is_recent(ts: Option<i64>, now: i64, max_age_secs: i64) -> bool {
    match ts {
        Some(t) => now - t <= max_age_secs,
        None => false,
    }
}

/// A *trusted* publish timestamp (unix seconds) for recency decisions, or `None`
/// when the item has no reliable publication date. Wikinews exposes only a
/// last-edit time (not a publish date), so its items are intentionally treated
/// as undated here — see `engines::wikinews`.
fn trusted_publish_time(r: &SearchResult) -> Option<i64> {
    parse_publish_time(r.published_date.as_deref())
}

/// Enforce per-source diversity: at most `cap` items per source key, preserving
/// the (already score-sorted) order. If honoring the cap would leave fewer than
/// `want` items, the over-cap remainder is appended (in score order) so the feed
/// is still filled — diversity is a strong preference, not a hard truncation.
fn cap_per_source(items: Vec<SearchResult>, cap: usize, want: usize) -> Vec<SearchResult> {
    if cap == 0 {
        return items;
    }
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut primary: Vec<SearchResult> = Vec::new();
    let mut overflow: Vec<SearchResult> = Vec::new();
    for r in items {
        let key = source_key(&r);
        let c = counts.entry(key).or_insert(0);
        if *c < cap {
            *c += 1;
            primary.push(r);
        } else {
            overflow.push(r);
        }
    }
    if primary.len() < want {
        let need = want - primary.len();
        primary.extend(overflow.into_iter().take(need));
    }
    primary
}

/// A result is feed-worthy if it has a non-trivial title and a usable http(s)
/// link. Deliberately permissive: we down-rank, not drop, on softer signals.
fn is_quality_result(r: &SearchResult) -> bool {
    let title = r.title.trim();
    if title.chars().count() < 3 || !title.chars().any(|c| c.is_alphanumeric()) {
        return false;
    }
    let url = r.url.trim();
    url.starts_with("http://") || url.starts_with("https://")
}

/// True when the result carries any non-empty image/thumbnail URL.
fn has_usable_image(r: &SearchResult) -> bool {
    !r.img_src.trim().is_empty() || !r.thumbnail.trim().is_empty()
}

/// Recency factor in `(0.0, 1.0]`: `1.0` for a just-published item, decaying by
/// half every `half_life_hours`. Undated items get [`UNDATED_FRESHNESS`].
fn freshness_factor(published: Option<i64>, now: i64, half_life_hours: f64) -> f64 {
    let Some(ts) = published else {
        return UNDATED_FRESHNESS;
    };
    let age_secs = (now - ts).max(0) as f64;
    let age_hours = age_secs / 3600.0;
    0.5f64.powf(age_hours / half_life_hours)
}

/// Topical-relevance factor in `[0.05, 1.0]`: the fraction of seed query terms
/// present in the title/snippet. A total miss gets 0.05 (effectively filtered out).
fn relevance_factor(terms: &[String], title: &str, content: &str) -> f64 {
    if terms.is_empty() {
        return 1.0;
    }
    let hay = format!("{title} {content}").to_lowercase();
    let matched = terms.iter().filter(|t| hay.contains(t.as_str())).count();
    if matched == 0 {
        return 0.05; // Effectively filter out completely off-topic results
    }

    // Filter out e-sports from sports category
    let is_sports = terms.iter().any(|t| t == "스포츠" || t == "축구" || t == "sports");
    let is_esports = hay.contains("e스포츠") || hay.contains("esports") || hay.contains("lol")
        || hay.contains("리그오브레전드") || hay.contains("게임대회") || hay.contains("선수모집");
    if is_sports && is_esports {
        return 0.05;
    }

    // Filter out promotional/advertising content and opinion pieces
    let title_lower = title.to_lowercase();
    let is_promo = title_lower.contains("top 3") || title_lower.contains("top 5")
        || title_lower.contains("top 10") || title_lower.contains("top 20")
        || title_lower.contains("프로모션") || title_lower.contains("콘테스트")
        || title_lower.contains("수상작") || title_lower.contains("이벤트 안내")
        || title_lower.contains("할인") || title_lower.contains("무료 제공")
        || title_lower.contains("예약 top") || title_lower.contains("추천 top")
        || title_lower.contains("[광고]") || title_lower.contains("[ad]")
        || title_lower.contains("sponsored");
    if is_promo {
        return 0.05;
    }

    let frac = matched as f64 / terms.len() as f64;
    0.5 + 0.5 * frac
}

/// Significant lowercase query terms (≥3 chars, alphanumeric-trimmed), matching
/// the convention used by the base-search annotator.
fn query_terms(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(|t| {
            t.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|t| t.chars().count() >= 3)
        .collect()
}

/// Common headline words ignored when comparing titles for near-duplicate
/// clustering, so two stories aren't merged just because both say "the news".
const TITLE_STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "from", "that", "this", "are", "was", "has", "have", "will",
    "after", "over", "into", "out", "new", "news", "say", "says", "amid",
];

/// Tokenize a title into a set of significant lowercase words for similarity.
fn title_tokens(title: &str) -> BTreeSet<String> {
    title
        .split_whitespace()
        .map(|t| {
            t.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|t| t.chars().count() >= 3 && !TITLE_STOPWORDS.contains(&t.as_str()))
        .collect()
}

/// Jaccard similarity of two title-token sets in `[0.0, 1.0]`.
fn title_similarity(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    inter / union
}

/// The host of a result, lowercased with a leading `www.` stripped. Falls back
/// to re-parsing the URL when the aggregator didn't fill `parsed_url`.
fn result_host(r: &SearchResult) -> String {
    let netloc = r.parsed_url[1].split(':').next().unwrap_or("");
    let host = if netloc.is_empty() {
        url::Url::parse(&r.url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_default()
    } else {
        netloc.to_string()
    };
    host.trim_start_matches("www.").to_lowercase()
}

/// The diversity key for a result: normally its host, but for aggregator
/// redirect hosts (Google News) the underlying publisher parsed from the title
/// suffix (`"… - CBS News"`), so a single aggregator can't crowd out the feed.
fn source_key(r: &SearchResult) -> String {
    let host = result_host(r);
    if AGGREGATOR_HOSTS.contains(&host.as_str()) {
        if let Some(publisher) = publisher_from_title(&r.title) {
            return publisher;
        }
    }
    host
}

/// Extract a publisher name from a Google-News-style `"Headline - Publisher"`
/// title suffix. Returns a normalized lowercase key, or `None` when there's no
/// plausible trailing source.
fn publisher_from_title(title: &str) -> Option<String> {
    let (_, tail) = title.rsplit_once(" - ")?;
    let tail = tail.trim();
    // A publisher suffix is short and not a sentence; reject long tails (those
    // are almost certainly part of the headline, e.g. an em-dash aside).
    if tail.is_empty() || tail.chars().count() > 40 {
        return None;
    }
    Some(tail.to_lowercase())
}

/// Current unix time in seconds (0 on the practically-impossible clock error).
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn rfc3339_utc(ts: i64) -> String {
    if ts <= 0 {
        return String::new();
    }
    let days = ts.div_euclid(86_400);
    let secs = ts.rem_euclid(86_400);
    let (y, mo, d) = civil_from_days(days);
    let hh = secs / 3600;
    let mm = (secs % 3600) / 60;
    let ss = secs % 60;
    format!("{y:04}-{mo:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Parse a publish timestamp from the formats the news engines emit, returning
/// unix seconds. Handles RFC822/1123 (`Mon, 01 Jun 2026 08:30:00 GMT` — Google
/// News), RFC3339/ISO8601 (`2026-06-01T08:30:00Z` — Hacker News / Lemmy) and
/// GDELT's compact `20260601T083000Z`. Returns `None` on anything unrecognized.
fn parse_publish_time(s: Option<&str>) -> Option<i64> {
    let s = s?.trim();
    if s.is_empty() {
        return None;
    }
    parse_rfc3339(s)
        .or_else(|| parse_compact(s))
        .or_else(|| parse_rfc822(s))
}

/// Days from the unix epoch (1970-01-01) to `y-m-d` (proleptic Gregorian),
/// via Howard Hinnant's `days_from_civil` algorithm.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let y = y + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}

/// Assemble a UTC unix timestamp from broken-down calendar fields.
fn to_unix_utc(y: i64, mo: i64, d: i64, hh: i64, mm: i64, ss: i64) -> i64 {
    days_from_civil(y, mo, d) * 86_400 + hh * 3600 + mm * 60 + ss
}

/// Split an RFC3339 time portion into `(time_without_tz, tz_offset_seconds)`.
fn split_tz(rest: &str) -> (&str, i64) {
    if let Some(t) = rest.strip_suffix('Z').or_else(|| rest.strip_suffix('z')) {
        return (t, 0);
    }
    if let Some(pos) = rest.rfind(['+', '-']) {
        let (t, tz) = rest.split_at(pos);
        let sign = if tz.starts_with('-') { -1 } else { 1 };
        let digits: String = tz[1..].chars().filter(|c| c.is_ascii_digit()).collect();
        let h: i64 = digits.get(0..2).and_then(|x| x.parse().ok()).unwrap_or(0);
        let m: i64 = digits.get(2..4).and_then(|x| x.parse().ok()).unwrap_or(0);
        return (t, sign * (h * 3600 + m * 60));
    }
    (rest, 0)
}

fn parse_rfc3339(s: &str) -> Option<i64> {
    let (date, rest) = s.split_once(['T', ' '])?;
    let mut dp = date.split('-');
    let y: i64 = dp.next()?.parse().ok()?;
    let mo: i64 = dp.next()?.parse().ok()?;
    let d: i64 = dp.next()?.parse().ok()?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let (time, tz) = split_tz(rest);
    let mut tp = time.split(':');
    let hh: i64 = tp.next()?.parse().ok()?;
    let mm: i64 = tp.next().unwrap_or("0").parse().ok()?;
    let ss: i64 = tp
        .next()
        .unwrap_or("0")
        .split('.')
        .next()
        .unwrap_or("0")
        .parse()
        .ok()?;
    Some(to_unix_utc(y, mo, d, hh, mm, ss) - tz)
}

/// Parse GDELT's compact `YYYYMMDDThhmmss[Z]` form.
fn parse_compact(s: &str) -> Option<i64> {
    let s = s.trim_end_matches(['Z', 'z']);
    let (d, t) = s.split_once('T')?;
    if d.len() != 8 || t.len() < 6 || !d.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let y: i64 = d.get(0..4)?.parse().ok()?;
    let mo: i64 = d.get(4..6)?.parse().ok()?;
    let dd: i64 = d.get(6..8)?.parse().ok()?;
    let hh: i64 = t.get(0..2)?.parse().ok()?;
    let mm: i64 = t.get(2..4)?.parse().ok()?;
    let ss: i64 = t.get(4..6)?.parse().ok()?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&dd) {
        return None;
    }
    Some(to_unix_utc(y, mo, dd, hh, mm, ss))
}

fn month_num(m: &str) -> Option<i64> {
    Some(match m.get(0..3)?.to_ascii_lowercase().as_str() {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return None,
    })
}

/// Parse the offset of an RFC822 timezone token (`GMT`/`UTC`/`Z` → 0, numeric
/// `+0900`/`-0500` → seconds). Named US zones are not in feeds we consume.
fn rfc822_tz(tok: &str) -> i64 {
    match tok.to_ascii_uppercase().as_str() {
        "GMT" | "UTC" | "UT" | "Z" => 0,
        other => {
            let sign = if other.starts_with('-') { -1 } else { 1 };
            let digits: String = other.chars().filter(|c| c.is_ascii_digit()).collect();
            let h: i64 = digits.get(0..2).and_then(|x| x.parse().ok()).unwrap_or(0);
            let m: i64 = digits.get(2..4).and_then(|x| x.parse().ok()).unwrap_or(0);
            sign * (h * 3600 + m * 60)
        }
    }
}

fn parse_rfc822(s: &str) -> Option<i64> {
    // Drop an optional leading weekday ("Mon, ").
    let s = s.split_once(", ").map(|(_, rest)| rest).unwrap_or(s);
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() < 4 {
        return None;
    }
    let d: i64 = parts[0].parse().ok()?;
    let mo = month_num(parts[1])?;
    let mut y: i64 = parts[2].parse().ok()?;
    if y < 100 {
        y += if y < 70 { 2000 } else { 1900 };
    }
    let mut tp = parts[3].split(':');
    let hh: i64 = tp.next()?.parse().ok()?;
    let mm: i64 = tp.next().unwrap_or("0").parse().ok()?;
    let ss: i64 = tp.next().unwrap_or("0").parse().ok()?;
    let tz = parts.get(4).map(|t| rfc822_tz(t)).unwrap_or(0);
    Some(to_unix_utc(y, mo, d, hh, mm, ss) - tz)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_image_rejects_favicon() {
        let r = SearchResult {
            title: "T".into(),
            url: "https://example.com/a".into(),
            content: "Snippet.".into(),
            engine: "news".into(),
            template: String::new(),
            img_src: "https://icons.duckduckgo.com/ip3/foo.com.ico".into(),
            thumbnail: String::new(),
            ..Default::default()
        };
        assert!(large_image(&r).is_empty());
    }

    #[test]
    fn teaser_limits_to_two_sentences() {
        let r = SearchResult {
            title: "T".into(),
            url: "https://example.com/a".into(),
            content: "First sentence here. Second sentence follows. Third should be dropped."
                .into(),
            engine: "news".into(),
            ..Default::default()
        };
        let t = teaser_from_snippet(&r);
        assert!(t.contains("First"));
        assert!(t.contains("Second"));
        assert!(!t.contains("Third"));
    }

    #[test]
    fn teaser_from_publisher_only_content_uses_headline() {
        let r = SearchResult {
            title: "China celebrates National Science Day - China Daily".into(),
            url: "https://news.google.com/rss/articles/abc".into(),
            content: "China Daily".into(),
            engine: "googlenews".into(),
            ..Default::default()
        };
        let t = teaser_from_snippet(&r);
        assert!(t.contains("China celebrates"));
        assert!(!t.contains("China Daily"));
    }

    // --- selection / ranking helpers --------------------------------------

    /// Build a news result. `host` becomes both the URL host and `parsed_url[1]`.
    fn news(title: &str, host: &str, score: f64, published: Option<&str>) -> SearchResult {
        let url = format!("https://{host}/{}", title.replace(' ', "-").to_lowercase());
        let mut r = SearchResult {
            title: title.into(),
            url,
            content: String::new(),
            engine: "news".into(),
            template: "default.html".into(),
            score,
            published_date: published.map(String::from),
            ..Default::default()
        };
        r.parsed_url[1] = host.into();
        r
    }

    fn cfg() -> NewsSettings {
        NewsSettings::default()
    }

    #[test]
    fn parses_rfc822_google_news_date() {
        // Mon, 01 Jun 2026 00:00:00 GMT == 2026-06-01T00:00:00Z.
        let expect = to_unix_utc(2026, 6, 1, 0, 0, 0);
        assert_eq!(
            parse_publish_time(Some("Mon, 01 Jun 2026 00:00:00 GMT")),
            Some(expect)
        );
        // A +0900 offset shifts the UTC instant back nine hours.
        assert_eq!(
            parse_publish_time(Some("Mon, 01 Jun 2026 09:00:00 +0900")),
            Some(expect)
        );
    }

    #[test]
    fn parses_rfc3339_and_compact_dates() {
        let expect = to_unix_utc(2026, 6, 1, 8, 30, 0);
        assert_eq!(
            parse_publish_time(Some("2026-06-01T08:30:00Z")),
            Some(expect)
        );
        assert_eq!(
            parse_publish_time(Some("2026-06-01T08:30:00.123Z")),
            Some(expect)
        );
        // GDELT compact form.
        assert_eq!(parse_publish_time(Some("20260601T083000Z")), Some(expect));
        // +09:00 offset.
        assert_eq!(
            parse_publish_time(Some("2026-06-01T17:30:00+09:00")),
            Some(expect)
        );
        // Garbage / empty → None (graceful: undated).
        assert_eq!(parse_publish_time(Some("not a date")), None);
        assert_eq!(parse_publish_time(Some("")), None);
        assert_eq!(parse_publish_time(None), None);
    }

    #[test]
    fn freshness_decays_with_age_and_handles_undated() {
        let now = to_unix_utc(2026, 6, 2, 0, 0, 0);
        let fresh = freshness_factor(Some(now), now, 18.0);
        let half = freshness_factor(Some(now - 18 * 3600), now, 18.0);
        let old = freshness_factor(Some(now - 72 * 3600), now, 18.0);
        assert!((fresh - 1.0).abs() < 1e-9);
        assert!((half - 0.5).abs() < 1e-9);
        assert!(old < half);
        // Undated gets the neutral midpoint, never zero.
        assert_eq!(freshness_factor(None, now, 18.0), UNDATED_FRESHNESS);
    }

    #[test]
    fn fresher_item_outranks_stale_one_with_equal_base() {
        let now = to_unix_utc(2026, 6, 2, 0, 0, 0);
        let fresh = news(
            "Alpha story breaks today",
            "a.example",
            1.0,
            Some("2026-06-02T00:00:00Z"),
        );
        let stale = news(
            "Beta story from last week",
            "b.example",
            1.0,
            Some("2026-05-26T00:00:00Z"),
        );
        let out = curate_news(vec![stale, fresh], "", &cfg(), now, 5);
        assert_eq!(out[0].parsed_url[1], "a.example");
    }

    #[test]
    fn per_source_cap_limits_one_outlet() {
        let now = to_unix_utc(2026, 6, 2, 0, 0, 0);
        // Five *distinct* stories all from one host (distinct so near-dup
        // clustering doesn't merge them first), plus one from another outlet.
        let headlines = [
            "Senate passes sweeping budget reform bill",
            "Hurricane warning issued for southern coast",
            "Central bank holds interest rates steady",
            "Olympic committee unveils host city shortlist",
            "Tech firm recalls flagship laptop battery",
        ];
        let mut input: Vec<SearchResult> = headlines
            .iter()
            .enumerate()
            .map(|(i, h)| {
                news(
                    h,
                    "dominant.example",
                    5.0 - i as f64,
                    Some("2026-06-02T00:00:00Z"),
                )
            })
            .collect();
        // Two more distinct outlets so the feed can fill without relaxing the
        // cap (the relax fallback is exercised separately).
        input.push(news(
            "A separate publisher weighs in on policy",
            "other-one.example",
            0.2,
            Some("2026-06-02T00:00:00Z"),
        ));
        input.push(news(
            "A third outlet covers the local election",
            "other-two.example",
            0.1,
            Some("2026-06-02T00:00:00Z"),
        ));
        let out = curate_news(input, "", &cfg(), now, 4);
        let from_dominant = out
            .iter()
            .filter(|r| r.parsed_url[1] == "dominant.example")
            .count();
        // Cap is 2: the dominant outlet contributes at most two of the four.
        assert_eq!(from_dominant, 2);
        assert!(out.iter().any(|r| r.parsed_url[1] == "other-one.example"));
    }

    #[test]
    fn cap_relaxes_when_needed_to_fill_feed() {
        let now = to_unix_utc(2026, 6, 2, 0, 0, 0);
        // Only one source available, want 3 — cap must relax to fill. Distinct
        // titles so dedup keeps them separate.
        let headlines = [
            "Wildfire spreads across northern hills",
            "New subway line opens downtown today",
            "Researchers map deepest ocean trench",
            "Festival draws record crowds this weekend",
        ];
        let input: Vec<SearchResult> = headlines
            .iter()
            .enumerate()
            .map(|(i, h)| news(h, "solo.example", 4.0 - i as f64, None))
            .collect();
        let out = curate_news(input, "", &cfg(), now, 3);
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn near_duplicate_stories_collapse() {
        let now = to_unix_utc(2026, 6, 2, 0, 0, 0);
        let a = news(
            "Massive earthquake strikes coastal region overnight",
            "outlet-a.example",
            2.0,
            Some("2026-06-02T00:00:00Z"),
        );
        let b = news(
            "Massive earthquake strikes coastal region, dozens missing",
            "outlet-b.example",
            1.0,
            Some("2026-06-02T00:00:00Z"),
        );
        let c = news(
            "Stock markets rally on rate-cut hopes",
            "outlet-c.example",
            0.5,
            Some("2026-06-02T00:00:00Z"),
        );
        let out = curate_news(vec![a, b, c], "", &cfg(), now, 5);
        // The two earthquake stories collapse to one; the markets story stays.
        assert_eq!(out.len(), 2);
        assert!(out.iter().any(|r| r.title.contains("earthquake")));
        assert!(out.iter().any(|r| r.title.contains("markets")));
    }

    #[test]
    fn duplicate_donates_image_to_representative() {
        let now = to_unix_utc(2026, 6, 2, 0, 0, 0);
        let rep = news("Election results announced tonight", "a.example", 2.0, None);
        let mut dup = news(
            "Election results announced tonight, official",
            "b.example",
            1.0,
            None,
        );
        dup.img_src = "https://cdn.example.com/pic-1200x630.jpg".into();
        // Representative has no image; the lower-scored duplicate does.
        assert!(rep.img_src.is_empty());
        let out = curate_news(vec![rep, dup], "", &cfg(), now, 5);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].img_src, "https://cdn.example.com/pic-1200x630.jpg");
    }

    #[test]
    fn quality_filter_drops_empty_and_non_http() {
        let now = to_unix_utc(2026, 6, 2, 0, 0, 0);
        let good = news("Real headline here", "a.example", 1.0, None);
        let mut empty = news("x", "b.example", 5.0, None); // too-short title
        empty.title = "  ".into();
        let mut ftp = news("Has title but bad scheme", "c.example", 5.0, None);
        ftp.url = "ftp://c.example/file".into();
        let out = curate_news(vec![good, empty, ftp], "", &cfg(), now, 5);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "Real headline here");
    }

    #[test]
    fn relevance_downranks_offtopic_only() {
        let terms = query_terms("quantum computing breakthrough");
        let on = relevance_factor(&terms, "Quantum computing breakthrough announced", "");
        let off = relevance_factor(&terms, "Local bakery wins award", "");
        assert!(on > off);
        assert!((on - 1.0).abs() < 1e-9);
        // Empty query → neutral, never penalizing.
        assert_eq!(relevance_factor(&[], "anything", ""), 1.0);
    }

    #[test]
    fn aggregator_source_key_uses_publisher() {
        let mut g1 = news("Story one - CBS News", "news.google.com", 1.0, None);
        let mut g2 = news("Story two - CBS News", "news.google.com", 1.0, None);
        let mut g3 = news("Story three - Reuters", "news.google.com", 1.0, None);
        g1.parsed_url[1] = "news.google.com".into();
        g2.parsed_url[1] = "news.google.com".into();
        g3.parsed_url[1] = "news.google.com".into();
        assert_eq!(source_key(&g1), "cbs news");
        assert_eq!(source_key(&g2), "cbs news");
        assert_eq!(source_key(&g3), "reuters");
        // A plain host result keys on its host.
        let plain = news("Plain story", "bbc.com", 1.0, None);
        assert_eq!(source_key(&plain), "bbc.com");
    }

    #[test]
    fn google_news_diversity_by_publisher() {
        let now = to_unix_utc(2026, 6, 2, 0, 0, 0);
        // Three *distinct* CBS stories + one Reuters, all via the Google News
        // redirect host (distinct titles so dedup doesn't merge them first).
        let date = Some("2026-06-02T00:00:00Z");
        let mut input = vec![
            news(
                "Senate passes budget overhaul - CBS News",
                "news.google.com",
                3.0,
                date,
            ),
            news(
                "Hurricane barrels toward gulf coast - CBS News",
                "news.google.com",
                2.5,
                date,
            ),
            news(
                "Airline grounds fleet for inspections - CBS News",
                "news.google.com",
                2.0,
                date,
            ),
            news(
                "Markets climb on strong jobs report - Reuters",
                "news.google.com",
                0.5,
                date,
            ),
        ];
        for r in &mut input {
            r.parsed_url[1] = "news.google.com".into();
        }
        let out = curate_news(input, "", &cfg(), now, 3);
        let cbs = out.iter().filter(|r| source_key(r) == "cbs news").count();
        assert_eq!(cbs, 2, "CBS capped to per_source_cap even via aggregator");
        assert!(out.iter().any(|r| source_key(r) == "reuters"));
    }

    // --- hard recency cutoff ----------------------------------------------

    #[test]
    fn hard_cutoff_drops_old_news() {
        let now = to_unix_utc(2026, 6, 1, 0, 0, 0);
        // A 2012 election story (like the 박근혜 case) must be dropped, even with
        // a high base score, while recent items survive. Three recent items keep
        // us above min_results so the fallback never engages.
        let old = news(
            "Park elected president in 2012",
            "old.example",
            9.0,
            Some("2012-12-19T00:00:00Z"),
        );
        let r1 = news(
            "Recent breaking development today",
            "a.example",
            1.0,
            Some("2026-05-30T00:00:00Z"),
        );
        let r2 = news(
            "Second recent story appears now",
            "b.example",
            0.9,
            Some("2026-05-29T00:00:00Z"),
        );
        let r3 = news(
            "Third recent story published lately",
            "c.example",
            0.8,
            Some("2026-05-28T00:00:00Z"),
        );
        let out = curate_news(vec![old, r1, r2, r3], "", &cfg(), now, 8);
        assert_eq!(out.len(), 3, "only the three recent items remain");
        assert!(
            out.iter().all(|r| !r.title.contains("2012")),
            "the years-old story is dropped by the hard cutoff"
        );
    }

    #[test]
    fn undated_items_excluded_when_recent_exist() {
        let now = to_unix_utc(2026, 6, 1, 0, 0, 0);
        // Undated Wikinews-style item with the highest base score still must not
        // appear while there are enough confidently-recent items. Distinct
        // headlines so near-dup clustering doesn't merge them first.
        let headlines = [
            "Senate passes the new budget today",
            "Hurricane warning issued for the coast",
            "Central bank holds interest rates steady",
        ];
        let mut input: Vec<SearchResult> = headlines
            .iter()
            .enumerate()
            .map(|(i, h)| {
                news(
                    h,
                    &format!("h{i}.example"),
                    1.0,
                    Some("2026-05-30T00:00:00Z"),
                )
            })
            .collect();
        input.push(news("Undated wikinews piece", "en.wikinews.org", 9.0, None));
        let out = curate_news(input, "", &cfg(), now, 8);
        assert_eq!(out.len(), 3);
        assert!(
            out.iter().all(|r| r.title != "Undated wikinews piece"),
            "undated item is excluded when recent items exist"
        );
    }

    #[test]
    fn fallback_backfills_when_too_few_recent() {
        let now = to_unix_utc(2026, 6, 1, 0, 0, 0);
        // Only one recent item (< min_results=3): the feed must still fill from
        // older/undated items, but the recent one must come first.
        let recent = news(
            "The one recent story",
            "recent.example",
            1.0,
            Some("2026-05-31T00:00:00Z"),
        );
        let undated1 = news("Older undated story one", "u1.example", 0.9, None);
        let undated2 = news("Older undated story two", "u2.example", 0.8, None);
        let old = news(
            "Ancient story from 2012",
            "old.example",
            0.7,
            Some("2012-01-01T00:00:00Z"),
        );
        let out = curate_news(vec![undated1, undated2, old, recent], "", &cfg(), now, 8);
        assert!(
            out.len() >= 3,
            "fallback fills the feed instead of near-empty"
        );
        assert_eq!(
            out[0].title, "The one recent story",
            "the recent item always ranks first in the fallback"
        );
    }

    #[test]
    fn cutoff_disabled_keeps_old_items() {
        let now = to_unix_utc(2026, 6, 1, 0, 0, 0);
        let mut c = cfg();
        c.max_age_days = 0; // disable the hard cutoff (legacy soft-freshness)
        let old = news(
            "Ancient story from 2012",
            "old.example",
            5.0,
            Some("2012-01-01T00:00:00Z"),
        );
        let recent = news(
            "A recent story today",
            "r.example",
            1.0,
            Some("2026-05-30T00:00:00Z"),
        );
        let out = curate_news(vec![old, recent], "", &c, now, 8);
        assert_eq!(out.len(), 2, "with the cutoff disabled, old items remain");
    }

    #[test]
    fn is_recent_boundary() {
        let now = to_unix_utc(2026, 6, 1, 0, 0, 0);
        let max = 14 * 86_400;
        assert!(is_recent(Some(now), now, max), "just-published is recent");
        assert!(
            is_recent(Some(now - 13 * 86_400), now, max),
            "13d within 14d"
        );
        assert!(
            !is_recent(Some(now - 20 * 86_400), now, max),
            "20d is too old"
        );
        assert!(
            is_recent(Some(now + 3600), now, max),
            "slight future skew is recent"
        );
        assert!(!is_recent(None, now, max), "undated is never recent");
    }

    // --- digest cache ------------------------------------------------------

    fn digest(query: &str, n: usize) -> NewsDigestResponse {
        NewsDigestResponse {
            query: query.into(),
            articles: (0..n)
                .map(|i| DigestArticle {
                    title: format!("t{i}"),
                    url: format!("https://example.com/{i}"),
                    engine: "news".into(),
                    teaser: String::new(),
                    image_url_large: String::new(),
                    publisher_url: String::new(),
                    published_date: String::new(),
                    favicon_url: String::new(),
                    category: None,
                })
                .collect(),
        }
    }

    #[test]
    fn cache_hit_and_miss() {
        let c = DigestCache::new(180);
        assert!(c.enabled());
        assert!(c.get("k").is_none(), "cold lookup misses");
        c.put("k".into(), digest("hello", 3));
        let hit = c.get("k").expect("warm lookup hits");
        assert_eq!(hit.query, "hello");
        assert_eq!(hit.articles.len(), 3);
        // A different key still misses (no cross-query bleed).
        assert!(c.get("other").is_none());
    }

    #[test]
    fn cache_disabled_when_ttl_zero() {
        let c = DigestCache::new(0);
        assert!(!c.enabled());
        c.put("k".into(), digest("x", 1));
        assert!(c.get("k").is_none());
    }

    #[test]
    fn cache_entry_expires() {
        // 0-second TTL means any stored entry is immediately stale; a tiny TTL
        // proves expiry without sleeping. Use 1ns via a from-millis-rounded TTL
        // isn't possible (secs granularity), so simulate by constructing with a
        // sub-second elapsed check: build with TTL 1s and a hand-aged entry.
        let c = DigestCache::new(1);
        {
            let mut map = c.map.lock().unwrap();
            let aged = std::time::Instant::now() - Duration::from_secs(5);
            map.insert("k".into(), (aged, digest("stale", 1)));
        }
        // The 5s-old entry is older than the 1s TTL → evicted on lookup.
        assert!(c.get("k").is_none());
        assert!(
            c.map.lock().unwrap().get("k").is_none(),
            "stale entry is evicted, not just hidden"
        );
    }

    #[test]
    fn cache_bounds_size() {
        let c = DigestCache::new(180);
        for i in 0..(DIGEST_CACHE_MAX_ENTRIES + 50) {
            c.put(format!("k{i}"), digest("q", 1));
        }
        let len = c.map.lock().unwrap().len();
        assert!(
            len <= DIGEST_CACHE_MAX_ENTRIES,
            "cache stays within the size bound (got {len})"
        );
    }

    #[test]
    fn cache_clear_drops_entries() {
        let c = DigestCache::new(180);
        c.put("k".into(), digest("hello", 3));
        assert!(c.get("k").is_some());
        c.clear();
        assert!(c.get("k").is_none());
    }

    #[test]
    fn cache_key_separates_query_limit_locale() {
        let a = digest_cache_key("world news", 8, "auto");
        let b = digest_cache_key("world news", 5, "auto");
        let d = digest_cache_key("world news", 8, "ko-KR");
        let e = digest_cache_key("other query", 8, "auto");
        // Limit, locale and query each change the key — no incorrect sharing.
        assert_ne!(a, b);
        assert_ne!(a, d);
        assert_ne!(a, e);
        assert_eq!(a, digest_cache_key("world news", 8, "auto"));
    }

    // --- og:image enrichment (no-op paths are network-free) ----------------

    fn article(image: &str) -> DigestArticle {
        DigestArticle {
            title: "Story".into(),
            url: "https://news.google.com/rss/articles/abc".into(),
            engine: "googlenews".into(),
            teaser: String::new(),
            image_url_large: image.into(),
            publisher_url: String::new(),
            published_date: String::new(),
            favicon_url: String::new(),
            category: None,
        }
    }

    #[tokio::test]
    async fn enrich_noop_when_disabled() {
        let mut c = cfg();
        c.enrich_max = 0; // disabled → never touches the network
        let settings = Settings::default();
        let rt = Runtime::new(&settings);
        let mut arts = vec![article(""), article("")];
        enrich_with_og_images(&mut arts, &c, &settings, &rt).await;
        assert!(
            arts.iter().all(|a| a.image_url_large.is_empty()),
            "disabled enrichment leaves cards untouched"
        );
    }

    #[tokio::test]
    async fn enrich_noop_when_all_have_images() {
        // Every card already has an image → no enrichment targets, so the call
        // returns without any network I/O.
        let mut c = cfg();
        c.enrich_max = 8;
        let settings = Settings::default();
        let rt = Runtime::new(&settings);
        let pre = "https://cdn.example.com/hero-1200x630.jpg";
        let mut arts = vec![article(pre), article(pre)];
        enrich_with_og_images(&mut arts, &c, &settings, &rt).await;
        assert!(
            arts.iter().all(|a| a.image_url_large == pre),
            "existing images are preserved, nothing fetched"
        );
    }
}
