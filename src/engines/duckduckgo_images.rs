//! DuckDuckGo image search via the keyless `i.js` JSON endpoint.
//!
//! A short-lived `vqd` token is scraped from the images landing page first
//! (same flow as the standard `duckduckgo_images` engine). Provides the `images`
//! category.

use serde_json::Value;

use super::{ddg_images_safe, extract_vqd, EngineContext, USER_AGENT};
use crate::types::EngineResult;

const SITE_URL: &str = "https://duckduckgo.com/";

/// Image grids want density, so fetch/return more per engine than the general
/// per-engine cap (DDG's `i.js` page already carries ~100 candidates).
const IMAGE_TARGET: usize = 30;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let vqd = fetch_vqd(ctx).await?;

    let offset = ctx.offset().to_string();
    let safe = ddg_images_safe(ctx.safe_search);

    let resp = ctx
        .client
        .get("https://duckduckgo.com/i.js")
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
    Ok(parse(&body, ctx.max_results.max(IMAGE_TARGET)))
}

async fn fetch_vqd(ctx: &EngineContext<'_>) -> Result<String, String> {
    let resp = ctx
        .client
        .get(SITE_URL)
        .header("User-Agent", USER_AGENT)
        .query(&[
            ("q", ctx.query),
            ("iar", "images"),
            ("iax", "1"),
            ("ia", "images"),
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

/// Parse a DuckDuckGo `i.js` response. Pure for fixture testing.
pub(crate) fn parse(body: &Value, max_results: usize) -> Vec<EngineResult> {
    let items = match body["results"].as_array() {
        Some(i) => i,
        None => return Vec::new(),
    };
    let mut results = Vec::new();
    for item in items {
        let page_url = item["url"].as_str().unwrap_or_default();
        let title = item["title"].as_str().unwrap_or_default();
        let img = item["image"].as_str().unwrap_or_default();
        let thumb = item["thumbnail"].as_str().unwrap_or(img);
        if page_url.is_empty() || title.is_empty() || img.is_empty() {
            continue;
        }
        // `i.js` reports the full-size dimensions; skip provably icon-sized hits
        // so the grid prefers real, large imagery.
        let width = item["width"].as_u64().unwrap_or(0) as u32;
        let height = item["height"].as_u64().unwrap_or(0) as u32;
        if crate::thumbnail::is_tiny_dimension(width, height) {
            continue;
        }
        let source = item["source"].as_str().unwrap_or_default();
        let mut content = String::new();
        if !source.is_empty() {
            content = source.to_string();
        }
        results.push(EngineResult::new(page_url, title, content).image(img, thumb));
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
            serde_json::from_str(include_str!("../../tests/fixtures/duckduckgo_images.json"))
                .unwrap();
        let results = parse(&body, 10);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Test Cat Image");
        assert_eq!(results[0].url, "https://example.com/cat-page");
        assert_eq!(
            results[0].img_src.as_deref(),
            Some("https://example.com/cat-full.jpg")
        );
        assert_eq!(results[0].template.as_deref(), Some("images.html"));
    }

    #[test]
    fn extract_vqd_from_html() {
        let html = r#"<script>vqd="4-12345678901234567890123456789012345678"</script>"#;
        assert_eq!(
            super::super::extract_vqd(html).as_deref(),
            Some("4-12345678901234567890123456789012345678")
        );
    }
}
