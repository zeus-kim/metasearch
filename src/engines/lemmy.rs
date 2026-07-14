//! Lemmy (federated link aggregator) post search via the public API v3 (JSON,
//! keyless). `social` category. Defaults to the large `lemmy.world` instance.

use serde_json::Value;

use super::{looks_like_image_url, strip_html, EngineContext, USER_AGENT};
use crate::thumbnail::is_usable_thumbnail_url;
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    // Allow overriding the instance via `base_url`; default to lemmy.world.
    let base = ctx
        .base_url
        .map(|b| b.trim_end_matches('/').to_string())
        .unwrap_or_else(|| "https://lemmy.world".to_string());
    let url = format!("{base}/api/v3/search");
    let limit = ctx.max_results.clamp(1, 50).to_string();
    let page = ctx.pageno.max(1).to_string();

    let resp = ctx
        .client
        .get(&url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json")
        .query(&[
            ("q", ctx.query),
            ("type_", "Posts"),
            ("sort", "TopAll"),
            ("limit", limit.as_str()),
            ("page", page.as_str()),
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

/// Parse a Lemmy `search` response (Posts). Pure for fixture testing.
pub(crate) fn parse(body: &Value) -> Vec<EngineResult> {
    let items = match body["posts"].as_array() {
        Some(i) => i,
        None => return Vec::new(),
    };
    let mut results = Vec::new();
    for item in items {
        let post = &item["post"];
        let title = post["name"].as_str().unwrap_or_default();
        // Prefer the canonical Lemmy permalink (`ap_id`), falling back to the
        // linked URL.
        let link_url = post["url"].as_str().filter(|s| !s.is_empty());
        let url = post["ap_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or(link_url)
            .unwrap_or_default();
        if title.is_empty() || url.is_empty() {
            continue;
        }
        let community = item["community"]["name"].as_str().unwrap_or("");
        let score = item["counts"]["score"].as_i64().unwrap_or(0);
        let comments = item["counts"]["comments"].as_i64().unwrap_or(0);
        let mut content = String::new();
        if !community.is_empty() {
            content.push_str(&format!("c/{community} · "));
        }
        content.push_str(&format!("{score} points · {comments} comments"));
        if let Some(body_text) = post["body"].as_str().filter(|s| !s.is_empty()) {
            let snippet = strip_html(body_text);
            let snippet: String = snippet.chars().take(160).collect();
            if !snippet.trim().is_empty() {
                content.push_str(" — ");
                content.push_str(snippet.trim());
            }
        }
        let mut r = EngineResult::new(url.to_string(), title, content);
        if let Some(u) = link_url.filter(|u| looks_like_image_url(u) && is_usable_thumbnail_url(u))
        {
            r.thumbnail = Some(u.to_string());
            r.img_src = Some(u.to_string());
        }
        r.category = Some("social".into());
        r.published_date = item["counts"]["published"]
            .as_str()
            .or_else(|| post["published"].as_str())
            .map(String::from);
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
            serde_json::from_str(include_str!("../../tests/fixtures/lemmy.json")).unwrap();
        let results = parse(&body);
        assert!(!results.is_empty());
        assert_eq!(results[0].title, "Why privacy matters in 2026");
        assert!(results[0].url.contains("lemmy.world"));
        assert!(results[0].content.contains("points"));
        assert!(results[0].content.contains("c/privacy"));
        assert!(results[0].thumbnail.is_none());
    }
}
