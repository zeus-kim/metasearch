//! Bing search scraper - searches Bing like a regular user.
//! May get blocked with heavy use. For personal use only.

use scraper::{Html, Selector};
use url::form_urlencoded;
use super::{EngineContext, USER_AGENT};
use crate::types::EngineResult;

pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let encoded_query: String = form_urlencoded::byte_serialize(ctx.query.as_bytes()).collect();
    let url = format!(
        "https://www.bing.com/search?q={}&setlang=ko&count={}",
        encoded_query,
        ctx.max_results.min(30)
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
    Ok(parse_bing_results(&html, ctx.max_results))
}

fn parse_bing_results(html: &str, max_results: usize) -> Vec<EngineResult> {
    let doc = Html::parse_document(html);
    let mut results = Vec::new();
    let mut seen = std::collections::HashSet::new();
    
    // Bing search result selectors
    let title_sel = Selector::parse("li.b_algo h2 a").ok();
    let snippet_sel = Selector::parse("li.b_algo .b_caption p").ok();
    
    if let Some(sel) = title_sel {
        let snippets: Vec<String> = snippet_sel
            .map(|s| doc.select(&s).map(|el| el.text().collect::<String>()).collect())
            .unwrap_or_default();
        
        for (i, link) in doc.select(&sel).enumerate() {
            let href = match link.value().attr("href") {
                Some(h) => h.to_string(),
                None => continue,
            };
            
            // Skip Bing internal links
            if href.contains("bing.com") || href.starts_with("#") {
                continue;
            }
            
            if seen.contains(&href) {
                continue;
            }
            seen.insert(href.clone());
            
            let title = link.text().collect::<String>().trim().to_string();
            if title.is_empty() {
                continue;
            }
            
            let content = snippets.get(i).cloned().unwrap_or_default();
            
            results.push(EngineResult::new(href, title, content));
            
            if results.len() >= max_results {
                break;
            }
        }
    }
    
    results
}
