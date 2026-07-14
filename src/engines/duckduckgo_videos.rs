//! DuckDuckGo video search via the keyless `v.js` JSON endpoint.
//!
//! A short-lived `vqd` token is scraped from the videos landing page first
//! (same flow as the standard `duckduckgo videos` engine). Provides the `videos`
//! category.

use serde_json::Value;

use super::{ddg_images_safe, extract_vqd, EngineContext, USER_AGENT};
use crate::types::EngineResult;

const SITE_URL: &str = "https://duckduckgo.com/";

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let vqd = fetch_vqd(ctx).await?;

    let offset = ctx.offset().to_string();
    let safe = ddg_images_safe(ctx.safe_search);

    let resp = ctx
        .client
        .get("https://duckduckgo.com/v.js")
        .header("User-Agent", USER_AGENT)
        .header("Referer", SITE_URL)
        .query(&[
            ("q", ctx.query),
            ("o", "json"),
            ("vqd", &vqd),
            ("p", safe),
            ("s", &offset),
        ])
        .timeout(ctx.timeout)
        .send()
        .await
        .map_err(|e| super::request_error(&e))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let body: Value = resp.json().await.map_err(|e| super::body_error(&e))?;
    Ok(parse(&body, ctx.max_results))
}

async fn fetch_vqd(ctx: &EngineContext<'_>) -> Result<String, String> {
    let resp = ctx
        .client
        .get(SITE_URL)
        .header("User-Agent", USER_AGENT)
        .query(&[
            ("q", ctx.query),
            ("iar", "videos"),
            ("iax", "1"),
            ("ia", "videos"),
        ])
        .timeout(ctx.timeout)
        .send()
        .await
        .map_err(|e| super::request_error(&e))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let html = resp.text().await.map_err(|e| super::body_read_error(&e))?;
    extract_vqd(&html).ok_or_else(|| "could not extract vqd token".into())
}

/// Parse a DuckDuckGo `v.js` response. Pure for fixture testing.
pub(crate) fn parse(body: &Value, max_results: usize) -> Vec<EngineResult> {
    let items = match body["results"].as_array() {
        Some(i) => i,
        None => return Vec::new(),
    };
    let mut results = Vec::new();
    for item in items {
        let page_url = item["content"].as_str().unwrap_or_default();
        let title = item["title"].as_str().unwrap_or_default();
        if page_url.is_empty() || title.is_empty() {
            continue;
        }
        let publisher = item["publisher"].as_str().unwrap_or_default();
        let uploader = item["uploader"].as_str().unwrap_or_default();
        let duration = item["duration"].as_str().unwrap_or_default();
        let mut content = String::new();
        if !publisher.is_empty() {
            content.push_str(publisher);
        } else if !uploader.is_empty() {
            content.push_str(uploader);
        }
        if !duration.is_empty() {
            if !content.is_empty() {
                content.push_str(" · ");
            }
            content.push_str(duration);
        }
        let thumb = item["images"]["large"].as_str().or_else(|| {
            item["images"]["medium"]
                .as_str()
                .or_else(|| item["images"]["small"].as_str())
        });
        let mut r = EngineResult::new(page_url, title, content);
        r.template = Some("videos.html".into());
        r.category = Some("videos".into());
        if let Some(t) = thumb.filter(|s| !s.is_empty()) {
            r.thumbnail = Some(t.to_string());
            r.img_src = Some(t.to_string());
        }
        results.push(r);
        if results.len() >= max_results {
            break;
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fixture() {
        let body: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/duckduckgo_videos.json"))
                .unwrap();
        let results = parse(&body, 10);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Rust Programming Language");
        assert_eq!(results[0].url, "https://example.com/rust-video");
        assert_eq!(results[0].template.as_deref(), Some("videos.html"));
        assert!(results[0].content.contains("Mozilla"));
    }
}
