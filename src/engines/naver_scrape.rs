//! Naver search scraper - searches Naver like a regular user.
//! No API key needed, just scrapes the search results page.

use scraper::{Html, Selector, ElementRef};
use url::form_urlencoded;
use super::{EngineContext, USER_AGENT};
use crate::types::EngineResult;

/// Naver web search (scraping)
pub async fn search(ctx: &EngineContext<'_>) -> Result<Vec<EngineResult>, String> {
    let encoded_query: String = form_urlencoded::byte_serialize(ctx.query.as_bytes()).collect();
    let url = format!(
        "https://search.naver.com/search.naver?where=nexearch&query={}",
        encoded_query
    );

    let resp = ctx.client
        .get(&url)
        .header("User-Agent", USER_AGENT)
        .header("Accept-Language", "ko-KR,ko;q=0.9")
        .timeout(ctx.timeout)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let html = resp.text().await.map_err(|e| format!("body read: {e}"))?;
    Ok(parse_naver_results(&html, ctx.max_results))
}

fn parse_naver_results(html: &str, max_results: usize) -> Vec<EngineResult> {
    let doc = Html::parse_document(html);
    let mut results = Vec::new();
    let mut seen_urls = std::collections::HashSet::new();

    // Try multiple result container selectors (Naver changes HTML frequently)
    // Updated for 2024-2026 Naver structure
    let container_selectors = [
        // Modern web results (main search results)
        "div.total_wrap",
        "div.api_subject_bx",
        "li.bx",
        // News results
        "div.news_wrap",
        "ul.list_news li",
        "div.news_area",
        // Blog/Cafe results
        "li.sh_blog_top",
        "div.blog_item",
        "ul.lst_total li",
        // Knowledge/Encyclopedia
        "div.info_group",
        "div.keyword_box",
        // Place results
        "li.place_item",
        // View tab results
        "div.view_wrap",
    ];

    for sel_str in container_selectors {
        if let Ok(container_sel) = Selector::parse(sel_str) {
            for container in doc.select(&container_sel) {
                if let Some(result) = extract_result_from_container(&container, &mut seen_urls) {
                    results.push(result);
                    if results.len() >= max_results {
                        return results;
                    }
                }
            }
        }
    }

    // Fallback: find all external links with nearby text
    if results.is_empty() {
        results = fallback_link_extraction(&doc, &mut seen_urls, max_results);
    }

    results
}

fn extract_result_from_container(
    container: &ElementRef,
    seen_urls: &mut std::collections::HashSet<String>,
) -> Option<EngineResult> {
    // Find the main link (title) - updated for modern Naver
    let link_selectors = [
        "a.link_tit",
        "a.total_tit",
        "a.api_txt_lines",
        "a.title_link",
        "a.news_tit",
        "a.sub_tit",
        "a.title",
        "a[href]",
    ];

    let mut url = None;
    let mut title = None;

    for sel_str in link_selectors {
        if let Ok(sel) = Selector::parse(sel_str) {
            if let Some(link) = container.select(&sel).next() {
                if let Some(href) = link.value().attr("href") {
                    if is_valid_external_url(href) && !seen_urls.contains(href) {
                        url = Some(href.to_string());
                        title = Some(link.text().collect::<String>().trim().to_string());
                        break;
                    }
                }
            }
        }
    }

    let url = url?;
    let title = title.filter(|t| !t.is_empty())?;

    // Find description/snippet
    let desc_selectors = [
        "div.total_dsc",
        "div.dsc_txt",
        "div.api_txt_lines.dsc_txt",
        "p.dsc_txt",
        "span.etc_dsc_area",
        "div.detail_box",
        "div.desc",
        "span.txt",
    ];

    let mut content = String::new();
    for sel_str in desc_selectors {
        if let Ok(sel) = Selector::parse(sel_str) {
            if let Some(desc_el) = container.select(&sel).next() {
                let text: String = desc_el.text().collect::<String>().trim().to_string();
                if text.len() > 10 && text.len() > content.len() {
                    content = text;
                }
            }
        }
    }

    // If no description found, try to get text from container excluding title
    if content.is_empty() {
        let all_text: String = container.text().collect();
        let cleaned = all_text
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && *l != title)
            .take(3)
            .collect::<Vec<_>>()
            .join(" ");
        if cleaned.len() > 20 {
            content = cleaned.chars().take(300).collect();
        }
    }

    // Add source hint to content if it's a known type
    if content.is_empty() {
        content = get_source_hint(&url);
    }

    seen_urls.insert(url.clone());
    Some(EngineResult::new(url, title, content))
}

fn fallback_link_extraction(
    doc: &Html,
    seen_urls: &mut std::collections::HashSet<String>,
    max_results: usize,
) -> Vec<EngineResult> {
    let mut results = Vec::new();
    let link_sel = Selector::parse("a[href]").unwrap();

    for link in doc.select(&link_sel) {
        let href = match link.value().attr("href") {
            Some(h) if is_valid_external_url(h) && !seen_urls.contains(h) => h,
            _ => continue,
        };

        let title: String = link.text().collect::<String>().trim().to_string();
        if title.is_empty() || title.len() < 4 {
            continue;
        }

        // Try to get sibling/parent text as description
        let mut content = String::new();
        if let Some(parent) = link.parent() {
            if let Some(parent_el) = parent.value().as_element() {
                if let Some(parent_ref) = ElementRef::wrap(link.parent().unwrap()) {
                    let parent_text: String = parent_ref.text().collect();
                    let desc = parent_text
                        .replace(&title, "")
                        .trim()
                        .chars()
                        .take(200)
                        .collect::<String>();
                    if desc.len() > 15 {
                        content = desc;
                    }
                }
            }
        }

        if content.is_empty() {
            content = get_source_hint(href);
        }

        seen_urls.insert(href.to_string());
        results.push(EngineResult::new(href.to_string(), title, content));

        if results.len() >= max_results {
            break;
        }
    }

    results
}

fn is_valid_external_url(href: &str) -> bool {
    // Exclude Naver internal/navigation links
    let naver_internal = [
        "www.naver.com",
        "naver.com/",
        "search.naver.com",
        "shopping.naver.com",
        "map.naver.com",
        "dict.naver.com",
        "finance.naver.com",
        "weather.naver.com",
        "sports.naver.com",
        "entertain.naver.com",
        "ad.naver.com",
        "adcr.naver.com",
        "cc.naver.com",
        "help.naver.com",
        "m.naver.com",
        "login.naver.com",
        "nid.naver.com",
    ];

    for internal in naver_internal {
        if href.contains(internal) {
            return false;
        }
    }

    !href.starts_with("#") &&
    !href.starts_with("javascript:") &&
    (href.starts_with("http://") || href.starts_with("https://"))
}

fn get_source_hint(url: &str) -> String {
    if url.contains("map.naver.com") || url.contains("place.naver.com") {
        "네이버 지도/플레이스 - 위치, 영업시간, 리뷰 정보".to_string()
    } else if url.contains("blog.naver.com") {
        "네이버 블로그 - 사용자 리뷰 및 경험담".to_string()
    } else if url.contains("cafe.naver.com") {
        "네이버 카페 - 커뮤니티 게시글".to_string()
    } else if url.contains("namu.wiki") {
        "나무위키 - 위키 백과 문서".to_string()
    } else if url.contains("shopping.naver") || url.contains("smartstore.naver") {
        "네이버 쇼핑 - 상품 정보 및 가격".to_string()
    } else if url.contains("news.naver.com") {
        "네이버 뉴스 - 언론사 기사".to_string()
    } else if url.contains("kin.naver.com") {
        "네이버 지식iN - Q&A".to_string()
    } else {
        String::new()
    }
}
