//! Startpage general web engine — keyless HTML scrape, OPT-IN.
//!
//! Disabled by default: Startpage aggressively bot-blocks datacenter IPs and
//! often requires session tokens, so live success is best-effort. The selectors
//! are guarded by a fixture test so drift is caught in CI, and the engine fails
//! gracefully (recoverable error → marked unresponsive) when blocked.

use scraper::{Html, Selector};

use super::{strip_html, EngineContext, USER_AGENT};
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let page = ctx.pageno.max(1).to_string();
    let mut query = vec![("query", ctx.query), ("page", page.as_str())];
    if ctx.safe_search >= 2 {
        query.push(("qadf", "heavy"));
    }

    let resp = ctx
        .client
        .get("https://www.startpage.com/sp/search")
        .header("User-Agent", USER_AGENT)
        .header("Accept-Language", ctx.lang)
        .query(&query)
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
        // Empty usually means a bot-block / token-challenge page.
        return Err("no results (possible bot-block)".into());
    }
    Ok(results)
}

/// Parse a Startpage results page. Pure for fixture testing.
pub(crate) fn parse(html: &str, max_results: usize) -> Vec<EngineResult> {
    let doc = Html::parse_document(html);
    let result_sel = Selector::parse("div.w-gl__result").unwrap();
    let link_sel = Selector::parse("a.w-gl__result-title, a.result-link").unwrap();
    let title_sel = Selector::parse("h3, .w-gl__result-title").unwrap();
    let desc_sel = Selector::parse("p.w-gl__description").unwrap();

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
        let title = node
            .select(&title_sel)
            .next()
            .map(|t| t.text().collect::<String>().trim().to_string())
            .unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        let content = node
            .select(&desc_sel)
            .next()
            .map(|d| strip_html(&d.text().collect::<String>()))
            .unwrap_or_default();
        results.push(EngineResult::new(href.to_string(), title, content));
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
        let html = include_str!("../../tests/fixtures/startpage.html");
        let results = parse(html, 10);
        assert!(!results.is_empty());
        assert_eq!(results[0].title, "Rust Programming Language");
        assert_eq!(results[0].url, "https://www.rust-lang.org/");
        assert!(results[0].content.contains("memory-safe"));
    }
}
