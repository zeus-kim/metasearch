//! DuckDuckGo engines.
//!
//! * [`search_instant`] — the keyless Instant Answer JSON API.
//! * [`search_lite`] — the `lite.duckduckgo.com` HTML endpoint, parsed with
//!   `scraper` (the closest analogue to the standard lxml-based DDG engine).
//! * [`autocomplete`] — the keyless `duckduckgo.com/ac/` suggestion endpoint.

use std::time::Duration;

use scraper::{Html, Selector};
use serde_json::Value;
use url::Url;

use super::{ddg_region, ddg_safe, EngineContext, EngineResponse, USER_AGENT};
use crate::types::{EngineResult, Infobox};

/// DuckDuckGo Instant Answer API (https://api.duckduckgo.com). Keyless JSON.
///
/// Related topics become normal results; the abstract becomes an infobox
/// (following standard pattern, which routes the DDG abstract into `infoboxes`).
pub async fn search_instant(ctx: &EngineContext<'_>) -> Result<EngineResponse, String> {
    let kl = ddg_region(ctx.lang);
    let resp = ctx
        .client
        .get("https://api.duckduckgo.com/")
        .header("User-Agent", USER_AGENT)
        .query(&[
            ("q", ctx.query),
            ("format", "json"),
            ("no_html", "1"),
            ("no_redirect", "1"),
            ("kl", kl.as_str()),
            ("t", "metasearch"),
        ])
        .timeout(ctx.timeout)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let body: Value = resp.json().await.map_err(|e| super::body_error(&e))?;
    let mut out = EngineResponse::from(parse_instant(&body, ctx.max_results));
    if let Some((title, text, url, image)) = instant_infobox(&body) {
        let mut ib = Infobox::new(title, text, "duckduckgo");
        ib.id = url.clone();
        ib.urls.push(crate::types::InfoboxUrl {
            title: "DuckDuckGo".into(),
            url,
        });
        if let Some(img) = image {
            ib.img_src = img;
        }
        out.infoboxes.push(ib);
    }
    Ok(out)
}

/// Parse the Instant Answer JSON into related-topic results. Pure for testing.
///
/// Note: the *abstract* (instant answer) is intentionally NOT returned here as a
/// normal web result — the orchestrator pulls it out via [`instant_infobox`] so
/// it lands in `infoboxes`, following standard pattern semantics.
pub(crate) fn parse_instant(body: &Value, max_results: usize) -> Vec<EngineResult> {
    let mut results = Vec::new();
    if let Some(topics) = body["RelatedTopics"].as_array() {
        for topic in topics {
            if let Some(nested) = topic["Topics"].as_array() {
                for t in nested {
                    push_topic(&mut results, t);
                }
            } else {
                push_topic(&mut results, topic);
            }
            if results.len() >= max_results {
                break;
            }
        }
    }
    results.truncate(max_results);
    results
}

/// Extract the DDG abstract as an infobox payload `(title, text, url, image)`.
pub(crate) fn instant_infobox(body: &Value) -> Option<(String, String, String, Option<String>)> {
    let abstract_text = body["AbstractText"].as_str().unwrap_or_default();
    let abstract_url = body["AbstractURL"].as_str().unwrap_or_default();
    if abstract_text.is_empty() || abstract_url.is_empty() {
        return None;
    }
    let heading = body["Heading"]
        .as_str()
        .unwrap_or(abstract_text)
        .to_string();
    let image = body["Image"].as_str().filter(|s| !s.is_empty()).map(|s| {
        if s.starts_with("http") {
            s.to_string()
        } else {
            format!("https://duckduckgo.com{s}")
        }
    });
    Some((
        heading,
        abstract_text.to_string(),
        abstract_url.to_string(),
        image,
    ))
}

fn push_topic(out: &mut Vec<EngineResult>, topic: &Value) {
    let url = topic["FirstURL"].as_str().unwrap_or_default();
    let text = topic["Text"].as_str().unwrap_or_default();
    if url.is_empty() || text.is_empty() {
        return;
    }
    let (title, content) = match text.split_once(" - ") {
        Some((t, c)) => (t.to_string(), c.to_string()),
        None => (text.to_string(), text.to_string()),
    };
    out.push(EngineResult::new(url, title, content));
}

/// DuckDuckGo "lite" HTML endpoint. Returns real web results without an API key.
pub async fn search_lite(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let offset = ctx.offset().to_string();
    let kp = ddg_safe(ctx.safe_search);
    // Region/language: derive DuckDuckGo's `kl` from the resolved locale so a
    // Korean query asks for Korea-region results (`kr-kr`) instead of the old
    // hardcoded worldwide `wt-wt`.
    let kl = ddg_region(ctx.lang);
    let mut form: Vec<(&str, &str)> = vec![("q", ctx.query), ("kl", kl.as_str()), ("kp", kp)];
    if ctx.offset() > 0 {
        form.push(("s", &offset));
        form.push(("dc", &offset));
    }
    if let Some(tr) = ctx.time_range {
        if let Some(df) = ddg_time_range(tr) {
            form.push(("df", df));
        }
    }

    let resp = ctx
        .client
        .post("https://lite.duckduckgo.com/lite/")
        .header("User-Agent", USER_AGENT)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&form)
        .timeout(ctx.timeout)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let html = resp.text().await.map_err(|e| super::body_read_error(&e))?;
    Ok(parse_lite(&html, ctx.max_results))
}

fn ddg_time_range(tr: &str) -> Option<&'static str> {
    Some(match tr {
        "day" => "d",
        "week" => "w",
        "month" => "m",
        "year" => "y",
        _ => return None,
    })
}

/// Parse the lite HTML into ordered results. Kept as a pure function so it can
/// be unit-tested against a captured fixture without the network.
pub(crate) fn parse_lite(html: &str, max_results: usize) -> Vec<EngineResult> {
    let doc = Html::parse_document(html);
    let link_sel = Selector::parse("a.result-link").unwrap();
    let snippet_sel = Selector::parse("td.result-snippet").unwrap();

    let links: Vec<_> = doc.select(&link_sel).collect();
    let snippets: Vec<String> = doc
        .select(&snippet_sel)
        .map(|s| {
            s.text()
                .collect::<String>()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();

    let mut results = Vec::new();
    for (i, link) in links.iter().enumerate() {
        let href = match link.value().attr("href") {
            Some(h) => resolve_ddg_href(h),
            None => continue,
        };
        let title = link
            .text()
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if href.is_empty() || title.is_empty() {
            continue;
        }
        let content = snippets.get(i).cloned().unwrap_or_default();
        results.push(EngineResult::new(href, title, content));
        if results.len() >= max_results {
            break;
        }
    }
    results
}

/// DDG often wraps targets in a redirect: `//duckduckgo.com/l/?uddg=<encoded>`.
fn resolve_ddg_href(href: &str) -> String {
    let normalized = if href.starts_with("//") {
        format!("https:{href}")
    } else {
        href.to_string()
    };
    if let Ok(parsed) = Url::parse(&normalized) {
        if parsed.path().starts_with("/l/") {
            if let Some((_, target)) = parsed.query_pairs().find(|(k, _)| k == "uddg") {
                return target.into_owned();
            }
        }
    }
    normalized
}

/// DuckDuckGo autocomplete suggestions (keyless JSON).
pub async fn autocomplete(client: &reqwest::Client, query: &str, timeout: Duration) -> Vec<String> {
    let resp = client
        .get("https://duckduckgo.com/ac/")
        .header("User-Agent", USER_AGENT)
        .query(&[("q", query), ("type", "list")])
        .timeout(timeout)
        .send()
        .await;
    let Ok(resp) = resp else { return Vec::new() };
    let Ok(body) = resp.json::<Value>().await else {
        return Vec::new();
    };
    parse_autocomplete(&body)
}

/// Parse DDG autocomplete JSON (handles both the `[{phrase}]` and `[q,[...]]`
/// shapes). Pure for testing.
pub(crate) fn parse_autocomplete(body: &Value) -> Vec<String> {
    // type=list shape: ["query", ["s1", "s2", ...]]
    if let Some(list) = body.get(1).and_then(|v| v.as_array()) {
        return list
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
    }
    // object-array shape: [{"phrase": "..."}, ...]
    if let Some(arr) = body.as_array() {
        return arr
            .iter()
            .filter_map(|v| v["phrase"].as_str().map(String::from))
            .collect();
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lite_fixture() {
        let html = include_str!("../../tests/fixtures/ddg_lite.html");
        let results = parse_lite(html, 10);
        assert!(results.len() >= 2);
        assert_eq!(results[0].title, "Example Domain");
        assert_eq!(results[0].url, "https://example.com/");
        assert!(results[0].content.contains("illustrative"));
        // The redirect-wrapped second link should be unwrapped.
        assert_eq!(results[1].url, "https://www.rust-lang.org/");
    }

    #[test]
    fn parses_instant_fixture() {
        let body: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/ddg_instant.json")).unwrap();
        let topics = parse_instant(&body, 10);
        assert!(!topics.is_empty());
        let (title, text, url, _img) = instant_infobox(&body).unwrap();
        assert_eq!(title, "Rust");
        assert!(text.contains("programming language"));
        assert!(url.contains("wikipedia"));
    }

    #[test]
    fn parses_autocomplete_list() {
        let body: Value = serde_json::json!(["rust", ["rust lang", "rust book"]]);
        assert_eq!(parse_autocomplete(&body), vec!["rust lang", "rust book"]);
    }
}
