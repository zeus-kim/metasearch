//! Daum search scraper - searches Daum like a regular user.
//! No API key needed, just scrapes the search results page.

use scraper::{Html, Selector};
use url::form_urlencoded;
use super::{EngineContext, USER_AGENT};
use crate::types::EngineResult;

/// Daum web search (scraping)
pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let encoded_query: String = form_urlencoded::byte_serialize(ctx.query.as_bytes()).collect();
    let url = format!(
        "https://search.daum.net/search?w=web&q={}",
        encoded_query
    );
    
    let resp = ctx.client
        .get(&url)
        .header("User-Agent", USER_AGENT)
        .timeout(ctx.timeout)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    
    let html = resp.text().await.map_err(|e| format!("body read: {e}"))?;
    Ok(parse_daum_results(&html, ctx.query, ctx.max_results))
}

fn parse_daum_results(html: &str, query: &str, max_results: usize) -> Vec<EngineResult> {
    let doc = Html::parse_document(html);
    let link_sel = Selector::parse("a[href]").unwrap();
    
    let mut results = Vec::new();
    let mut seen_urls = std::collections::HashSet::new();
    
    let query_first_word = query.split_whitespace().next().unwrap_or(query);
    
    for link in doc.select(&link_sel) {
        let href = match link.value().attr("href") {
            Some(h) => h,
            None => continue,
        };
        
        // Skip internal Daum links
        if href.contains("search.daum.net") || 
           href.contains("ad.search.daum.net") ||
           href.starts_with("#") ||
           href.starts_with("javascript:") ||
           href.contains("login.daum.net") {
            continue;
        }
        
        let title = link.text().collect::<String>().trim().to_string();
        
        // Only include links that mention our query
        if title.is_empty() || title.len() < 5 {
            continue;
        }
        
        if !title.contains(query_first_word) && !href.contains(query_first_word) {
            continue;
        }
        
        // Skip duplicates
        if seen_urls.contains(href) {
            continue;
        }
        seen_urls.insert(href.to_string());
        
        // Determine result type from URL
        let content = if href.contains("blog.daum.net") || href.contains("tistory.com") {
            "다음 블로그".to_string()
        } else if href.contains("cafe.daum.net") {
            "다음 카페".to_string()
        } else if href.contains("news.") || href.contains("/news/") {
            "뉴스".to_string()
        } else if href.contains("map.kakao.com") {
            "카카오맵 - 장소 정보".to_string()
        } else {
            String::new()
        };
        
        results.push(EngineResult::new(href.to_string(), title, content));
        
        if results.len() >= max_results {
            break;
        }
    }
    
    results
}
