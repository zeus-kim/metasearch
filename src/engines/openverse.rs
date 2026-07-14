//! Openverse (Creative Commons) image search via the public JSON API (keyless).
//!
//! API docs: <https://api.openverse.org/>. Unauthenticated requests are capped
//! at 240 total results per query; no API key is required for normal use.

use serde_json::Value;

use super::{body_error, request_error, EngineContext, USER_AGENT};
use crate::types::EngineResult;

/// Image grids want density, so fetch/return more per engine than the general
/// per-engine cap.
const IMAGE_TARGET: usize = 30;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let want = ctx.max_results.max(IMAGE_TARGET);
    let page_size = want.clamp(1, 50).to_string();
    let page = ctx.pageno.max(1).to_string();

    let resp = ctx
        .client
        .get("https://api.openverse.org/v1/images/")
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json")
        .query(&[
            ("q", ctx.query),
            ("page_size", page_size.as_str()),
            ("page", page.as_str()),
        ])
        .timeout(ctx.timeout)
        .send()
        .await
        .map_err(|e| request_error(&e))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let body: Value = resp.json().await.map_err(|e| body_error(&e))?;
    Ok(parse(&body, want))
}

/// Parse an Openverse `/v1/images/` response. Pure for fixture testing.
pub(crate) fn parse(body: &Value, max_results: usize) -> Vec<EngineResult> {
    let items = match body["results"].as_array() {
        Some(i) => i,
        None => return Vec::new(),
    };
    let mut results = Vec::new();
    for item in items {
        let img = item["url"].as_str().unwrap_or_default();
        let page_url = item["foreign_landing_url"].as_str().unwrap_or(img);
        let title = item["title"].as_str().unwrap_or_default();
        let thumb = item["thumbnail"].as_str().unwrap_or(img);
        if img.is_empty() || title.is_empty() {
            continue;
        }
        // Drop provably icon-sized images (the API reports full dimensions).
        let width = item["width"].as_u64().unwrap_or(0) as u32;
        let height = item["height"].as_u64().unwrap_or(0) as u32;
        if crate::thumbnail::is_tiny_dimension(width, height) {
            continue;
        }
        let creator = item["creator"].as_str().unwrap_or_default();
        let license = item["license"].as_str().unwrap_or_default();
        let mut content = String::new();
        if !creator.is_empty() {
            content.push_str(creator);
        }
        if !license.is_empty() {
            if !content.is_empty() {
                content.push_str(" · ");
            }
            content.push_str("CC ");
            content.push_str(license);
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
            serde_json::from_str(include_str!("../../tests/fixtures/openverse.json")).unwrap();
        let results = parse(&body, 10);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "cat");
        assert!(results[0].url.contains("flickr.com"));
        assert!(results[0]
            .img_src
            .as_ref()
            .unwrap()
            .contains("staticflickr"));
        assert_eq!(results[0].template.as_deref(), Some("images.html"));
        assert!(results[0].content.contains("CC by"));
    }
}
