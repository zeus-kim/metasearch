//! Hacker News engine via the Algolia HN Search API (JSON, keyless).

use serde_json::Value;

use super::{looks_like_image_url, EngineContext, USER_AGENT};
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let per_page = ctx.max_results.to_string();
    let page = ctx.pageno.saturating_sub(1).to_string();
    let mut query = vec![
        ("query", ctx.query),
        ("tags", "story"),
        ("hitsPerPage", per_page.as_str()),
        ("page", page.as_str()),
    ];
    let numeric;
    if let Some(filter) = time_range_filter(ctx.time_range) {
        numeric = filter;
        query.push(("numericFilters", &numeric));
    }

    let resp = ctx
        .client
        .get("https://hn.algolia.com/api/v1/search")
        .header("User-Agent", USER_AGENT)
        .query(&query)
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

fn time_range_filter(tr: Option<&str>) -> Option<String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    let window = match tr? {
        "day" => 86_400,
        "week" => 604_800,
        "month" => 2_592_000,
        "year" => 31_536_000,
        _ => return None,
    };
    Some(format!("created_at_i>{}", now - window))
}

/// Parse an Algolia HN search response. Pure for fixture testing.
pub(crate) fn parse(body: &Value) -> Vec<EngineResult> {
    let hits = match body["hits"].as_array() {
        Some(h) => h,
        None => return Vec::new(),
    };
    let mut results = Vec::new();
    for hit in hits {
        let object_id = hit["objectID"].as_str().unwrap_or_default();
        let title = hit["title"]
            .as_str()
            .or_else(|| hit["story_title"].as_str())
            .unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        let url = match hit["url"].as_str() {
            Some(u) if !u.is_empty() => u.to_string(),
            _ => format!("https://news.ycombinator.com/item?id={object_id}"),
        };
        let points = hit["points"].as_i64().unwrap_or(0);
        let author = hit["author"].as_str().unwrap_or("unknown");
        let comments = hit["num_comments"].as_i64().unwrap_or(0);
        let content = format!("{points} points by {author} · {comments} comments");

        let mut r = EngineResult::new(url.clone(), title, content);
        r.published_date = hit["created_at"].as_str().map(|s| s.to_string());
        r.category = Some("news".into());
        if looks_like_image_url(&url) {
            r.thumbnail = Some(url.clone());
            r.img_src = Some(url);
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
            serde_json::from_str(include_str!("../../tests/fixtures/hackernews.json")).unwrap();
        let results = parse(&body);
        assert!(!results.is_empty());
        assert_eq!(
            results[0].title,
            "Show HN: A privacy-respecting metasearch engine"
        );
        assert!(results[0].content.contains("points"));
        assert!(results[0].published_date.is_some());
    }
}
