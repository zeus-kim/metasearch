//! Directory of Open Access Journals (DOAJ) article search via the public API
//! (JSON, keyless). `science` category.
//!
//! `https://doaj.org/api/search/articles/{query}` searches peer-reviewed open
//! access articles. Keyless. The `parse` function is pure (fixture-tested).

use serde_json::Value;

use super::{body_error, EngineContext, USER_AGENT};
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let page_size = ctx.max_results.clamp(1, 100).to_string();
    let page = ctx.pageno.max(1).to_string();
    // The query is a path segment; percent-encode it.
    let encoded: String = url::form_urlencoded::byte_serialize(ctx.query.as_bytes()).collect();
    let path = format!("https://doaj.org/api/search/articles/{encoded}");

    let resp = ctx
        .client
        .get(&path)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json")
        .query(&[("pageSize", page_size.as_str()), ("page", page.as_str())])
        .timeout(ctx.timeout)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let body: Value = resp.json().await.map_err(|e| body_error(&e))?;
    Ok(parse(&body))
}

/// Parse a DOAJ `search/articles` response. Pure for fixture testing.
pub(crate) fn parse(body: &Value) -> Vec<EngineResult> {
    let items = match body["results"].as_array() {
        Some(i) => i,
        None => return Vec::new(),
    };
    let mut results = Vec::new();
    for item in items {
        let bib = &item["bibjson"];
        let title = bib["title"].as_str().unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        // Prefer a full-text link; fall back to a DOI.
        let url = bib["link"]
            .as_array()
            .and_then(|links| {
                links
                    .iter()
                    .find(|l| l["type"].as_str() == Some("fulltext"))
                    .or_else(|| links.first())
            })
            .and_then(|l| l["url"].as_str())
            .map(String::from)
            .or_else(|| {
                bib["identifier"].as_array().and_then(|ids| {
                    ids.iter()
                        .find(|i| i["type"].as_str() == Some("doi"))
                        .and_then(|i| i["id"].as_str())
                        .map(|doi| format!("https://doi.org/{doi}"))
                })
            })
            .unwrap_or_default();
        if url.is_empty() {
            continue;
        }
        let authors: Vec<String> = bib["author"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x["name"].as_str().map(String::from))
                    .take(3)
                    .collect()
            })
            .unwrap_or_default();
        let journal = bib["journal"]["title"].as_str().unwrap_or("");
        let year = bib["year"].as_str().unwrap_or("");
        let mut content = String::new();
        if !authors.is_empty() {
            content.push_str(&authors.join(", "));
        }
        for part in [journal, year] {
            if !part.is_empty() {
                if !content.is_empty() {
                    content.push_str(" · ");
                }
                content.push_str(part);
            }
        }
        let mut r = EngineResult::new(url, title, content);
        r.category = Some("science".into());
        if !year.is_empty() {
            r.published_date = Some(year.to_string());
        }
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
            serde_json::from_str(include_str!("../../tests/fixtures/doaj.json")).unwrap();
        let results = parse(&body);
        assert!(!results.is_empty());
        assert!(results[0].title.contains("Open Access"));
        assert!(results[0].url.starts_with("http"));
        assert_eq!(results[0].category.as_deref(), Some("science"));
        assert!(results[0].content.contains("2021"));
    }
}
