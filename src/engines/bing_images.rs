//! Bing image search via the keyless `/images/async` HTML endpoint.
//!
//! Parses embedded JSON metadata from `a.iusc` elements (same approach as
//! the standard `bing_images` engine). Provides the `images` category.

use scraper::{Html, Selector};
use serde_json::Value;

use super::{body_read_error, request_error, EngineContext, USER_AGENT};
use crate::types::EngineResult;

/// Image grids want density, so request a full Bing page regardless of the
/// general per-engine cap.
const IMAGE_TARGET: usize = 35;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let first = ((ctx.pageno.max(1) - 1) * IMAGE_TARGET + 1).to_string();
    let count = ctx.max_results.clamp(1, 50).max(IMAGE_TARGET).to_string();

    let resp = ctx
        .client
        .get("https://www.bing.com/images/async")
        .header("User-Agent", USER_AGENT)
        .query(&[
            ("q", ctx.query),
            ("async", "1"),
            ("first", first.as_str()),
            ("count", count.as_str()),
        ])
        .timeout(ctx.timeout)
        .send()
        .await
        .map_err(|e| request_error(&e))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let html = resp.text().await.map_err(|e| body_read_error(&e))?;
    Ok(parse(&html, ctx.max_results.max(IMAGE_TARGET)))
}

/// Read an integer query param (e.g. `expw=640`) from a Bing detail href.
fn href_dim(href: &str, key: &str) -> u32 {
    let needle = format!("{key}=");
    let Some(idx) = href.find(&needle) else {
        return 0;
    };
    href[idx + needle.len()..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap_or(0)
}

/// Parse Bing Images async HTML. Pure for fixture testing.
pub(crate) fn parse(html: &str, max_results: usize) -> Vec<EngineResult> {
    let doc = Html::parse_document(html);
    // Match every image link, not just those nested in `ul.dgControl_list li`:
    // Bing's async payload shape drifts and real hits also live in `.imgpt`.
    let link_sel = Selector::parse("a.iusc").unwrap();

    let mut results = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for link in doc.select(&link_sel) {
        let Some(raw_meta) = link.value().attr("m") else {
            continue;
        };
        let meta: Value = match serde_json::from_str(raw_meta) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let page_url = meta["purl"].as_str().unwrap_or_default();
        let img = meta["murl"].as_str().unwrap_or_default();
        let thumb = meta["turl"].as_str().unwrap_or(img);
        let title = meta["t"]
            .as_str()
            .or_else(|| meta["desc"].as_str())
            .unwrap_or_default();
        if page_url.is_empty() || img.is_empty() || title.is_empty() {
            continue;
        }
        // Bing exposes the full-image dimensions in the detail href
        // (`expw`/`exph`); drop icon-sized results when known.
        let href = link.value().attr("href").unwrap_or_default();
        let width = href_dim(href, "expw");
        let height = href_dim(href, "exph");
        if crate::thumbnail::is_tiny_dimension(width, height) {
            continue;
        }
        if !seen.insert(img.to_string()) {
            continue;
        }
        let content = meta["desc"].as_str().unwrap_or_default().to_string();
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
        let html = include_str!("../../tests/fixtures/bing_images.html");
        let results = parse(html, 10);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Cute cat portrait");
        assert_eq!(results[0].url, "https://example.com/cat-article");
        assert_eq!(
            results[0].img_src.as_deref(),
            Some("https://example.com/cat-full.jpg")
        );
        assert_eq!(results[0].template.as_deref(), Some("images.html"));
    }
}
