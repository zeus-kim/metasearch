//! Marginalia search — keyless HTML scrape, OPT-IN and FEATURE-GATED
//! (`--features marginalia`).
//!
//! Marginalia (a small-web / non-commercial search engine) was previously
//! dropped; it is re-attempted here behind a cargo feature. It fails gracefully
//! when the markup drifts or the instance is unreachable. The pure `parse`
//! function (and its fixture test) compile regardless of the feature.

use scraper::{Html, Selector};

use super::{strip_html, EngineContext};
use crate::types::EngineResult;

#[cfg(feature = "marginalia")]
pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    use super::USER_AGENT;
    let profile = "default";
    let resp = ctx
        .client
        .get("https://old-search.marginalia.nu/search")
        .header("User-Agent", USER_AGENT)
        .query(&[("query", ctx.query), ("profile", profile)])
        .timeout(ctx.timeout)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let html = resp.text().await.map_err(|e| super::body_read_error(&e))?;
    let results = parse(&html, ctx.max_results);
    if results.is_empty() {
        return Err("no results".into());
    }
    Ok(results)
}

#[cfg(not(feature = "marginalia"))]
pub async fn search(_ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    Err("marginalia engine disabled at build time (rebuild with --features marginalia)".into())
}

/// Parse a Marginalia results page. Pure for fixture testing.
#[cfg_attr(not(feature = "marginalia"), allow(dead_code))]
pub(crate) fn parse(html: &str, max_results: usize) -> Vec<EngineResult> {
    let doc = Html::parse_document(html);
    let result_sel = Selector::parse("section.card.search-result, div.search-result").unwrap();
    let link_sel = Selector::parse("h2 a, a.title").unwrap();
    let desc_sel = Selector::parse("p.description, .description").unwrap();

    let mut results = Vec::new();
    for node in doc.select(&result_sel) {
        let Some(link) = node.select(&link_sel).next() else {
            continue;
        };
        let Some(href) = link.value().attr("href") else {
            continue;
        };
        if !href.starts_with("http") {
            continue;
        }
        let title = link.text().collect::<String>().trim().to_string();
        if title.is_empty() {
            continue;
        }
        let content = node
            .select(&desc_sel)
            .next()
            .map(|d| strip_html(&d.text().collect::<String>()))
            .unwrap_or_default();
        let mut r = EngineResult::new(href.to_string(), title, content);
        r.category = Some("general".into());
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
        let html = include_str!("../../tests/fixtures/marginalia.html");
        let results = parse(html, 10);
        assert!(!results.is_empty());
        assert_eq!(results[0].title, "The Rust Programming Language");
        assert_eq!(results[0].url, "https://www.rust-lang.org/");
        assert!(results[0].content.contains("systems programming"));
    }
}
