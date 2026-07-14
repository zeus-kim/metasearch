//! Wikimedia Commons image engine via the MediaWiki API (JSON, keyless).
//!
//! Provides the `images` category. Thumbnails can be proxied by the server's
//! `/image_proxy` so clients never contact upstream image hosts directly.

use serde_json::Value;

use super::{EngineContext, WIKIMEDIA_USER_AGENT};
use crate::types::EngineResult;

/// Image grids want density, so fetch/return more per engine than the general
/// per-engine cap.
const IMAGE_TARGET: usize = 30;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let limit = ctx.max_results.max(IMAGE_TARGET).clamp(1, 50).to_string();
    let offset = ctx.offset().to_string();

    let resp = ctx
        .client
        .get("https://commons.wikimedia.org/w/api.php")
        .header("User-Agent", WIKIMEDIA_USER_AGENT)
        .query(&[
            ("action", "query"),
            ("generator", "search"),
            ("gsrsearch", ctx.query),
            ("gsrlimit", &limit),
            ("gsroffset", &offset),
            ("gsrnamespace", "6"), // File:
            ("prop", "imageinfo"),
            ("iiprop", "url|size"),
            ("iiurlwidth", "400"),
            ("format", "json"),
        ])
        .timeout(ctx.timeout)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let body: Value = resp.json().await.map_err(|e| super::body_error(&e))?;
    Ok(parse(&body))
}

/// Parse a Commons image-search response. Pure for fixture testing.
pub(crate) fn parse(body: &Value) -> Vec<EngineResult> {
    let pages = match body["query"]["pages"].as_object() {
        Some(p) => p,
        None => return Vec::new(),
    };
    // Pages come keyed by id; order by the `index` field the API provides.
    let mut entries: Vec<&Value> = pages.values().collect();
    entries.sort_by_key(|p| p["index"].as_i64().unwrap_or(i64::MAX));

    let mut results = Vec::new();
    for page in entries {
        let title = page["title"].as_str().unwrap_or_default();
        let info = match page["imageinfo"].as_array().and_then(|a| a.first()) {
            Some(i) => i,
            None => continue,
        };
        let full = info["url"].as_str().unwrap_or_default();
        let thumb = info["thumburl"].as_str().unwrap_or(full);
        let desc_url = info["descriptionurl"].as_str().unwrap_or(full);
        if title.is_empty() || full.is_empty() {
            continue;
        }
        // Prefer full size for the tiny check, falling back to the thumb size.
        let width = info["width"]
            .as_u64()
            .or_else(|| info["thumbwidth"].as_u64())
            .unwrap_or(0) as u32;
        let height = info["height"]
            .as_u64()
            .or_else(|| info["thumbheight"].as_u64())
            .unwrap_or(0) as u32;
        if crate::thumbnail::is_tiny_dimension(width, height) {
            continue;
        }
        let display = title.strip_prefix("File:").unwrap_or(title);
        let mut r = EngineResult::new(desc_url, display, String::new());
        r.img_src = Some(full.to_string());
        r.thumbnail = Some(thumb.to_string());
        r.template = Some("images.html".into());
        r.category = Some("images".into());
        results.push(r);
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fixture() {
        let body: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/wikicommons.json")).unwrap();
        let results = parse(&body);
        assert!(!results.is_empty());
        assert_eq!(results[0].template.as_deref(), Some("images.html"));
        assert!(results[0].img_src.is_some());
        assert!(results[0].thumbnail.is_some());
    }
}
