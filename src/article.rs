//! SSRF-safe article fetch and main-text extraction (news full-page rewrite).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::thumbnail::{is_large_thumbnail, thumbnail_quality, ThumbnailQuality};
use crate::url_safety::{is_safe_public_url, safe_fetch_client};

const MAX_HTML_BYTES: usize = 2 * 1024 * 1024;
const FETCH_TIMEOUT: Duration = Duration::from_secs(6);
const CACHE_TTL: Duration = Duration::from_secs(3600);
/// Browser UA for article pages — many publishers block generic bot strings.
const ARTICLE_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArticleBody {
    pub url: String,
    pub title: String,
    pub text: String,
    #[serde(default)]
    pub images: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<String>,
}

/// In-process TTL cache for fetched article bodies (keyed by URL hash).
pub struct ArticleCache {
    max: usize,
    inner: Mutex<HashMap<[u8; 32], (Instant, ArticleBody)>>,
}

impl ArticleCache {
    pub fn new(max: usize) -> Self {
        ArticleCache {
            max: max.max(16),
            inner: Mutex::new(HashMap::new()),
        }
    }

    fn key(url: &str) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(url.trim().as_bytes());
        h.finalize().into()
    }

    pub fn get(&self, url: &str) -> Option<ArticleBody> {
        let key = Self::key(url);
        let map = self.inner.lock().ok()?;
        let (at, body) = map.get(&key)?;
        if at.elapsed() > CACHE_TTL {
            return None;
        }
        Some(body.clone())
    }

    pub fn clear(&self) {
        if let Ok(mut map) = self.inner.lock() {
            map.clear();
        }
    }

    pub fn put(&self, url: &str, body: ArticleBody) {
        let key = Self::key(url);
        if let Ok(mut map) = self.inner.lock() {
            if map.len() >= self.max {
                let now = Instant::now();
                map.retain(|_, (t, _)| now.duration_since(*t) < CACHE_TTL);
                if map.len() >= self.max {
                    map.clear();
                }
            }
            map.insert(key, (Instant::now(), body));
        }
    }
}

pub async fn fetch_article_cached(
    cache: &ArticleCache,
    url: &str,
    title_hint: &str,
) -> ArticleBody {
    if let Some(hit) = cache.get(url) {
        if is_usable_article_text(&hit.text, title_hint)
            && !crate::googlenews_decode::is_google_news_article_url(&hit.url)
        {
            return hit;
        }
    }
    let body = fetch_article(url, title_hint).await;
    if is_usable_article_text(&body.text, title_hint)
        && body.error.is_none()
        && !crate::googlenews_decode::is_google_news_article_url(&body.url)
    {
        cache.put(url, body.clone());
        if body.url.trim() != url.trim() {
            cache.put(&body.url, body.clone());
        }
    }
    body
}

pub async fn fetch_article(url: &str, title_hint: &str) -> ArticleBody {
    let original = url.trim();
    if original.is_empty() {
        return err_body(original, title_hint, "empty url");
    }
    if !is_safe_public_url(original) {
        return err_body(original, title_hint, "url not allowed");
    }

    // Google News RSS links are redirect wrappers — resolve to the publisher
    // page before extraction so the rewrite has real article text.
    let mut fetch_url = original.to_string();
    let is_gn = crate::googlenews_decode::is_google_news_article_url(original);
    if is_gn {
        if let Some(resolved) = crate::googlenews_decode::resolve_publisher_url(original).await {
            if is_safe_public_url(&resolved) {
                fetch_url = resolved;
            }
        }
        // Google blocks direct fetch — if resolve failed, return error instead of 503
        if fetch_url == original {
            return err_body(original, title_hint, "Google News URL - 원본 기사 링크를 클릭하세요");
        }
    }

    let client = safe_fetch_client();
    let resp = match client
        .get(&fetch_url)
        .header(reqwest::header::USER_AGENT, ARTICLE_USER_AGENT)
        .header(reqwest::header::ACCEPT, "text/html,application/xhtml+xml")
        .timeout(FETCH_TIMEOUT)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return err_body(original, title_hint, &format!("fetch failed: {e}")),
    };

    if !resp.status().is_success() {
        return err_body(
            original,
            title_hint,
            &format!("http {} (resolved: {fetch_url})", resp.status()),
        );
    }

    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => return err_body(original, title_hint, &format!("read failed: {e}")),
    };
    if bytes.len() > MAX_HTML_BYTES {
        return err_body(original, title_hint, "response too large");
    }

    let html = String::from_utf8_lossy(&bytes);
    let (title, text) = extract_article_text(&html, title_hint);
    let images = extract_large_images(&html);
    let fetch_url_still_gn = crate::googlenews_decode::is_google_news_article_url(&fetch_url);
    let error = if text.trim().is_empty() {
        Some("no extractable text".into())
    } else if fetch_url_still_gn || is_boilerplate_article_text(&text, title_hint) {
        Some(boilerplate_error_message(title_hint))
    } else {
        None
    };
    ArticleBody {
        url: if fetch_url != original {
            fetch_url
        } else {
            original.to_string()
        },
        title: if title.trim().eq_ignore_ascii_case("Google News") {
            title_hint.to_string()
        } else {
            title
        },
        text: if error.is_some() { String::new() } else { text },
        images,
        error,
    }
}

fn err_body(url: &str, title_hint: &str, msg: &str) -> ArticleBody {
    ArticleBody {
        url: url.to_string(),
        title: title_hint.to_string(),
        text: String::new(),
        images: Vec::new(),
        error: Some(msg.into()),
    }
}

/// True when extracted text is long enough and not Google News interstitial noise.
pub fn is_usable_article_text(text: &str, title_hint: &str) -> bool {
    text.trim().chars().count() >= 40 && !is_boilerplate_article_text(text, title_hint)
}

/// Detect Google News shell / meta copy that must never be rewritten as an article.
pub fn is_boilerplate_article_text(text: &str, title_hint: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if lower.contains("google news") {
        return true;
    }
    const GN_PHRASES: &[&str] = &[
        "news aggregator",
        "news aggregation",
        "comprehensive and up-to-date",
        "most widely used news",
        "most popular news aggregator",
        "personalized news feeds",
        "google's powerful search",
    ];
    if GN_PHRASES.iter().any(|p| lower.contains(p)) {
        return true;
    }
    // Korean headline but English-only body → almost certainly GN boilerplate.
    if has_hangul(title_hint) && !has_hangul(text) && text.chars().count() > 80 {
        return true;
    }
    false
}

pub fn has_hangul(s: &str) -> bool {
    s.chars().any(|c| {
        matches!(c, '\u{AC00}'..='\u{D7A3}' | '\u{1100}'..='\u{11FF}' | '\u{3130}'..='\u{318F}')
    })
}

fn boilerplate_error_message(title_hint: &str) -> String {
    if has_hangul(title_hint) {
        "기사 본문을 불러오지 못했습니다 (Google News 안내 페이지만 추출됨). 아래 원문 링크를 이용해 주세요.".into()
    } else {
        "Could not extract the publisher article (Google News interstitial only). Use the original link below.".into()
    }
}

fn extract_og_description(html: &str) -> Option<String> {
    let doc = Html::parse_document(html);
    for sel in [
        "meta[property='og:description']",
        "meta[name='description']",
        "meta[name='twitter:description']",
    ] {
        if let Ok(selector) = Selector::parse(sel) {
            for el in doc.select(&selector) {
                if let Some(content) = el.value().attr("content") {
                    let t = normalize_ws(content);
                    if t.chars().count() >= 40 {
                        return Some(t);
                    }
                }
            }
        }
    }
    None
}

pub fn extract_article_text(html: &str, title_hint: &str) -> (String, String) {
    let doc = Html::parse_document(html);
    let page_title = doc
        .select(&Selector::parse("title").unwrap())
        .next()
        .map(|el| normalize_ws(&el.text().collect::<String>()))
        .filter(|t| !t.is_empty())
        .or_else(|| {
            doc.select(&Selector::parse("h1").unwrap())
                .next()
                .map(|el| normalize_ws(&el.text().collect::<String>()))
        })
        .unwrap_or_else(|| title_hint.trim().to_string());

    for sel in ["article", "main", "[role=main]", "#article", "#content"] {
        if let Ok(selector) = Selector::parse(sel) {
            if let Some(node) = doc.select(&selector).next() {
                let text = paragraphs_from(node);
                if text.chars().count() >= 120 {
                    return (page_title, cap_text(&text));
                }
            }
        }
    }

    let p_sel = Selector::parse("p").unwrap();
    let paras: Vec<String> = doc
        .select(&p_sel)
        .map(|p| normalize_ws(&p.text().collect::<String>()))
        .filter(|s| s.chars().count() >= 40)
        .collect();
    let text = cap_text(&paras.join("\n\n"));

    // Fallback to og:description if extracted text is too short (JS-rendered pages)
    if text.chars().count() < 100 {
        if let Some(desc) = extract_og_description(&doc.html()) {
            return (page_title, desc);
        }
    }

    (page_title, text)
}

const OG_IMAGE_SELECTORS: [&str; 5] = [
    "meta[property='og:image']",
    "meta[property='og:image:url']",
    "meta[property='og:image:secure_url']",
    "meta[name='twitter:image']",
    "meta[name='twitter:image:src']",
];

fn extract_large_images(html: &str) -> Vec<String> {
    let doc = Html::parse_document(html);
    let mut out = Vec::new();
    for sel in OG_IMAGE_SELECTORS {
        if let Ok(selector) = Selector::parse(sel) {
            for el in doc.select(&selector) {
                if let Some(content) = el.value().attr("content") {
                    let u = content.trim();
                    if !u.is_empty()
                        && (is_large_thumbnail(u)
                            || thumbnail_quality(u) == ThumbnailQuality::Large)
                    {
                        out.push(u.to_string());
                    }
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Best-effort hero-image extraction from `og:image` / `twitter:image` meta
/// tags. Unlike [`extract_large_images`], this accepts any candidate that is
/// not provably an icon/tiny asset, because social-card images are large by
/// convention even when the URL carries no size hint. Returns the first usable
/// candidate (markup order), or `None`.
pub fn extract_og_image(html: &str) -> Option<String> {
    let doc = Html::parse_document(html);
    // 1. og:image / twitter:image social-card meta tags (best signal).
    for sel in OG_IMAGE_SELECTORS {
        if let Ok(selector) = Selector::parse(sel) {
            for el in doc.select(&selector) {
                if let Some(u) = usable_image_candidate(el.value().attr("content")) {
                    return Some(u);
                }
            }
        }
    }
    // 2. `<link rel="image_src">` — the legacy oEmbed/Facebook hint some
    //    publishers still emit instead of og:image.
    for rel in ["image_src", "image-src"] {
        if let Ok(selector) = Selector::parse(&format!("link[rel='{rel}']")) {
            for el in doc.select(&selector) {
                if let Some(u) = usable_image_candidate(el.value().attr("href")) {
                    return Some(u);
                }
            }
        }
    }
    // 3. First large inline `<img>` (prefer high-res hints), so pages with no
    //    social-card metadata still surface a real picture rather than nothing.
    if let Ok(selector) = Selector::parse("img") {
        for el in doc.select(&selector) {
            let v = el.value();
            let candidate = v
                .attr("src")
                .or_else(|| v.attr("data-src"))
                .or_else(|| v.attr("data-original"));
            if let Some(u) = candidate {
                let u = u.trim();
                if (u.starts_with("http://") || u.starts_with("https://")) && is_large_thumbnail(u)
                {
                    return Some(u.to_string());
                }
            }
        }
    }
    None
}

/// Accept a meta/link image value only if it is an absolute http(s) URL that
/// isn't provably icon-sized.
fn usable_image_candidate(value: Option<&str>) -> Option<String> {
    let u = value?.trim();
    if u.is_empty() || !(u.starts_with("http://") || u.starts_with("https://")) {
        return None;
    }
    if thumbnail_quality(u) != ThumbnailQuality::Small {
        Some(u.to_string())
    } else {
        None
    }
}

/// Fetch a page and return its `og:image` / `twitter:image`, bounded by
/// `timeout`. SSRF-guarded and fully graceful: any failure (blocked URL,
/// network error, oversized body, missing tag) yields `None`. Used to enrich
/// news cards that lack a large image without slowing the feed.
pub async fn fetch_og_image(url: &str, timeout: Duration) -> Option<String> {
    let url = url.trim();
    if url.is_empty() || !is_safe_public_url(url) {
        return None;
    }
    let client = safe_fetch_client();
    let resp = client
        .get(url)
        .header(reqwest::header::USER_AGENT, ARTICLE_USER_AGENT)
        .header(reqwest::header::ACCEPT, "text/html,application/xhtml+xml")
        .timeout(timeout)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let bytes = resp.bytes().await.ok()?;
    if bytes.is_empty() || bytes.len() > MAX_HTML_BYTES {
        return None;
    }
    let html = String::from_utf8_lossy(&bytes);
    extract_og_image(&html)
}

fn paragraphs_from(node: scraper::ElementRef<'_>) -> String {
    let p_sel = Selector::parse("p").unwrap();
    let parts: Vec<String> = node
        .select(&p_sel)
        .map(|p| normalize_ws(&p.text().collect::<String>()))
        .filter(|s| s.chars().count() >= 30)
        .collect();
    if !parts.is_empty() {
        return parts.join("\n\n");
    }
    let raw = normalize_ws(&node.text().collect::<String>());
    if raw.chars().count() >= 80 {
        raw
    } else {
        String::new()
    }
}

fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn cap_text(s: &str) -> String {
    s.chars().take(24_000).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_from_fixture_html() {
        let html = include_str!("../tests/fixtures/article_sample.html");
        let (title, text) = extract_article_text(html, "Fallback");
        assert!(title.contains("Sample"));
        assert!(text.contains("quantum processor"));
        assert!(text.chars().count() >= 100);
    }

    #[test]
    fn extract_og_image_prefers_social_card() {
        let html = r#"<html><head>
            <meta property="og:image" content="https://cdn.example.com/hero.jpg">
            <meta name="twitter:image" content="https://cdn.example.com/tw.jpg">
        </head><body></body></html>"#;
        assert_eq!(
            extract_og_image(html).as_deref(),
            Some("https://cdn.example.com/hero.jpg")
        );
    }

    #[test]
    fn extract_og_image_skips_tiny_icon() {
        let html = r#"<html><head>
            <meta property="og:image" content="https://cdn.example.com/favicon-16x16.png">
            <meta property="og:image" content="https://cdn.example.com/big.jpg">
        </head><body></body></html>"#;
        assert_eq!(
            extract_og_image(html).as_deref(),
            Some("https://cdn.example.com/big.jpg")
        );
    }

    #[test]
    fn extract_og_image_none_when_absent() {
        let html = "<html><head><title>No card</title></head><body><p>x</p></body></html>";
        assert_eq!(extract_og_image(html), None);
    }

    #[test]
    fn extract_og_image_accepts_google_news_thumbnail() {
        // Google News article pages expose the publisher thumbnail via og:image
        // as a googleusercontent URL with a `=s0-w300` sizing suffix.
        let html = r#"<html><head>
            <meta property="og:image" content="https://lh3.googleusercontent.com/abc=s0-w300">
        </head><body></body></html>"#;
        assert_eq!(
            extract_og_image(html).as_deref(),
            Some("https://lh3.googleusercontent.com/abc=s0-w300")
        );
    }

    #[test]
    fn extract_og_image_falls_back_to_link_image_src() {
        let html = r#"<html><head>
            <link rel="image_src" href="https://cdn.example.com/hero-1200x630.jpg">
        </head><body></body></html>"#;
        assert_eq!(
            extract_og_image(html).as_deref(),
            Some("https://cdn.example.com/hero-1200x630.jpg")
        );
    }

    #[test]
    fn extract_og_image_falls_back_to_first_large_img() {
        let html = r#"<html><head></head><body>
            <img src="https://cdn.example.com/icon-16x16.png">
            <img src="https://cdn.example.com/photo-800x600.jpg">
        </body></html>"#;
        assert_eq!(
            extract_og_image(html).as_deref(),
            Some("https://cdn.example.com/photo-800x600.jpg")
        );
    }

    #[test]
    fn extract_og_image_ignores_relative_and_tiny_img() {
        let html = r#"<html><head></head><body>
            <img src="/relative/photo.jpg">
            <img src="https://cdn.example.com/spacer-1x1.gif">
        </body></html>"#;
        assert_eq!(extract_og_image(html), None);
    }

    #[test]
    fn cache_roundtrip() {
        let cache = ArticleCache::new(8);
        let body = ArticleBody {
            url: "https://example.com/s".into(),
            title: "T".into(),
            text: "Long enough body for cache storage test.".into(),
            images: Vec::new(),
            error: None,
        };
        cache.put("https://example.com/s", body.clone());
        assert_eq!(cache.get("https://example.com/s").unwrap().text, body.text);
    }

    #[test]
    fn boilerplate_detects_google_news_copy() {
        let gn = "Google News provides a comprehensive and up-to-date news aggregation service.";
        assert!(is_boilerplate_article_text(gn, "Some headline"));
        assert!(!is_usable_article_text(gn, "Some headline"));
    }

    #[test]
    fn boilerplate_rejects_english_body_for_korean_title() {
        let body = "Google News stands as a major hub for news aggregation worldwide. \
            It offers personalized news feeds and comprehensive coverage from thousands \
            of publishers using Google's powerful search technology.";
        let title = "박근혜·이명박 '동시 등판' - KBS 뉴스";
        assert!(is_boilerplate_article_text(body, title));
    }

    #[test]
    fn usable_korean_article_passes() {
        let body = "박근혜 전 대통령과 이명박 전 대통령이 같은 무대에 섰다. \
            여야는 선거 개입 논란을 두고 공방을 벌였다.";
        let title = "박근혜·이명박 '동시 등판' - KBS 뉴스";
        assert!(!is_boilerplate_article_text(body, title));
        assert!(is_usable_article_text(body, title));
    }
}
