//! Orgos Core engines - call the local minelite API for indexed search results.
//!
//! This module provides multiple engines for different content types:
//! - orgos_news: News articles
//! - orgos_blog: Blog posts
//! - orgos_youtube: YouTube videos
//! - orgos_knowledge: Knowledge/reference content
//! - orgos_shopping: Shopping content
//! - orgos_sns: Social media content

use serde::Deserialize;

use crate::types::EngineResult;

use super::EngineContext;

#[derive(Debug, Deserialize)]
struct CoreResult {
    title: String,
    url: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    source: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    published_at: i64,
}

/// Search all types (no filter)
pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    search_with_type(ctx, None).await
}

/// Search news only
pub async fn search_news(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    search_with_type(ctx, Some("news")).await
}

/// Search blogs only
pub async fn search_blog(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    search_with_type(ctx, Some("blog")).await
}

/// Search YouTube only
pub async fn search_youtube(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    search_with_type(ctx, Some("youtube")).await
}

/// Search knowledge only
pub async fn search_knowledge(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    search_with_type(ctx, Some("knowledge")).await
}

/// Search shopping only
pub async fn search_shopping(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    search_with_type(ctx, Some("shopping")).await
}

/// Search SNS only
pub async fn search_sns(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    search_with_type(ctx, Some("sns")).await
}

async fn search_with_type(ctx: &EngineContext<'_>, content_type: Option<&str>) -> Result<Vec<EngineResult>, String> {
    let base_url = ctx.base_url.unwrap_or("http://127.0.0.1:8765");

    let query_encoded = percent_encode(ctx.query);
    let type_param = content_type.map(|t| format!("&type={}", t)).unwrap_or_default();
    let lang_code = ctx.lang_code();
    let lang_param = if !lang_code.is_empty() {
        format!("&language={}", lang_code)
    } else {
        String::new()
    };
    let url = format!(
        "{}/api/search?q={}&limit={}{}{}",
        base_url.trim_end_matches('/'),
        query_encoded,
        ctx.max_results.min(20),
        type_param,
        lang_param
    );

    let resp = ctx
        .client
        .get(&url)
        .timeout(ctx.timeout)
        .send()
        .await
        .map_err(|e| format!("orgos request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("orgos returned {}", resp.status()));
    }

    let results: Vec<CoreResult> = resp
        .json()
        .await
        .map_err(|e| format!("orgos parse error: {e}"))?;

    Ok(results
        .into_iter()
        .map(|r| {
            let content = if r.summary.is_empty() {
                format!("{} · {}", r.source, r.kind)
            } else {
                r.summary
            };

            let category = match r.kind.as_str() {
                "News" => "news",
                "YouTube" => "videos",
                _ => "general",
            };

            EngineResult {
                url: r.url,
                title: r.title,
                content,
                img_src: None,
                thumbnail: None,
                published_date: if r.published_at > 0 {
                    Some(format_timestamp(r.published_at))
                } else {
                    None
                },
                template: None,
                category: Some(category.into()),
                priority: None,
                publisher_url: None,
            }
        })
        .collect())
}

fn percent_encode(s: &str) -> String {
    let mut result = String::new();
    for c in s.chars() {
        match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => result.push(c),
            _ => {
                for b in c.to_string().as_bytes() {
                    result.push_str(&format!("%{:02X}", b));
                }
            }
        }
    }
    result
}

fn format_timestamp(ts: i64) -> String {
    if ts <= 0 {
        return String::new();
    }
    let days = ts.div_euclid(86_400);
    let secs = ts.rem_euclid(86_400);
    let (y, mo, d) = civil_from_days(days);
    let hh = secs / 3600;
    let mm = (secs % 3600) / 60;
    let ss = secs % 60;
    format!("{y:04}-{mo:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_core_result() {
        let json = r#"[{
            "title": "Test Article",
            "url": "https://example.com/test",
            "summary": "Test summary",
            "publisher": "example.com",
            "source": "RSS",
            "kind": "News",
            "published_at": 1779918505
        }]"#;
        let results: Vec<CoreResult> = serde_json::from_str(json).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Test Article");
        assert_eq!(results[0].kind, "News");
    }
}
