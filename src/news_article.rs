//! Full-page news article rewrite with SSE streaming and related media.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ai::{self, TokenUsage};
use crate::article::{fetch_article_cached, is_usable_article_text, ArticleBody};
use crate::config::Settings;
use crate::googlenews_decode;
use crate::search::{search_all, Runtime, SearchParams};
use crate::thumbnail::{is_large_thumbnail, thumbnail_quality, ThumbnailQuality};
use crate::types::SearchResult;

const MAX_MEDIA: usize = 6;
const MIN_MEDIA_SIDEBAR: usize = 2;
const REWRITE_CACHE_TTL: Duration = Duration::from_secs(6 * 3600);
pub const ARTICLE_REWRITE_PROMPT_VERSION: &str = "news-full-page-v2-keypoints";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewsArticleSource {
    pub title: String,
    pub url: String,
    pub engine: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub favicon: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewsMediaItem {
    pub url: String,
    pub thumb: String,
    pub title: String,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewsArticleResponse {
    pub title: String,
    pub url: String,
    pub engine: String,
    pub article: String,
    pub sections: Vec<String>,
    pub source: NewsArticleSource,
    pub media: Vec<NewsMediaItem>,
    /// Token usage statistics (for API-based models).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
}

/// In-process cache for completed article rewrites. Values are keyed by
/// resolved publisher URL plus model and prompt version; callers may also store
/// the original card URL as an alias so repeat Google News clicks skip all work.
pub struct NewsArticleRewriteCache {
    max: usize,
    inner: Mutex<HashMap<[u8; 32], (Instant, NewsArticleResponse)>>,
}

impl NewsArticleRewriteCache {
    pub fn new(max: usize) -> Self {
        Self {
            max: max.max(16),
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub fn get(&self, url: &str, title: &str, model: &str) -> Option<NewsArticleResponse> {
        let key = rewrite_cache_key(url, title, model);
        let map = self.inner.lock().ok()?;
        let (at, resp) = map.get(&key)?;
        if at.elapsed() > REWRITE_CACHE_TTL {
            return None;
        }
        Some(resp.clone())
    }

    pub fn put(&self, urls: &[&str], title: &str, model: &str, resp: NewsArticleResponse) {
        if let Ok(mut map) = self.inner.lock() {
            if map.len() >= self.max {
                let now = Instant::now();
                map.retain(|_, (t, _)| now.duration_since(*t) < REWRITE_CACHE_TTL);
                if map.len() >= self.max {
                    map.clear();
                }
            }
            for url in urls.iter().map(|u| u.trim()).filter(|u| !u.is_empty()) {
                map.insert(
                    rewrite_cache_key(url, title, model),
                    (Instant::now(), resp.clone()),
                );
            }
        }
    }

    pub fn clear(&self) {
        if let Ok(mut map) = self.inner.lock() {
            map.clear();
        }
    }
}

pub fn effective_article_model_name(settings: &Settings, override_model: Option<&str>) -> String {
    if let Some(m) = override_model.map(str::trim).filter(|m| !m.is_empty()) {
        return m.to_string();
    }
    let article = settings.ai.article_model.trim();
    if !article.is_empty() {
        return article.to_string();
    }
    settings.ai.model.trim().to_string()
}

fn rewrite_cache_key(url: &str, title: &str, model: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(ARTICLE_REWRITE_PROMPT_VERSION.as_bytes());
    h.update([0]);
    h.update(model.trim().as_bytes());
    h.update([0]);
    h.update(url.trim().as_bytes());
    h.update([0]);
    h.update(title.trim().as_bytes());
    h.finalize().into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedEvent {
    pub title: String,
    pub url: String,
    pub text_chars: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SectionsDoneEvent {
    pub sections: Vec<String>,
}

/// Fetch article body, resolving Google News redirects and falling back to a
/// site-scoped news search when decode is unavailable.
pub async fn fetch_article_for_rewrite(
    cache: &crate::article::ArticleCache,
    url: &str,
    title_hint: &str,
    publisher_url: &str,
    settings: &Settings,
    rt: &Runtime,
) -> ArticleBody {
    let body = fetch_article_cached(cache, url, title_hint).await;
    if body.error.is_none()
        && is_usable_article_text(&body.text, title_hint)
        && !googlenews_decode::is_google_news_article_url(&body.url)
    {
        return normalize_article_body(body, title_hint);
    }

    // Try to resolve Google News URL to actual publisher URL
    if googlenews_decode::is_google_news_article_url(url) {
        if let Some(resolved) = googlenews_decode::resolve_publisher_url(url).await {
            let retry = fetch_article_cached(cache, &resolved, title_hint).await;
            if retry.error.is_none() && is_usable_article_text(&retry.text, title_hint) {
                cache.put(url, retry.clone());
                return normalize_article_body(retry, title_hint);
            }
        }
    }

    if googlenews_decode::is_google_news_article_url(url) && !publisher_url.trim().is_empty() {
        if publisher_host(publisher_url).is_some_and(|host| host.ends_with("kbs.co.kr")) {
            if let Some(kbs) = search_kbs_article_body(title_hint, rt).await {
                if kbs.error.is_none() && is_usable_article_text(&kbs.text, title_hint) {
                    cache.put(&kbs.url, kbs.clone());
                    cache.put(url, kbs.clone());
                    return normalize_article_body(kbs, title_hint);
                }
            }
        }
        if let Some(found) = search_publisher_article(title_hint, publisher_url, settings, rt).await
        {
            let retry = fetch_article_cached(cache, &found, title_hint).await;
            if retry.error.is_none()
                && is_usable_article_text(&retry.text, title_hint)
                && !googlenews_decode::is_google_news_article_url(&retry.url)
            {
                return normalize_article_body(retry, title_hint);
            }
        }
    }

    if body.error.is_some() {
        return body;
    }
    err_article_body(url, title_hint, boilerplate_error_for_title(title_hint))
}

fn normalize_article_body(mut body: ArticleBody, title_hint: &str) -> ArticleBody {
    if body.title.trim().eq_ignore_ascii_case("Google News")
        || title_hint.chars().count() > body.title.chars().count() + 8
    {
        body.title = title_hint.to_string();
    }
    body
}

fn err_article_body(url: &str, title_hint: &str, msg: &str) -> ArticleBody {
    ArticleBody {
        url: url.to_string(),
        title: title_hint.to_string(),
        text: String::new(),
        images: Vec::new(),
        error: Some(msg.into()),
    }
}

fn boilerplate_error_for_title(title_hint: &str) -> &'static str {
    if crate::article::has_hangul(title_hint) {
        "기사 본문을 불러오지 못했습니다. 아래 원문 링크를 이용해 주세요."
    } else {
        "Could not load the publisher article. Use the original link below."
    }
}

async fn search_publisher_article(
    title: &str,
    publisher_home: &str,
    settings: &Settings,
    rt: &Runtime,
) -> Option<String> {
    let headline = headline_for_search(title);
    if headline.trim().chars().count() < 6 {
        return None;
    }
    let host = publisher_host(publisher_home)?;
    if host.ends_with("kbs.co.kr") {
        if let Some(url) = search_kbs_article(&headline, rt).await {
            return Some(url);
        }
    }

    let site_q = format!("{headline} site:{host}");
    // General search first — news category often returns only Google News redirects.
    if let Some(url) = pick_publisher_hit(
        search_all(&general_params(&site_q), settings, rt).await,
        &host,
        &headline,
    ) {
        return Some(url);
    }
    if let Some(url) = pick_publisher_hit(
        search_all(&news_params(&site_q), settings, rt).await,
        &host,
        &headline,
    ) {
        return Some(url);
    }
    if let Some(url) = pick_publisher_hit(
        search_all(&general_params(&headline), settings, rt).await,
        &host,
        &headline,
    ) {
        return Some(url);
    }

    None
}

async fn search_kbs_article(headline: &str, rt: &Runtime) -> Option<String> {
    search_kbs_article_body(headline, rt)
        .await
        .map(|body| body.url)
}

async fn search_kbs_article_body(headline: &str, rt: &Runtime) -> Option<ArticleBody> {
    let resp = rt
        .client
        .get("https://reco.kbs.co.kr/v2/search")
        .query(&[
            ("target", "newstotal"),
            ("keyword", headline),
            ("page", "1"),
            ("page_size", "5"),
            ("sort_option", "date"),
            ("searchfield", "all"),
            ("categoryfield", ""),
            ("sdate", ""),
            ("edate", ""),
            ("include", ""),
            ("exclude", ""),
        ])
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: serde_json::Value = resp.json().await.ok()?;
    let tokens = headline_tokens(headline);
    v.get("data")?.as_array()?.iter().find_map(|item| {
        let title = item.get("title").and_then(|v| v.as_str()).unwrap_or("");
        if !tokens.is_empty() && !tokens.iter().any(|t| title.contains(t)) {
            return None;
        }
        kbs_article_body_from_item(item)
    })
}

fn kbs_article_body_from_item(item: &serde_json::Value) -> Option<ArticleBody> {
    let id = item.get("contents_id")?.as_i64()?;
    let title = item.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let text = item
        .get("contents")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if text.chars().count() < 40 {
        return None;
    }
    let url = item
        .get("target_url")
        .and_then(|v| v.as_str())
        .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
        .map(str::to_string)
        .unwrap_or_else(|| format!("https://news.kbs.co.kr/news/pc/view/view.do?ncd={id}"));
    let images = ["image_w", "image_o", "image_h", "image_s"]
        .into_iter()
        .filter_map(|key| item.get(key).and_then(|v| v.as_str()))
        .map(str::trim)
        .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
        .map(str::to_string)
        .collect();
    Some(ArticleBody {
        url,
        title: if title.trim().is_empty() {
            format!("KBS 뉴스 {id}")
        } else {
            title.to_string()
        },
        text: text.to_string(),
        images,
        error: None,
    })
}

fn publisher_host(publisher_home: &str) -> Option<String> {
    url::Url::parse(publisher_home.trim()).ok().and_then(|u| {
        u.host_str()
            .map(|h| h.trim_start_matches("www.").to_string())
    })
}

fn headline_tokens(headline: &str) -> Vec<String> {
    headline
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|w| w.chars().count() >= 3)
        .take(6)
        .map(|w| w.to_string())
        .collect()
}

fn news_params(q: &str) -> SearchParams {
    let mut params = SearchParams::new(q);
    params.categories = vec!["news".into()];
    params.ai_answer = Some(false);
    params
}

fn general_params(q: &str) -> SearchParams {
    let mut params = SearchParams::new(q);
    params.ai_answer = Some(false);
    params
}

fn pick_publisher_hit(
    response: crate::search::SearchResponse,
    host: &str,
    headline: &str,
) -> Option<String> {
    let tokens: Vec<String> = headline
        .split_whitespace()
        .filter(|w| w.chars().count() >= 2)
        .map(|w| w.to_string())
        .collect();
    let mut best: Option<(i32, String)> = None;
    for r in response.results {
        if googlenews_decode::is_google_news_article_url(&r.url) {
            continue;
        }
        if !(r.url.contains(host) || r.parsed_url.get(1).is_some_and(|h| h.contains(host))) {
            continue;
        }
        let hay = format!("{} {}", r.title, r.url).to_lowercase();
        let score = tokens
            .iter()
            .filter(|t| hay.contains(&t.to_lowercase()))
            .count() as i32;
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

/// Non-streaming full article rewrite.
pub async fn rewrite_news_article(
    url: &str,
    title_hint: &str,
    engine: &str,
    publisher_url: &str,
    settings: &Settings,
    rt: &Runtime,
    model: Option<&str>,
) -> Result<NewsArticleResponse, String> {
    let model_key = effective_article_model_name(settings, model);
    if let Some(hit) = rt.article_rewrite_cache.get(url, title_hint, &model_key) {
        return Ok(hit);
    }

    let body = fetch_article_for_rewrite(
        &rt.article_cache,
        url,
        title_hint,
        publisher_url,
        settings,
        rt,
    )
    .await;
    // Use extracted title if hint is empty
    let effective_title = if title_hint.trim().is_empty() {
        &body.title
    } else {
        title_hint
    };
    if body.error.is_some() || !is_usable_article_text(&body.text, effective_title) {
        return Err(body
            .error
            .unwrap_or_else(|| "could not extract article text".into()));
    }
    if let Some(hit) = rt
        .article_rewrite_cache
        .get(&body.url, effective_title, &model_key)
    {
        return Ok(hit);
    }

    let article = if settings.ai.enabled {
        ai::rewrite_news_full_page(
            &settings.ai,
            &rt.client,
            effective_title,
            &body.url,
            &body.text,
            Some(&model_key),
            None, // perspective - TODO: pass from request
        )
        .await?
    } else {
        return Err("AI disabled — enable ai.enabled for full article rewrite".into());
    };

    let sections = parse_sections(&article);
    let media = related_media_for(&body, effective_title, settings, rt).await;

    let resp = NewsArticleResponse {
        title: effective_title.to_string(),
        url: body.url.clone(),
        engine: engine.to_string(),
        article,
        sections: sections.clone(),
        source: build_source(&body, engine),
        media,
        usage: None, // TODO: pass through from AI layer
    };
    rt.article_rewrite_cache
        .put(&[url, &body.url], effective_title, &model_key, resp.clone());
    Ok(resp)
}

/// Stream rewrite: calls `on_event(event, json_data)` for each SSE payload.
#[allow(clippy::too_many_arguments)]
pub async fn stream_news_article<F>(
    url: &str,
    title_hint: &str,
    engine: &str,
    publisher_url: &str,
    settings: &Settings,
    rt: &Runtime,
    model: Option<&str>,
    mut on_event: F,
) -> Result<(), String>
where
    F: FnMut(&str, &str),
{
    let body = fetch_article_for_rewrite(
        &rt.article_cache,
        url,
        title_hint,
        publisher_url,
        settings,
        rt,
    )
    .await;
    if body.error.is_some() || !is_usable_article_text(&body.text, title_hint) {
        return Err(body
            .error
            .unwrap_or_else(|| "could not extract article text".into()));
    }

    on_event(
        "extracted",
        &serde_json::to_string(&ExtractedEvent {
            title: title_hint.to_string(),
            url: body.url.clone(),
            text_chars: body.text.chars().count(),
        })
        .unwrap_or_else(|_| "{}".into()),
    );

    if !settings.ai.enabled {
        return Err("AI disabled".into());
    }

    let mut article = String::new();
    #[allow(unused_assignments)]
    let mut usage: Option<TokenUsage> = None;
    let text = body.text.clone();
    let source_url = body.url.clone();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let ai_cfg = settings.ai.clone();
    let client = rt.client.clone();
    let model_owned = model.map(|s| s.to_string());
    let card_title = title_hint.to_string();
    let handle = tokio::spawn(async move {
        ai::stream_news_article(
            &ai_cfg,
            &client,
            &card_title,
            &source_url,
            &text,
            model_owned.as_deref(),
            None, // perspective - TODO: pass from request
            |t| {
                let _ = tx.send(t.to_string());
            },
        )
        .await
    });

    while let Some(tok) = rx.recv().await {
        article.push_str(&tok);
        on_event("token", &serde_json::json!({ "text": tok }).to_string());
    }

    match handle.await {
        Ok(Ok(result)) => {
            article = result.article;
            usage = result.usage;
        }
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err("rewrite task aborted".into()),
    }

    let sections = parse_sections(&article);
    on_event(
        "sections_done",
        &serde_json::to_string(&SectionsDoneEvent {
            sections: sections.clone(),
        })
        .unwrap_or_else(|_| "{}".into()),
    );

    let media = related_media_for(&body, title_hint, settings, rt).await;
    on_event(
        "media",
        &serde_json::json!({ "items": media, "source": build_source(&body, engine) }).to_string(),
    );

    on_event(
        "done",
        &serde_json::json!({
            "title": title_hint,
            "url": body.url,
            "engine": engine,
            "sections": sections,
            "usage": usage,
        })
        .to_string(),
    );

    Ok(())
}

pub fn parse_sections(article: &str) -> Vec<String> {
    article
        .lines()
        .filter_map(|line| {
            let t = line.trim();
            if t.starts_with("## ") {
                Some(t.trim_start_matches("## ").trim().to_string())
            } else {
                None
            }
        })
        .collect()
}

pub fn build_source(body: &ArticleBody, engine: &str) -> NewsArticleSource {
    let favicon = favicon_for_url(&body.url);
    NewsArticleSource {
        title: body.title.clone(),
        url: body.url.clone(),
        engine: engine.to_string(),
        favicon,
    }
}

fn favicon_for_url(url: &str) -> String {
    if let Ok(parsed) = url::Url::parse(url) {
        if let Some(host) = parsed.host_str() {
            return format!("https://icons.duckduckgo.com/ip3/{host}.ico");
        }
    }
    String::new()
}

pub async fn related_media_for(
    body: &ArticleBody,
    headline: &str,
    settings: &Settings,
    rt: &Runtime,
) -> Vec<NewsMediaItem> {
    if googlenews_decode::is_google_news_article_url(&body.url) {
        return Vec::new();
    }

    let mut items: Vec<NewsMediaItem> = Vec::new();

    for img in &body.images {
        if is_google_branded_media(img, "")
            || !is_large_thumbnail(img)
            || !image_relevant(headline, "", img, true)
        {
            continue;
        }
        items.push(NewsMediaItem {
            url: img.clone(),
            thumb: img.clone(),
            title: headline.to_string(),
            source: "og".into(),
        });
    }

    if items.len() < MIN_MEDIA_SIDEBAR {
        let search_items = image_search(headline, settings, rt).await;
        for item in search_items {
            if is_google_branded_media(&item.url, &item.title)
                || items.iter().any(|e| e.url == item.url)
            {
                continue;
            }
            items.push(item);
            if items.len() >= MAX_MEDIA {
                break;
            }
        }
    }

    items.retain(|m| {
        !is_google_branded_media(&m.url, &m.title)
            && thumbnail_quality(&m.thumb) == ThumbnailQuality::Large
            && image_relevant(headline, &m.title, &m.url, m.source == "og")
    });

    // Return whatever we found, even if less than MIN_MEDIA_SIDEBAR
    items.truncate(MAX_MEDIA);
    items
}

async fn image_search(headline: &str, settings: &Settings, rt: &Runtime) -> Vec<NewsMediaItem> {
    let q = headline.trim();
    if q.is_empty() {
        return Vec::new();
    }
    let mut params = SearchParams::new(q);
    params.categories = vec!["images".into()];
    params.ai_answer = Some(false);
    let response = search_all(&params, settings, rt).await;

    response
        .results
        .into_iter()
        .filter_map(|r| result_to_media(&r, headline))
        .take(MAX_MEDIA)
        .collect()
}

fn result_to_media(r: &SearchResult, headline: &str) -> Option<NewsMediaItem> {
    let thumb = if !r.img_src.is_empty() {
        r.img_src.clone()
    } else {
        r.thumbnail.clone()
    };
    if thumb.is_empty() || thumbnail_quality(&thumb) != ThumbnailQuality::Large {
        return None;
    }
    if is_stock_photo(&thumb) || is_stock_photo(&r.url) {
        return None;
    }
    if !image_relevant(headline, &r.title, &r.url, false) {
        return None;
    }
    Some(NewsMediaItem {
        url: r.url.clone(),
        thumb,
        title: r.title.clone(),
        source: "search".into(),
    })
}

pub fn image_relevant(headline: &str, img_title: &str, img_url: &str, from_page: bool) -> bool {
    if from_page {
        return true;
    }
    let hay = format!("{} {}", img_title, img_url).to_ascii_lowercase();
    let words: Vec<String> = headline
        .split_whitespace()
        .filter(|w| w.chars().count() >= 4)
        .map(|w| {
            w.trim_matches(|c: char| !c.is_alphanumeric())
                .to_ascii_lowercase()
        })
        .filter(|w| w.len() >= 4)
        .collect();
    if words.is_empty() {
        return headline
            .chars()
            .filter(|c| c.is_alphanumeric())
            .take(12)
            .count()
            >= 4
            && hay.contains(
                &headline
                    .chars()
                    .filter(|c| c.is_alphanumeric())
                    .take(8)
                    .collect::<String>()
                    .to_ascii_lowercase(),
            );
    }
    words.iter().filter(|w| hay.contains(w.as_str())).count() >= 1
}

fn is_stock_photo(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.contains("shutterstock")
        || lower.contains("gettyimages")
        || lower.contains("istockphoto")
        || lower.contains("depositphotos")
        || lower.contains("dreamstime")
        || lower.contains("123rf")
        || lower.contains("stock.adobe")
        || lower.contains("placeholder")
        || lower.contains("random=dog")
        || lower.contains("/dog-")
}

fn is_google_branded_media(url: &str, title: &str) -> bool {
    let hay = format!("{url} {title}").to_ascii_lowercase();
    hay.contains("google.com/logos")
        || hay.contains("google.com/images/branding")
        || hay.contains("gstatic.com/images/branding")
        || hay.contains("google news logo")
        || (hay.contains("googleusercontent.com") && hay.contains("google"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sections_finds_h2() {
        let md = "## Amazon's Breakthrough\n\nPara.\n\n## Industry Implications\n\nMore.";
        let s = parse_sections(md);
        assert_eq!(s.len(), 2);
        assert!(s[0].contains("Amazon"));
    }

    #[test]
    fn image_relevant_requires_keyword() {
        assert!(image_relevant(
            "Amazon quantum networking breakthrough",
            "Amazon data center rack",
            "https://example.com/amazon-rack.jpg",
            false
        ));
        assert!(!image_relevant(
            "Amazon quantum networking breakthrough",
            "Cute puppy playing",
            "https://example.com/random-dog.jpg",
            false
        ));
    }

    #[test]
    fn og_images_always_relevant() {
        assert!(image_relevant(
            "Anything",
            "",
            "https://example.com/og.jpg",
            true
        ));
    }

    #[test]
    fn pick_publisher_prefers_matching_host_and_title() {
        use crate::search::SearchResponse;
        use crate::types::SearchResult;
        let host = "news.kbs.co.kr";
        let headline = "박근혜·이명박 동시 등판";
        let kbs = SearchResult {
            url: "https://news.kbs.co.kr/news/pc/view/view.do?ncd=8574516".into(),
            title: headline.into(),
            parsed_url: [
                "https".into(),
                host.into(),
                "/news/pc/view/view.do".into(),
                String::new(),
                "ncd=8574516".into(),
                String::new(),
            ],
            ..Default::default()
        };
        let gn = SearchResult {
            url: "https://news.google.com/rss/articles/CBMi".into(),
            title: headline.into(),
            ..Default::default()
        };
        let response = SearchResponse {
            results: vec![gn, kbs],
            ..SearchResponse::empty("q".into(), 1)
        };
        let picked = pick_publisher_hit(response, host, headline);
        assert_eq!(
            picked.as_deref(),
            Some("https://news.kbs.co.kr/news/pc/view/view.do?ncd=8574516")
        );
    }

    #[test]
    fn headline_for_search_strips_publisher_suffix() {
        let t = "박근혜·이명박 '동시 등판' - KBS 뉴스";
        assert!(!headline_for_search(t).contains("KBS"));
    }

    #[test]
    fn kbs_article_body_uses_json_content() {
        let item = serde_json::json!({
            "contents_id": 8573971,
            "title": "국민영웅 히딩크 감독의 조언",
            "contents": "히딩크 감독은 체코와의 첫 경기에 집중해야 한다고 조언했다. 대표팀은 본선 준비 상황을 점검하고 있다.",
            "target_url": "https://news.kbs.co.kr/news/view.do?ncd=8573971",
            "image_w": "https://news.kbs.co.kr/data/news/title_image/news.jpg"
        });
        let body = kbs_article_body_from_item(&item).unwrap();
        assert!(body.text.contains("히딩크"));
        assert_eq!(body.url, "https://news.kbs.co.kr/news/view.do?ncd=8573971");
        assert_eq!(body.images.len(), 1);
    }
}
