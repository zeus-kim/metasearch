//! Mojeek general web engine (independent crawler) — HTML scrape, keyless.

use scraper::{Html, Selector};

use super::{EngineContext, USER_AGENT};
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let offset = ctx.offset();
    let s = (offset + 1).to_string(); // Mojeek `s` is 1-indexed result start
    let mut query = vec![("q", ctx.query)];
    if offset > 0 {
        query.push(("s", s.as_str()));
    }
    // Mojeek SafeSearch: `safe=1` strict.
    if ctx.safe_search >= 1 {
        query.push(("safe", "1"));
    }
    // Mojeek's `lb` parameter biases results toward a language (best-effort).
    // Skip it for English/`all` (Mojeek's index is English-leaning by default).
    let lang = ctx.lang_code();
    if !lang.is_empty() && lang != "en" && lang != "all" {
        query.push(("lb", lang));
    }

    let resp = ctx
        .client
        .get("https://www.mojeek.com/search")
        .header("User-Agent", USER_AGENT)
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

/// Parse Mojeek results HTML. Pure for fixture testing.
pub(crate) fn parse(html: &str, max_results: usize) -> Vec<EngineResult> {
    let doc = Html::parse_document(html);
    let li_sel = Selector::parse("ul.results-standard li").unwrap();
    let title_sel = Selector::parse("a.title").unwrap();
    let snippet_sel = Selector::parse("p.s").unwrap();

    let mut results = Vec::new();
    for li in doc.select(&li_sel) {
        let Some(link) = li.select(&title_sel).next() else {
            continue;
        };
        let Some(href) = link.value().attr("href") else {
            continue;
        };
        let title = link.text().collect::<String>().trim().to_string();
        if href.is_empty() || title.is_empty() {
            continue;
        }
        let content = li
            .select(&snippet_sel)
            .next()
            .map(|s| {
                s.text()
                    .collect::<String>()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            })
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
        let html = include_str!("../../tests/fixtures/mojeek.html");
        let results = parse(html, 10);
        assert!(results.len() >= 2);
        assert_eq!(results[0].title, "Rust Programming Language");
        assert_eq!(results[0].url, "https://www.rust-lang.org/");
        assert!(results[0].content.contains("reliable"));
    }
}
