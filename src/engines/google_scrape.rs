//! Google search scraper - searches Google like a regular user.
//! May get blocked with heavy use. For personal use only.

use scraper::{Html, Selector};
use url::form_urlencoded;
use super::{EngineContext, USER_AGENT};
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let encoded_query: String = form_urlencoded::byte_serialize(ctx.query.as_bytes()).collect();
    let url = format!(
        "https://www.google.com/search?q={}&hl=ko&num={}",
        encoded_query,
        ctx.max_results.min(20)
    );
    
    let resp = ctx.client
        .get(&url)
        .header("User-Agent", USER_AGENT)
        .header("Accept-Language", "ko-KR,ko;q=0.9,en;q=0.8")
        .timeout(ctx.timeout)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    
    let html = resp.text().await.map_err(|e| format!("body read: {e}"))?;
    
    if html.contains("detected unusual traffic") || html.contains("CAPTCHA") {
        return Err("blocked by Google (CAPTCHA)".to_string());
    }
    
    Ok(parse_google_results(&html, ctx.max_results))
}

fn parse_google_results(html: &str, max_results: usize) -> Vec<EngineResult> {
    let doc = Html::parse_document(html);
    let mut results = Vec::new();
    let mut seen = std::collections::HashSet::new();
    
    // Try multiple selectors for Google's varying HTML structure
    let selectors = [
        "div.g a[href^='http']",
        "a[href^='/url?q=']",
        "div[data-hveid] a[href^='http']",
    ];
    
    for sel_str in selectors {
        if let Ok(sel) = Selector::parse(sel_str) {
            for link in doc.select(&sel) {
                let href = match link.value().attr("href") {
                    Some(h) => h,
                    None => continue,
                };
                
                // Extract real URL from Google redirect
                let real_url = if href.starts_with("/url?q=") {
                    if let Some(qs) = href.strip_prefix("/url?") {
                        form_urlencoded::parse(qs.as_bytes())
                            .find(|(k, _)| k == "q")
                            .map(|(_, v)| v.to_string())
                            .unwrap_or_else(|| href.to_string())
                    } else {
                        href.to_string()
                    }
                } else {
                    href.to_string()
                };
                
                // Skip Google internal links
                if real_url.contains("google.com") || 
                   real_url.contains("youtube.com/results") ||
                   real_url.starts_with("#") {
                    continue;
                }
                
                if seen.contains(&real_url) {
                    continue;
                }
                seen.insert(real_url.clone());
                
                let title = link.text().collect::<String>().trim().to_string();
                if title.is_empty() || title.len() < 3 {
                    continue;
                }
                
                results.push(EngineResult::new(real_url, title, String::new()));
                
                if results.len() >= max_results {
                    return results;
                }
            }
        }
    }
    
    results
}
