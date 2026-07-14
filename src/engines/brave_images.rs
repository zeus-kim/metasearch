//! Brave Search image results — HTML scrape, keyless.
//!
//! Brave embeds image metadata in the initial HTML payload (Svelte SSR). The
//! parser extracts `thumbnail:{src,original,alt}` blocks. Provides `images`.

use super::{EngineContext, USER_AGENT};
use crate::types::EngineResult;

/// Image grids want density, so fetch/return more per engine than the general
/// per-engine cap.
const IMAGE_TARGET: usize = 30;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let offset = ctx.offset().to_string();
    let mut query = vec![("q", ctx.query)];
    if ctx.offset() > 0 {
        query.push(("offset", offset.as_str()));
    }

    let resp = ctx
        .client
        .get("https://search.brave.com/images")
        .header("User-Agent", USER_AGENT)
        .query(&query)
        .timeout(ctx.timeout)
        .send()
        .await
        .map_err(|e| super::request_error(&e))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let html = resp.text().await.map_err(|e| super::body_read_error(&e))?;
    Ok(parse(&html, ctx.max_results.max(IMAGE_TARGET)))
}

/// Extract a quoted string value for `key:` from a JS-like fragment.
fn js_str_field(fragment: &str, key: &str) -> Option<String> {
    let needle = format!("{key}:\"");
    let start = fragment.find(&needle)? + needle.len();
    let rest = &fragment[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Extract an unquoted integer value for `key:` from a JS-like fragment, e.g.
/// `width:500` inside a `thumbnail:{…}` block.
fn js_num_field(fragment: &str, key: &str) -> Option<u32> {
    let needle = format!("{key}:");
    let start = fragment.find(&needle)? + needle.len();
    let rest = &fragment[start..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<u32>().ok().filter(|n| *n > 0)
}

/// Parse Brave Images SSR HTML. Pure for fixture testing.
pub(crate) fn parse(html: &str, max_results: usize) -> Vec<EngineResult> {
    let mut results = Vec::new();
    for chunk in html.split("thumbnail:{").skip(1) {
        let thumb = js_str_field(chunk, "src").unwrap_or_default();
        let original = js_str_field(chunk, "original").unwrap_or_default();
        let alt = js_str_field(chunk, "alt").unwrap_or_default();
        // Page URL: prefer explicit url/src in the same block, else the original image URL.
        let page_url = js_str_field(chunk, "url")
            .or_else(|| js_str_field(chunk, "page_url"))
            .unwrap_or_else(|| original.clone());
        let img = if original.is_empty() {
            thumb.clone()
        } else {
            original
        };
        let title = if alt.is_empty() {
            page_url.clone()
        } else {
            alt
        };
        if page_url.is_empty() || img.is_empty() || title.is_empty() {
            continue;
        }
        // Brave embeds width/height in the thumbnail block; drop icon-sized hits.
        let width = js_num_field(chunk, "width").unwrap_or(0);
        let height = js_num_field(chunk, "height").unwrap_or(0);
        if crate::thumbnail::is_tiny_dimension(width, height) {
            continue;
        }
        results.push(EngineResult::new(page_url, title, String::new()).image(img, thumb));
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
        let html = include_str!("../../tests/fixtures/brave_images.html");
        let results = parse(html, 10);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Cute cat");
        assert_eq!(results[0].url, "https://example.com/cat-page");
        assert_eq!(
            results[0].img_src.as_deref(),
            Some("https://example.com/cat-full.jpg")
        );
    }
}
