//! Brave Search general web engine — HTML scrape, keyless.
//!
//! Brave frequently changes its markup; the paired fixture test guards the
//! selectors so breakage is caught by CI rather than silently in production.

use scraper::{Html, Selector};

use super::{strip_html, EngineContext, USER_AGENT};
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let offset = ctx.pageno.saturating_sub(1).to_string();
    let mut query = vec![("q", ctx.query), ("source", "web")];
    if ctx.pageno > 1 {
        query.push(("offset", offset.as_str()));
    }
    if ctx.safe_search >= 2 {
        query.push(("safesearch", "strict"));
    } else if ctx.safe_search == 1 {
        query.push(("safesearch", "moderate"));
    }

    let resp = ctx
        .client
        .get("https://search.brave.com/search")
        .header("User-Agent", USER_AGENT)
        // Brave honours Accept-Language for result language/region; the keyless
        // scraper has no query param for it, so this is how we localize it.
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
    Ok(parse(&html, ctx.max_results))
}

/// Parse Brave results HTML. Pure for fixture testing.
pub(crate) fn parse(html: &str, max_results: usize) -> Vec<EngineResult> {
    let doc = Html::parse_document(html);
    let snippet_sel = Selector::parse("div.snippet[data-type=web]").unwrap();
    let link_sel = Selector::parse("a").unwrap();
    let title_sel = Selector::parse(".title").unwrap();
    let desc_sel = Selector::parse(".snippet-description").unwrap();

    let mut results = Vec::new();
    for snip in doc.select(&snippet_sel) {
        let Some(link) = snip.select(&link_sel).next() else {
            continue;
        };
        let Some(href) = link.value().attr("href") else {
            continue;
        };
        if !href.starts_with("http") {
            continue;
        }
        let title = snip
            .select(&title_sel)
            .next()
            .map(|t| t.text().collect::<String>().trim().to_string())
            .unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        let content = snip
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
        let html = include_str!("../../tests/fixtures/brave.html");
        let results = parse(html, 10);
        assert!(!results.is_empty());
        assert_eq!(results[0].title, "Rust Programming Language");
        assert_eq!(results[0].url, "https://www.rust-lang.org/");
        assert!(results[0].content.contains("memory-safe"));
    }
}
