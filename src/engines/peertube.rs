//! PeerTube federated video search via SepiaSearch (JSON, keyless). `videos`.
//!
//! SepiaSearch indexes public videos across the PeerTube network and exposes a
//! PeerTube-compatible REST API. Thumbnails can be proxied via `/image_proxy`.

use serde_json::Value;

use super::{strip_html, EngineContext, USER_AGENT};
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let count = ctx.max_results.clamp(1, 50).to_string();
    let start = ctx.offset().to_string();
    let mut query = vec![
        ("search", ctx.query),
        ("count", count.as_str()),
        ("start", start.as_str()),
        ("sort", "-match"),
    ];
    // SepiaSearch maps `nsfw=false` to safe results.
    if ctx.safe_search >= 1 {
        query.push(("nsfw", "false"));
    }

    let resp = ctx
        .client
        .get("https://sepiasearch.org/api/v1/search/videos")
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json")
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

/// Format a duration in seconds as `H:MM:SS` / `M:SS`.
fn fmt_duration(secs: i64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Parse a SepiaSearch / PeerTube video search response. Pure for testing.
pub(crate) fn parse(body: &Value) -> Vec<EngineResult> {
    let items = match body["data"].as_array() {
        Some(i) => i,
        None => return Vec::new(),
    };
    let mut results = Vec::new();
    for item in items {
        let name = item["name"].as_str().unwrap_or_default();
        let url = item["url"].as_str().unwrap_or_default();
        if name.is_empty() || url.is_empty() {
            continue;
        }
        let channel = item["channel"]["displayName"]
            .as_str()
            .or_else(|| item["account"]["displayName"].as_str())
            .unwrap_or("");
        let views = item["views"].as_i64().unwrap_or(0);
        let mut meta = String::new();
        if !channel.is_empty() {
            meta.push_str(channel);
        }
        if let Some(dur) = item["duration"].as_i64() {
            if !meta.is_empty() {
                meta.push_str(" · ");
            }
            meta.push_str(&fmt_duration(dur));
        }
        if views > 0 {
            meta.push_str(&format!(" · {views} views"));
        }
        let desc = item["description"].as_str().unwrap_or_default();
        let content = if desc.is_empty() {
            meta
        } else {
            let snippet: String = strip_html(desc).chars().take(160).collect();
            format!("{meta} — {}", snippet.trim())
        };
        let mut r = EngineResult::new(url, name, content);
        let thumb = item["thumbnailUrl"].as_str().unwrap_or_default();
        if !thumb.is_empty() {
            r.thumbnail = Some(thumb.to_string());
            r.img_src = Some(thumb.to_string());
        }
        r.template = Some("videos.html".into());
        r.category = Some("videos".into());
        r.published_date = item["publishedAt"].as_str().map(String::from);
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
            serde_json::from_str(include_str!("../../tests/fixtures/peertube.json")).unwrap();
        let results = parse(&body);
        assert!(!results.is_empty());
        assert!(results[0].title.contains("Linux"));
        assert!(results[0].url.starts_with("https://"));
        assert_eq!(results[0].template.as_deref(), Some("videos.html"));
        assert!(results[0].thumbnail.is_some());
        assert!(results[0].content.contains("views"));
    }
}
