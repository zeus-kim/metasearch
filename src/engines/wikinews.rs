//! Wikinews engine via the MediaWiki search API (JSON, keyless). `news`.
//!
//! Free-content collaborative news. Uses `generator=search` with `pageimages`
//! so article thumbnails are available for the Discover feed.

use serde_json::Value;

use super::{body_error, EngineContext, WIKIMEDIA_USER_AGENT};
use crate::thumbnail::is_usable_thumbnail_url;
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let lang = ctx.lang_code();
    let host = format!("{lang}.wikinews.org");
    let url = format!("https://{host}/w/api.php");
    let offset = ctx.offset().to_string();
    let limit = ctx.max_results.to_string();
    let thumb_size = "640";

    let resp = ctx
        .client
        .get(&url)
        .header("User-Agent", WIKIMEDIA_USER_AGENT)
        .query(&[
            ("action", "query"),
            ("generator", "search"),
            ("gsrsearch", ctx.query),
            ("gsrlimit", &limit),
            ("gsroffset", &offset),
            ("prop", "pageimages"),
            ("piprop", "thumbnail"),
            ("pithumbsize", thumb_size),
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
    Ok(parse_wikinews(&body, &host))
}

/// Parse a Wikinews `generator=search` + `pageimages` response. Pure.
pub(crate) fn parse_wikinews(body: &Value, host: &str) -> Vec<EngineResult> {
    let article_base = format!("https://{host}/wiki/");
    let pages = match body["query"]["pages"].as_object() {
        Some(p) => p,
        None => return Vec::new(),
    };
    let mut results = Vec::new();
    for page in pages.values() {
        let title = page["title"].as_str().unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        let page_title = title.replace(' ', "_");
        let url = format!("{article_base}{page_title}");
        let snippet = page["snippet"]
            .as_str()
            .map(super::strip_html)
            .unwrap_or_default();
        let mut r = EngineResult::new(url, title, snippet);
        r.template = Some("default.html".into());
        r.category = Some("news".into());
        // NB: we deliberately do NOT use the MediaWiki `touched` field as a
        // publish date. `touched` is the last-*edit* time, so a years-old story
        // that received any later edit looks "fresh" — which let stale Wikinews
        // pieces (e.g. a 2012 election article) survive the recency cutoff. With
        // no trustworthy publication timestamp, Wikinews items stay undated and
        // are treated as not-known-recent by the digest's hard recency filter.
        if let Some(thumb) = page["thumbnail"]["source"]
            .as_str()
            .filter(|s| !s.is_empty() && is_usable_thumbnail_url(s))
        {
            r.thumbnail = Some(thumb.to_string());
            r.img_src = Some(thumb.to_string());
        }
        results.push(r);
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn parses_fixture_with_thumbnails() {
        let body: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/wikinews.json")).unwrap();
        let results = parse_wikinews(&body, "en.wikinews.org");
        assert!(!results.is_empty());
        assert!(results[0].url.contains("en.wikinews.org/wiki/"));
        assert_eq!(results[0].category.as_deref(), Some("news"));
        assert!(results.iter().any(|r| r.thumbnail.is_some()));
        assert!(results.iter().any(|r| r.img_src.is_some()));
    }

    #[test]
    fn legacy_list_search_still_parses() {
        let body: Value = serde_json::json!({
            "query": {
                "search": [{
                    "title": "Plain search hit",
                    "snippet": "No pageimages here."
                }]
            }
        });
        let results = super::super::parse_mediawiki(&body, "en.wikinews.org", "news");
        assert_eq!(results.len(), 1);
        assert!(results[0].thumbnail.is_none());
    }

    #[test]
    fn touched_is_not_used_as_publish_date() {
        // `touched` is a last-edit timestamp, not a publication date, so we must
        // NOT expose it as published_date (it let stale stories look fresh).
        let body: Value = serde_json::json!({
            "query": {
                "pages": {
                    "42": {
                        "title": "Old story edited recently",
                        "snippet": "snippet",
                        "touched": "2026-06-01T08:30:00Z"
                    }
                }
            }
        });
        let results = parse_wikinews(&body, "en.wikinews.org");
        assert_eq!(results.len(), 1);
        assert!(
            results[0].published_date.is_none(),
            "Wikinews items stay undated (touched is not a publish date)"
        );
    }
}
