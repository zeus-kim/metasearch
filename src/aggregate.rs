//! Aggregation, de-duplication and standard scoring.
//!
//! Scoring matches the standard `result_score` exactly:
//!
//! ```text
//! weight = (product of engine weights) * (number of contributing positions)
//! score  = sum over positions p of (weight / p)
//! ```
//!
//! This reproduces the scores observed on the live `g1` instance, e.g. a result
//! returned at positions `[1, 6]` by two weight-1 engines scores
//! `2*(1/1) + 2*(1/6) = 2.3333…`.
//!
//! ## Extended ranking signals
//!
//! Beyond the standard positional score, we apply several optional boosts/penalties:
//!
//! * **Exact title match** — results whose title contains the full query get a
//!   small score boost (configurable via `ranking.title_match_boost`).
//! * **Image presence** — results with a thumbnail/image get a boost, surfacing
//!   richer cards (configurable via `ranking.image_boost`).
//! * **Near-duplicate penalty** — when multiple results have very similar titles
//!   (Jaccard > threshold), only the highest-scored one keeps its full score;
//!   others are penalized to push diversity.
//! * **Freshness (news)** — for news-category results with a `published_date`,
//!   newer items get a recency boost (exponential decay).
//! * **Domain trust** — per-domain authority multipliers (existing feature).

use std::collections::{HashMap, HashSet};

use url::Url;

use crate::thumbnail::{is_usable_thumbnail_url, prefer_thumbnail};
use crate::types::{EngineResult, SearchResult};

/// Common tracking parameters to strip from URLs during deduplication.
/// These do not affect the destination content and are used only for analytics.
const TRACKING_PARAMS: &[&str] = &[
    // Google Analytics / Campaign
    "utm_source",
    "utm_medium",
    "utm_campaign",
    "utm_term",
    "utm_content",
    "utm_id",
    "utm_source_platform",
    "utm_creative_format",
    "utm_marketing_tactic",
    // Meta / Facebook
    "fbclid",
    "fb_action_ids",
    "fb_action_types",
    "fb_source",
    "fb_ref",
    // Google
    "gclid",
    "gclsrc",
    "dclid",
    // Microsoft / Bing
    "msclkid",
    // Twitter / X
    "twclid",
    // TikTok
    "ttclid",
    // LinkedIn
    "li_fat_id",
    // Mailchimp
    "mc_cid",
    "mc_eid",
    // HubSpot
    "_hsenc",
    "_hsmi",
    "hsCtaTracking",
    // Marketo
    "mkt_tok",
    // Adobe / Omniture
    "s_kwcid",
    // Session / tracking IDs
    "sid",
    "trk",
    // Generic click/campaign tracking
    "click_id",
    "clickid",
    // News / RSS specific
    "ncid",
    "ocid",
    // Outbrain / Taboola
    "obOrigUrl",
    "dicbo",
    // Yahoo
    "soc_src",
    "soc_trk",
    // Reddit
    "share_id",
    // Misc GA / tracking
    "_ga",
    "_gl",
    "igshid",
    "zanpid",
];

/// Remove tracking parameters from a URL query string.
/// Returns the cleaned query string (without leading `?`), or empty if no params remain.
fn strip_tracking_params(query: Option<&str>) -> String {
    let Some(q) = query else {
        return String::new();
    };
    if q.is_empty() {
        return String::new();
    }

    let tracking_set: HashSet<&str> = TRACKING_PARAMS.iter().copied().collect();
    let filtered: Vec<&str> = q
        .split('&')
        .filter(|pair| {
            let key = pair.split('=').next().unwrap_or("");
            !tracking_set.contains(key)
        })
        .collect();

    if filtered.is_empty() {
        String::new()
    } else {
        format!("?{}", filtered.join("&"))
    }
}

/// A `(scheme-insensitive, www/trailing-slash-normalized, tracking-stripped)` key
/// used to detect that two engines returned "the same" URL.
fn dedup_key(raw: &str) -> String {
    match Url::parse(raw) {
        Ok(u) => {
            let host = u
                .host_str()
                .unwrap_or("")
                .trim_start_matches("www.")
                .to_lowercase();
            let mut path = u.path().trim_end_matches('/').to_string();
            if path.is_empty() {
                path = "/".into();
            }
            let query = strip_tracking_params(u.query());
            format!("{host}{path}{query}")
        }
        Err(_) => raw.trim().trim_end_matches('/').to_lowercase(),
    }
}

/// Normalize a title for fuzzy comparison: lowercase, strip punctuation, collapse whitespace.
fn normalize_title(title: &str) -> String {
    title
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Extract word tokens from a normalized title.
fn title_tokens(normalized: &str) -> HashSet<&str> {
    normalized.split_whitespace().collect()
}

/// Compute Jaccard similarity between two token sets.
fn jaccard_similarity(a: &HashSet<&str>, b: &HashSet<&str>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let intersection = a.intersection(b).count();
    let union = a.union(b).count();
    if union == 0 {
        0.0
    } else {
        intersection as f64 / union as f64
    }
}

/// Threshold for title similarity deduplication. Titles with Jaccard similarity
/// above this value (on the same domain) are considered near-duplicates.
const TITLE_SIMILARITY_THRESHOLD: f64 = 0.75;

/// Extract the domain from a URL for grouping similar results.
fn extract_domain(url: &str) -> String {
    match Url::parse(url) {
        Ok(u) => u
            .host_str()
            .unwrap_or("")
            .trim_start_matches("www.")
            .to_lowercase(),
        Err(_) => String::new(),
    }
}

/// Deduplicate results with similar titles on the same domain.
/// Keeps the highest-scored result and merges metadata from duplicates.
/// This catches cases where different URL paths on the same site return
/// essentially the same content (pagination, localization, etc.).
fn dedup_by_title_similarity(results: &mut Vec<SearchResult>) {
    if results.len() < 2 {
        return;
    }

    // Group by domain first for efficiency (only compare within same domain)
    let mut domain_groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, r) in results.iter().enumerate() {
        let domain = extract_domain(&r.url);
        domain_groups.entry(domain).or_default().push(i);
    }

    let mut to_remove: HashSet<usize> = HashSet::new();
    let mut merge_into: HashMap<usize, Vec<usize>> = HashMap::new(); // keeper -> list of merged indices

    for indices in domain_groups.values() {
        if indices.len() < 2 {
            continue;
        }

        // Precompute normalized titles and tokens for this domain group
        let normalized: Vec<(usize, String)> = indices
            .iter()
            .map(|&i| (i, normalize_title(&results[i].title)))
            .collect();

        // Compare pairs within the domain
        for i in 0..normalized.len() {
            let (idx_a, ref norm_a) = normalized[i];
            if to_remove.contains(&idx_a) || norm_a.is_empty() {
                continue;
            }
            let tokens_a = title_tokens(norm_a);
            if tokens_a.len() < 2 {
                // Skip very short titles to avoid false positives
                continue;
            }

            for j in (i + 1)..normalized.len() {
                let (idx_b, ref norm_b) = normalized[j];
                if to_remove.contains(&idx_b) || norm_b.is_empty() {
                    continue;
                }
                let tokens_b = title_tokens(norm_b);
                if tokens_b.len() < 2 {
                    continue;
                }

                let sim = jaccard_similarity(&tokens_a, &tokens_b);
                if sim >= TITLE_SIMILARITY_THRESHOLD {
                    // These are near-duplicates; keep the higher-scored one
                    let (keeper, duplicate) = if results[idx_a].score >= results[idx_b].score {
                        (idx_a, idx_b)
                    } else {
                        (idx_b, idx_a)
                    };
                    to_remove.insert(duplicate);
                    merge_into.entry(keeper).or_default().push(duplicate);
                }
            }
        }
    }

    // Merge metadata from duplicates into keepers
    for (&keeper, duplicates) in &merge_into {
        // Collect data from duplicates first to avoid borrow conflicts
        let mut engines_to_add: Vec<String> = Vec::new();
        let mut positions_to_add: Vec<usize> = Vec::new();
        let mut best_content: Option<String> = None;
        let mut best_thumbnail: Option<String> = None;
        let mut best_img_src: Option<String> = None;
        let mut score_boost: f64 = 0.0;

        for &dup in duplicates {
            for engine in &results[dup].engines {
                if !engines_to_add.contains(engine) {
                    engines_to_add.push(engine.clone());
                }
            }
            positions_to_add.extend(&results[dup].positions);
            if results[dup].content.len() > best_content.as_ref().map(|s| s.len()).unwrap_or(0) {
                best_content = Some(results[dup].content.clone());
            }
            if !results[dup].thumbnail.is_empty() && best_thumbnail.is_none() {
                best_thumbnail = Some(results[dup].thumbnail.clone());
            }
            if !results[dup].img_src.is_empty() && best_img_src.is_none() {
                best_img_src = Some(results[dup].img_src.clone());
            }
            score_boost += results[dup].score * 0.1;
        }

        // Now apply the collected data to the keeper
        let keeper_result = &mut results[keeper];
        for engine in engines_to_add {
            if !keeper_result.engines.contains(&engine) {
                keeper_result.engines.push(engine);
            }
        }
        keeper_result.positions.extend(positions_to_add);
        if let Some(content) = best_content {
            if content.len() > keeper_result.content.len() {
                keeper_result.content = content;
            }
        }
        if keeper_result.thumbnail.is_empty() {
            if let Some(thumbnail) = best_thumbnail {
                keeper_result.thumbnail = thumbnail;
            }
        }
        if keeper_result.img_src.is_empty() {
            if let Some(img_src) = best_img_src {
                keeper_result.img_src = img_src;
            }
        }
        keeper_result.score += score_boost;
    }

    // Remove duplicates (in reverse order to preserve indices)
    let mut to_remove_sorted: Vec<usize> = to_remove.into_iter().collect();
    to_remove_sorted.sort_by(|a, b| b.cmp(a));
    for idx in to_remove_sorted {
        results.remove(idx);
    }

    // Re-sort by score after merging
    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

/// Build the `[scheme, netloc, path, params, query, fragment]` tuple that
/// exposes (it comes from Python's `urlparse`).
///
/// `params` is the RFC 3986 path-parameter segment (everything after the last
/// `;` in the final path segment), which `urlparse` separates out.
fn parsed_url(raw: &str) -> [String; 6] {
    match Url::parse(raw) {
        Ok(u) => {
            let netloc = match (u.host_str(), u.port()) {
                (Some(h), Some(p)) => format!("{h}:{p}"),
                (Some(h), None) => h.to_string(),
                _ => String::new(),
            };
            let (path, params) = split_path_params(u.path());
            [
                u.scheme().to_string(),
                netloc,
                path,
                params,
                u.query().unwrap_or("").to_string(),
                u.fragment().unwrap_or("").to_string(),
            ]
        }
        Err(_) => Default::default(),
    }
}

/// Split a URL path into `(path, params)` exactly like Python's `urlparse`:
/// only the last path segment may carry `;params`.
fn split_path_params(path: &str) -> (String, String) {
    match path.rsplit_once('/') {
        Some((head, last)) => match last.split_once(';') {
            Some((seg, par)) => (format!("{head}/{seg}"), par.to_string()),
            None => (path.to_string(), String::new()),
        },
        None => match path.split_once(';') {
            Some((seg, par)) => (seg.to_string(), par.to_string()),
            None => (path.to_string(), String::new()),
        },
    }
}

/// Merge per-engine result lists into a single ranked list.
///
/// `per_engine` is `(engine_name, results_in_order)`. `weights` maps engine name
/// to its configured weight (defaults to `1.0` when absent).
pub fn aggregate(
    per_engine: Vec<(String, Vec<EngineResult>)>,
    weights: &HashMap<String, f64>,
) -> Vec<SearchResult> {
    let mut order: Vec<String> = Vec::new();
    let mut map: HashMap<String, SearchResult> = HashMap::new();

    for (engine, results) in per_engine {
        for (i, r) in results.into_iter().enumerate() {
            let position = i + 1;
            let key = dedup_key(&r.url);
            // Image-search results *are* the image; the engine already dropped
            // icon-sized hits, so don't re-run the news-card thumbnail gate
            // (which would strip otherwise-fine image URLs and shrink coverage).
            let is_image = r.template.as_deref() == Some("images.html");

            if let Some(existing) = map.get_mut(&key) {
                existing.positions.push(position);
                if !existing.engines.contains(&engine) {
                    existing.engines.push(engine.clone());
                }
                // Keep the richest version of each field.
                if r.content.len() > existing.content.len() {
                    existing.content = r.content;
                }
                if existing.title.is_empty() && !r.title.is_empty() {
                    existing.title = r.title;
                }
                if existing.img_src.is_empty() {
                    if let Some(img) = r.img_src.filter(|u| is_image || is_usable_thumbnail_url(u))
                    {
                        existing.img_src = img;
                    }
                } else if let Some(img) = r.img_src {
                    if !is_image {
                        existing.img_src = prefer_thumbnail(&existing.img_src, &img);
                    }
                }
                if existing.thumbnail.is_empty() {
                    if let Some(t) = r
                        .thumbnail
                        .filter(|u| is_image || is_usable_thumbnail_url(u))
                    {
                        existing.thumbnail = t;
                    }
                } else if let Some(t) = r.thumbnail {
                    if !is_image {
                        existing.thumbnail = prefer_thumbnail(&existing.thumbnail, &t);
                    }
                }
                if existing.published_date.is_none() {
                    existing.published_date = r.published_date;
                }
                if existing.priority.is_empty() {
                    if let Some(p) = r.priority {
                        existing.priority = p;
                    }
                }
                if existing.publisher_url.is_empty() {
                    if let Some(u) = r.publisher_url.clone() {
                        existing.publisher_url = u;
                    }
                }
            } else {
                order.push(key.clone());
                map.insert(
                    key,
                    SearchResult {
                        parsed_url: parsed_url(&r.url),
                        url: r.url,
                        title: r.title,
                        content: r.content,
                        engine: engine.clone(),
                        template: r.template.unwrap_or_else(|| "default.html".into()),
                        img_src: r
                            .img_src
                            .filter(|u| is_image || is_usable_thumbnail_url(u))
                            .unwrap_or_default(),
                        thumbnail: r
                            .thumbnail
                            .filter(|u| is_image || is_usable_thumbnail_url(u))
                            .unwrap_or_default(),
                        priority: r.priority.unwrap_or_default(),
                        engines: vec![engine.clone()],
                        positions: vec![position],
                        score: 0.0,
                        category: r.category.unwrap_or_else(|| "general".into()),
                        published_date: r.published_date,
                        favicon: String::new(),
                        cluster: None,
                        summary: None,
                        highlights: Vec::new(),
                        publisher_url: r.publisher_url.clone().unwrap_or_default(),
                    },
                );
            }
        }
    }

    for result in map.values_mut() {
        let weight_product: f64 = result
            .engines
            .iter()
            .map(|e| weights.get(e).copied().unwrap_or(1.0))
            .product();
        let weight = weight_product * result.positions.len() as f64;
        result.score = result.positions.iter().map(|&p| weight / p as f64).sum();
    }

    let mut out: Vec<SearchResult> = order.into_iter().filter_map(|k| map.remove(&k)).collect();
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Second pass: deduplicate by title similarity (same domain, similar titles)
    dedup_by_title_similarity(&mut out);

    out
}

/// Merge several already-aggregated result lists (e.g. from multi-hop deep
/// research), de-duplicating by URL and combining scores / engine metadata.
/// Results are re-sorted by combined score and truncated to `cap`.
pub fn merge_aggregated(lists: Vec<Vec<SearchResult>>, cap: usize) -> Vec<SearchResult> {
    let cap = cap.max(1);
    let mut order: Vec<String> = Vec::new();
    let mut map: HashMap<String, SearchResult> = HashMap::new();

    for list in lists {
        for r in list {
            let key = dedup_key(&r.url);
            if let Some(existing) = map.get_mut(&key) {
                existing.score += r.score;
                for e in &r.engines {
                    if !existing.engines.contains(e) {
                        existing.engines.push(e.clone());
                    }
                }
                existing.positions.extend(&r.positions);
                if r.content.len() > existing.content.len() {
                    existing.content = r.content.clone();
                }
                if existing.title.is_empty() && !r.title.is_empty() {
                    existing.title = r.title.clone();
                }
                if existing.img_src.is_empty() && !r.img_src.is_empty() {
                    if is_usable_thumbnail_url(&r.img_src) {
                        existing.img_src = r.img_src.clone();
                    }
                } else if !r.img_src.is_empty() {
                    existing.img_src = prefer_thumbnail(&existing.img_src, &r.img_src);
                }
                if existing.thumbnail.is_empty() && !r.thumbnail.is_empty() {
                    if is_usable_thumbnail_url(&r.thumbnail) {
                        existing.thumbnail = r.thumbnail.clone();
                    }
                } else if !r.thumbnail.is_empty() {
                    existing.thumbnail = prefer_thumbnail(&existing.thumbnail, &r.thumbnail);
                }
                if existing.published_date.is_none() {
                    existing.published_date = r.published_date;
                }
            } else {
                order.push(key.clone());
                map.insert(key, r);
            }
        }
    }

    let mut out: Vec<SearchResult> = order.into_iter().filter_map(|k| map.remove(&k)).collect();
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Second pass: deduplicate by title similarity (same domain, similar titles)
    dedup_by_title_similarity(&mut out);

    out.truncate(cap);
    out
}

/// Apply per-domain authority/trust multipliers to result scores, then re-sort.
///
/// This is a no-AI ranking signal layered on top of the positional
/// score: each result's score is multiplied by the weight of the most specific
/// matching `domain_trust` entry (a result on `en.wikipedia.org` matches a
/// `wikipedia.org` entry). With an empty trust map this is a no-op, preserving
/// the exact default ordering as the default fallback.
pub fn apply_domain_trust(results: &mut [SearchResult], trust: &[crate::config::DomainTrust]) {
    if trust.is_empty() {
        return;
    }
    for r in results.iter_mut() {
        let host = r.parsed_url[1]
            .split(':')
            .next()
            .unwrap_or("")
            .to_lowercase();
        if host.is_empty() {
            continue;
        }
        if let Some(w) = best_trust_match(&host, trust) {
            r.score *= w;
        }
    }
    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

/// Find the weight of the most specific (longest) trust domain that `host`
/// equals or is a subdomain of.
fn best_trust_match(host: &str, trust: &[crate::config::DomainTrust]) -> Option<f64> {
    let mut best: Option<(usize, f64)> = None;
    for entry in trust {
        let d = entry.domain.trim_start_matches('.').to_lowercase();
        if d.is_empty() {
            continue;
        }
        let matches = host == d || host.ends_with(&format!(".{d}"));
        if matches {
            let len = d.len();
            if best.map(|(l, _)| len > l).unwrap_or(true) {
                best = Some((len, entry.weight));
            }
        }
    }
    best.map(|(_, w)| w)
}

/// Fill the `favicon` field of each result from a resolver template containing
/// `{domain}`. A no-op when the template is empty.
pub fn apply_favicons(results: &mut [SearchResult], resolver: &str) {
    if resolver.is_empty() {
        return;
    }
    for r in results.iter_mut() {
        // parsed_url[1] is the netloc (host[:port]); drop any port for the icon.
        let host = r.parsed_url[1].split(':').next().unwrap_or("");
        if !host.is_empty() {
            r.favicon = resolver.replace("{domain}", host);
        }
    }
}

// ============================================================================
// Extended ranking signals
// ============================================================================

/// Configuration for extended ranking signals.
#[derive(Debug, Clone)]
pub struct RankingConfig {
    /// Boost multiplier for results whose title contains the full query.
    pub title_match_boost: f64,
    /// Boost multiplier for results that have a thumbnail/image.
    pub image_boost: f64,
    /// Penalty multiplier applied to cross-domain near-duplicate results (lower = harsher).
    pub cross_domain_dedup_penalty: f64,
    /// Whether to apply freshness boost to news results.
    pub freshness_enabled: bool,
    /// Half-life in hours for freshness decay (how old an article must be to
    /// lose half its freshness bonus).
    pub freshness_half_life_hours: f64,
    /// Weight of freshness in the final score (0.0..1.0).
    pub freshness_weight: f64,
}

impl Default for RankingConfig {
    fn default() -> Self {
        RankingConfig {
            title_match_boost: 1.15,       // 15% boost for exact title match
            image_boost: 1.08,             // 8% boost for having an image
            cross_domain_dedup_penalty: 0.7, // 30% penalty for cross-domain duplicates
            freshness_enabled: true,
            freshness_half_life_hours: 24.0,
            freshness_weight: 0.3,         // 30% weight for freshness
        }
    }
}

/// Apply extended ranking signals to already-aggregated results: title match
/// boost, image boost, cross-domain near-duplicate penalty, and freshness for news.
/// Re-sorts by adjusted score.
pub fn apply_extended_ranking(
    results: &mut [SearchResult],
    query: &str,
    config: &RankingConfig,
    is_news: bool,
) {
    if results.is_empty() {
        return;
    }

    // Normalize query for title matching.
    let query_lower = query.trim().to_lowercase();

    // 1. Title match boost + image boost + web search engine boost.
    for r in results.iter_mut() {
        let title_lower = r.title.to_lowercase();

        // Exact title match: query appears as a substring in title.
        if !query_lower.is_empty() && title_lower.contains(&query_lower) {
            r.score *= config.title_match_boost;
        }

        // Image presence boost.
        if !r.img_src.is_empty() || !r.thumbnail.is_empty() {
            r.score *= config.image_boost;
        }

        // Web search engine boost: prioritize DuckDuckGo/Brave over reference engines.
        // This ensures actual web results appear before Wikipedia for general queries.
        let engine = r.engine.to_lowercase();
        if engine.contains("duckduckgo") || engine.contains("brave") || engine == "mojeek" || engine == "startpage" || engine == "google_web" || engine == "bing_web" || engine == "naver" || engine == "daum" {
            r.score *= 2.5; // 150% boost for web search engines
        } else if engine.contains("wiki") && !engine.contains("news") {
            r.score *= 0.3; // 70% penalty for Wikipedia/reference (often matches random words)
        }
    }

    // 2. Cross-domain near-duplicate penalty (different domains, similar titles).
    // Same-domain dedup is already handled by dedup_by_title_similarity in aggregate().
    apply_cross_domain_dedup_penalty(results, config.cross_domain_dedup_penalty);

    // 3. Freshness boost for news results.
    if is_news && config.freshness_enabled {
        apply_freshness_boost(results, config);
    }

    // 4. Re-sort by adjusted score.
    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

/// Penalize cross-domain near-duplicate results (different URLs/domains, similar titles).
/// The highest-scored result keeps its score; later near-duplicates get penalized.
fn apply_cross_domain_dedup_penalty(results: &mut [SearchResult], penalty: f64) {
    let n = results.len();
    if n < 2 {
        return;
    }

    // Pre-normalize and tokenize all titles.
    let normalized: Vec<String> = results.iter().map(|r| normalize_title(&r.title)).collect();
    let tokens: Vec<HashSet<&str>> = normalized.iter().map(|t| title_tokens(t)).collect();

    // Track which results have already been penalized (to avoid double penalty).
    let mut penalized: HashSet<usize> = HashSet::new();

    // Cross-domain threshold is slightly higher than same-domain (0.80 vs 0.75)
    // to reduce false positives across different sites.
    const CROSS_DOMAIN_THRESHOLD: f64 = 0.80;

    // For each result, check against higher-scored results for similarity.
    // Results are already sorted by score descending, so earlier = higher score.
    for i in 1..n {
        if penalized.contains(&i) {
            continue;
        }
        // Skip if title is too short (likely to match accidentally).
        if tokens[i].len() < 4 {
            continue;
        }
        for j in 0..i {
            // Skip same-domain pairs (already handled by dedup_by_title_similarity).
            let domain_i = extract_domain(&results[i].url);
            let domain_j = extract_domain(&results[j].url);
            if domain_i == domain_j {
                continue;
            }

            if jaccard_similarity(&tokens[i], &tokens[j]) >= CROSS_DOMAIN_THRESHOLD {
                // Result i is a near-duplicate of higher-scored result j (different domain).
                results[i].score *= penalty;
                penalized.insert(i);
                break;
            }
        }
    }
}

/// Apply freshness boost to news results based on published_date.
/// Uses simple relative time parsing ("X hours ago", "yesterday") since
/// we don't have chrono as a dependency.
fn apply_freshness_boost(results: &mut [SearchResult], config: &RankingConfig) {
    for r in results.iter_mut() {
        if let Some(ref date_str) = r.published_date {
            if let Some(age_hours) = parse_relative_time(date_str) {
                // Exponential decay: score_factor = 2^(-age / half_life).
                let decay = (-age_hours / config.freshness_half_life_hours).exp2();
                // Blend: final = base * (1 - w) + base * decay * w
                //             = base * (1 - w + w * decay)
                let factor = 1.0 - config.freshness_weight + config.freshness_weight * decay;
                r.score *= factor.max(0.5); // Floor at 50% of original score.
            }
        }
    }
}

/// Parse relative time strings like "2 hours ago", "3 days ago", "1 week ago".
/// Returns age in hours. No chrono dependency - just parses common patterns.
pub fn parse_relative_time(s: &str) -> Option<f64> {
    let lower = s.to_lowercase().trim().to_string();
    if lower.is_empty() {
        return None;
    }

    // Match "yesterday", "today", "just now".
    if lower.contains("just now") || lower.contains("moments ago") {
        return Some(0.1);
    }
    if lower.contains("today") {
        return Some(6.0); // Assume middle of day.
    }
    if lower.contains("yesterday") {
        return Some(24.0);
    }

    // Match patterns like "X unit(s) ago" using simple string parsing.
    // Format: "N unit ago" or "N units ago"
    let parts: Vec<&str> = lower.split_whitespace().collect();
    if parts.len() >= 3 && parts.last() == Some(&"ago") {
        if let Ok(num) = parts[0].parse::<f64>() {
            let unit = parts[1].trim_end_matches('s');
            let hours = match unit {
                "second" => num / 3600.0,
                "minute" | "min" => num / 60.0,
                "hour" | "hr" => num,
                "day" => num * 24.0,
                "week" | "wk" => num * 24.0 * 7.0,
                "month" | "mo" => num * 24.0 * 30.0,
                "year" | "yr" => num * 24.0 * 365.0,
                _ => return None,
            };
            return Some(hours);
        }
    }

    // Match "an hour ago", "a day ago" patterns.
    if lower.starts_with("an hour") || lower.starts_with("1 hour") {
        return Some(1.0);
    }
    if lower.starts_with("a day") || lower.starts_with("1 day") {
        return Some(24.0);
    }
    if lower.starts_with("a week") || lower.starts_with("1 week") {
        return Some(24.0 * 7.0);
    }

    None
}

/// Default set of trusted domains with their boost weights.
/// These are high-authority sources that should rank higher.
pub fn default_trusted_domains() -> Vec<crate::config::DomainTrust> {
    vec![
        // Reference / encyclopedic.
        crate::config::DomainTrust { domain: "wikipedia.org".into(), weight: 1.25 },
        crate::config::DomainTrust { domain: "britannica.com".into(), weight: 1.2 },
        crate::config::DomainTrust { domain: "wikiwand.com".into(), weight: 1.15 },
        // Academic / research.
        crate::config::DomainTrust { domain: "arxiv.org".into(), weight: 1.3 },
        crate::config::DomainTrust { domain: "nature.com".into(), weight: 1.25 },
        crate::config::DomainTrust { domain: "science.org".into(), weight: 1.25 },
        crate::config::DomainTrust { domain: "nih.gov".into(), weight: 1.2 },
        crate::config::DomainTrust { domain: "pubmed.ncbi.nlm.nih.gov".into(), weight: 1.2 },
        // Official / government.
        crate::config::DomainTrust { domain: "gov".into(), weight: 1.15 },
        crate::config::DomainTrust { domain: "edu".into(), weight: 1.15 },
        // Tech / dev.
        crate::config::DomainTrust { domain: "github.com".into(), weight: 1.15 },
        crate::config::DomainTrust { domain: "stackoverflow.com".into(), weight: 1.2 },
        crate::config::DomainTrust { domain: "developer.mozilla.org".into(), weight: 1.2 },
        crate::config::DomainTrust { domain: "docs.rs".into(), weight: 1.15 },
        crate::config::DomainTrust { domain: "crates.io".into(), weight: 1.1 },
        // News (reputable).
        crate::config::DomainTrust { domain: "reuters.com".into(), weight: 1.15 },
        crate::config::DomainTrust { domain: "apnews.com".into(), weight: 1.15 },
        crate::config::DomainTrust { domain: "bbc.com".into(), weight: 1.1 },
        crate::config::DomainTrust { domain: "bbc.co.uk".into(), weight: 1.1 },
        // Demote low-quality / SEO-heavy domains.
        crate::config::DomainTrust { domain: "pinterest.com".into(), weight: 0.7 },
        crate::config::DomainTrust { domain: "quora.com".into(), weight: 0.85 },
        crate::config::DomainTrust { domain: "w3schools.com".into(), weight: 0.8 },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn weights(pairs: &[(&str, f64)]) -> HashMap<String, f64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn dedup_normalizes_scheme_www_and_trailing_slash() {
        assert_eq!(
            dedup_key("https://www.Example.com/Path/"),
            dedup_key("http://example.com/Path")
        );
    }

    #[test]
    fn merges_duplicates_and_scores_standard() {
        // Reproduce the g1 example: one URL returned by engine "a" at position 1
        // and by engine "b" at position 6 -> score 2.3333…
        let target = "https://en.wikipedia.org/wiki/Test";
        let a = vec![EngineResult::new(target, "Test - Wikipedia", "snippet")];
        let mut b = Vec::new();
        for n in 1..=5 {
            b.push(EngineResult::new(
                format!("https://example.com/{n}"),
                format!("r{n}"),
                "x",
            ));
        }
        b.push(EngineResult::new(
            target,
            "Test",
            "a much longer snippet body",
        ));

        let weights = weights(&[("a", 1.0), ("b", 1.0)]);
        let results = aggregate(vec![("a".into(), a), ("b".into(), b)], &weights);

        let top = &results[0];
        assert_eq!(top.url, target);
        assert_eq!(top.engines, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(top.positions, vec![1, 6]);
        assert!((top.score - 2.3333333333333335).abs() < 1e-9);
        // Richer content wins on merge.
        assert_eq!(top.content, "a much longer snippet body");
        // The shared URL is de-duplicated: 5 unique others + 1 merged = 6.
        assert_eq!(results.len(), 6);
    }

    #[test]
    fn weight_scales_score() {
        let url = "https://a.test/";
        let one = aggregate(
            vec![("e".into(), vec![EngineResult::new(url, "t", "c")])],
            &weights(&[("e", 1.0)]),
        );
        let two = aggregate(
            vec![("e".into(), vec![EngineResult::new(url, "t", "c")])],
            &weights(&[("e", 2.0)]),
        );
        assert!((one[0].score - 1.0).abs() < 1e-9);
        assert!((two[0].score - 2.0).abs() < 1e-9);
    }

    #[test]
    fn parsed_url_extracts_params_and_fields() {
        assert_eq!(
            super::parsed_url("https://host.test:8443/a/b;sid=9?x=1#frag"),
            [
                "https".to_string(),
                "host.test:8443".to_string(),
                "/a/b".to_string(),
                "sid=9".to_string(),
                "x=1".to_string(),
                "frag".to_string(),
            ]
        );
    }

    #[test]
    fn favicons_use_host_only() {
        let mut results = aggregate(
            vec![(
                "e".into(),
                vec![EngineResult::new(
                    "https://www.rust-lang.org:443/",
                    "Rust",
                    "c",
                )],
            )],
            &weights(&[("e", 1.0)]),
        );
        apply_favicons(&mut results, "https://icons.example/{domain}.ico");
        assert_eq!(
            results[0].favicon,
            "https://icons.example/www.rust-lang.org.ico"
        );
    }

    #[test]
    fn domain_trust_boosts_and_resorts() {
        use crate::config::DomainTrust;
        // Two single-hit results: low.test at pos 1 (score 1.0), trusted.test at
        // pos 1 (score 1.0). A trust boost on trusted.test should float it up.
        let mut results = aggregate(
            vec![
                (
                    "a".into(),
                    vec![EngineResult::new("https://low.test/", "l", "c")],
                ),
                (
                    "b".into(),
                    vec![EngineResult::new("https://en.trusted.test/page", "t", "c")],
                ),
            ],
            &weights(&[("a", 1.0), ("b", 1.0)]),
        );
        let trust = vec![DomainTrust {
            domain: "trusted.test".into(),
            weight: 3.0,
        }];
        apply_domain_trust(&mut results, &trust);
        assert_eq!(results[0].parsed_url[1], "en.trusted.test");
        assert!((results[0].score - 3.0).abs() < 1e-9);
    }

    #[test]
    fn domain_trust_empty_is_noop() {
        let mut results = aggregate(
            vec![(
                "a".into(),
                vec![EngineResult::new("https://x.test/", "x", "c")],
            )],
            &weights(&[("a", 1.0)]),
        );
        let before = results[0].score;
        apply_domain_trust(&mut results, &[]);
        assert_eq!(results[0].score, before);
    }

    #[test]
    fn merge_aggregated_dedupes_and_caps() {
        let mk = |url: &str, score: f64| SearchResult {
            url: url.into(),
            title: url.into(),
            content: "c".into(),
            engine: "a".into(),
            template: "default.html".into(),
            parsed_url: Default::default(),
            img_src: String::new(),
            thumbnail: String::new(),
            priority: String::new(),
            engines: vec!["a".into()],
            positions: vec![1],
            score,
            category: "general".into(),
            published_date: None,
            favicon: String::new(),
            cluster: None,
            summary: None,
            highlights: Vec::new(),
            publisher_url: String::new(),
        };
        let a = vec![mk("https://dup.test/", 2.0), mk("https://one.test/", 1.0)];
        let b = vec![mk("https://dup.test/", 3.0), mk("https://two.test/", 0.5)];
        let merged = merge_aggregated(vec![a, b], 10);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0].url, "https://dup.test/");
        assert!((merged[0].score - 5.0).abs() < 1e-9);
        assert_eq!(merged[0].engines, vec!["a".to_string()]);
        let capped = merge_aggregated(
            vec![(0..5)
                .map(|i| mk(&format!("https://x{i}.test/"), 1.0))
                .collect()],
            3,
        );
        assert_eq!(capped.len(), 3);
    }

    #[test]
    fn image_results_bypass_news_thumbnail_gate() {
        // A URL that the news-card gate would reject (contains "/logo") must
        // survive for image-search results, where the image *is* the result.
        let r = EngineResult::new("https://example.com/brand", "Brand logo", "")
            .image("https://cdn.example.com/logo-1200x630.png", "");
        let results = aggregate(vec![("img".into(), vec![r])], &weights(&[("img", 1.0)]));
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].img_src,
            "https://cdn.example.com/logo-1200x630.png"
        );
        assert_eq!(results[0].template, "images.html");
    }

    #[test]
    fn ranks_by_score_descending() {
        let high = "https://high.test/";
        let low = "https://low.test/";
        // `high` appears at position 1 in two engines; `low` only once at pos 2.
        let results = aggregate(
            vec![
                (
                    "a".into(),
                    vec![
                        EngineResult::new(high, "h", "c"),
                        EngineResult::new(low, "l", "c"),
                    ],
                ),
                ("b".into(), vec![EngineResult::new(high, "h", "c")]),
            ],
            &weights(&[("a", 1.0), ("b", 1.0)]),
        );
        assert_eq!(results[0].url, high);
        assert_eq!(results[1].url, low);
        assert!(results[0].score > results[1].score);
    }

    #[test]
    fn dedup_strips_tracking_params() {
        // URLs with different tracking params should dedupe to the same key
        let base = "https://example.com/article/123";
        let with_utm = "https://example.com/article/123?utm_source=google&utm_medium=cpc";
        let with_fbclid = "https://example.com/article/123?fbclid=abc123";
        let with_gclid = "https://example.com/article/123?gclid=xyz789&msclkid=def456";

        assert_eq!(dedup_key(base), dedup_key(with_utm));
        assert_eq!(dedup_key(base), dedup_key(with_fbclid));
        assert_eq!(dedup_key(base), dedup_key(with_gclid));

        // But real query params should be preserved
        let with_real_param = "https://example.com/article/123?page=2";
        assert_ne!(dedup_key(base), dedup_key(with_real_param));

        // Mixed: real params survive, tracking params stripped
        let mixed = "https://example.com/article/123?page=2&utm_source=twitter";
        assert_eq!(dedup_key(with_real_param), dedup_key(mixed));
    }

    #[test]
    fn dedup_strips_multiple_tracking_params() {
        let clean = "https://news.site/story";
        let tracked = "https://news.site/story?utm_source=newsletter&utm_medium=email&utm_campaign=weekly&fbclid=123&gclid=456&_ga=789";
        assert_eq!(dedup_key(clean), dedup_key(tracked));
    }

    #[test]
    fn title_similarity_dedupes_same_domain() {
        // Two results from the same domain with very similar titles should be deduped
        let a = EngineResult::new(
            "https://news.example.com/article/123",
            "Breaking News: Major Event Happens Today",
            "First snippet",
        );
        let b = EngineResult::new(
            "https://news.example.com/article/456",
            "Breaking News: Major Event Happens Today - Updated",
            "Second longer snippet with more content",
        );
        let c = EngineResult::new(
            "https://other.example.com/story",
            "Completely Different Story About Something Else",
            "Different content",
        );

        let results = aggregate(
            vec![("google".into(), vec![a, b, c])],
            &weights(&[("google", 1.0)]),
        );

        // Should have 2 results: the merged news article + the different story
        assert_eq!(results.len(), 2);
        // The richer content should be kept
        assert!(results
            .iter()
            .any(|r| r.content == "Second longer snippet with more content"));
    }

    #[test]
    fn title_similarity_preserves_different_titles() {
        // Results with different titles on the same domain should NOT be deduped
        let a = EngineResult::new(
            "https://docs.example.com/api/users",
            "User API Reference",
            "User documentation",
        );
        let b = EngineResult::new(
            "https://docs.example.com/api/products",
            "Product API Reference",
            "Product documentation",
        );

        let results = aggregate(
            vec![("bing".into(), vec![a, b])],
            &weights(&[("bing", 1.0)]),
        );

        // Both should remain since titles are quite different
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn cross_engine_dedup_with_tracking_params() {
        // Same article from different engines with different tracking params
        let from_google = EngineResult::new(
            "https://blog.example.com/post/awesome-rust?utm_source=google",
            "Why Rust is Awesome",
            "Short snippet",
        );
        let from_bing = EngineResult::new(
            "https://blog.example.com/post/awesome-rust?msclkid=abc123",
            "Why Rust is Awesome",
            "Much longer and more detailed snippet explaining why Rust is great",
        );

        let results = aggregate(
            vec![
                ("google".into(), vec![from_google]),
                ("bing".into(), vec![from_bing]),
            ],
            &weights(&[("google", 1.0), ("bing", 1.0)]),
        );

        // Should be merged into one result
        assert_eq!(results.len(), 1);
        // Should have both engines
        assert!(results[0].engines.contains(&"google".to_string()));
        assert!(results[0].engines.contains(&"bing".to_string()));
        // Should keep the richer content
        assert!(results[0].content.contains("Much longer"));
    }

    #[test]
    fn jaccard_similarity_calculation() {
        let a: HashSet<&str> = ["rust", "programming", "language"].into_iter().collect();
        let b: HashSet<&str> = ["rust", "programming", "lang"].into_iter().collect();
        let c: HashSet<&str> = ["python", "scripting", "language"].into_iter().collect();

        // a and b share 2 of 4 unique words = 0.5
        let sim_ab = jaccard_similarity(&a, &b);
        assert!((sim_ab - 0.5).abs() < 0.01);

        // a and c share 1 of 5 unique words = 0.2
        let sim_ac = jaccard_similarity(&a, &c);
        assert!((sim_ac - 0.2).abs() < 0.01);

        // Empty sets
        let empty: HashSet<&str> = HashSet::new();
        assert!((jaccard_similarity(&empty, &empty) - 1.0).abs() < 0.01);
    }

    #[test]
    fn normalize_title_removes_punctuation() {
        assert_eq!(
            normalize_title("Hello, World! How's it going?"),
            "hello world hows it going"
        );
        assert_eq!(
            normalize_title("  Multiple   Spaces   Here  "),
            "multiple spaces here"
        );
        assert_eq!(
            normalize_title("ALL CAPS TITLE"),
            "all caps title"
        );
    }

    // ========================================================================
    // Extended ranking tests
    // ========================================================================

    #[test]
    fn title_match_boost_applies() {
        let mut results = aggregate(
            vec![(
                "a".into(),
                vec![
                    EngineResult::new("https://rust.test/", "Rust Programming Language", "c"),
                    EngineResult::new("https://other.test/", "Some Other Topic", "c"),
                ],
            )],
            &weights(&[("a", 1.0)]),
        );
        let config = RankingConfig::default();
        apply_extended_ranking(&mut results, "rust", &config, false);

        // The result with "rust" in the title should have been boosted.
        let rust_result = results.iter().find(|r| r.url.contains("rust.test")).unwrap();
        let other_result = results.iter().find(|r| r.url.contains("other.test")).unwrap();
        assert!(rust_result.score > other_result.score);
    }

    #[test]
    fn image_boost_applies() {
        let mut results = vec![
            SearchResult {
                url: "https://with-image.test/".into(),
                title: "With Image".into(),
                content: "c".into(),
                engine: "a".into(),
                template: "default.html".into(),
                parsed_url: Default::default(),
                img_src: "https://cdn.test/img.jpg".into(),
                thumbnail: String::new(),
                priority: String::new(),
                engines: vec!["a".into()],
                positions: vec![1],
                score: 1.0,
                category: "general".into(),
                published_date: None,
                favicon: String::new(),
                cluster: None,
                summary: None,
                highlights: Vec::new(),
                publisher_url: String::new(),
            },
            SearchResult {
                url: "https://no-image.test/".into(),
                title: "No Image".into(),
                content: "c".into(),
                engine: "a".into(),
                template: "default.html".into(),
                parsed_url: Default::default(),
                img_src: String::new(),
                thumbnail: String::new(),
                priority: String::new(),
                engines: vec!["a".into()],
                positions: vec![1],
                score: 1.0,
                category: "general".into(),
                published_date: None,
                favicon: String::new(),
                cluster: None,
                summary: None,
                highlights: Vec::new(),
                publisher_url: String::new(),
            },
        ];

        let config = RankingConfig::default();
        apply_extended_ranking(&mut results, "test", &config, false);

        let with_img = results.iter().find(|r| r.url.contains("with-image")).unwrap();
        let no_img = results.iter().find(|r| r.url.contains("no-image")).unwrap();
        // Image boost is 1.08, so result with image should have score 1.08.
        assert!(with_img.score > no_img.score);
        assert!((with_img.score - 1.08).abs() < 0.01);
    }

    #[test]
    fn parse_relative_time_works() {
        assert!((parse_relative_time("2 hours ago").unwrap() - 2.0).abs() < 0.01);
        assert!((parse_relative_time("3 days ago").unwrap() - 72.0).abs() < 0.01);
        assert!((parse_relative_time("1 week ago").unwrap() - 168.0).abs() < 0.01);
        assert!((parse_relative_time("yesterday").unwrap() - 24.0).abs() < 0.01);
        assert!(parse_relative_time("just now").unwrap() < 1.0);
        assert!(parse_relative_time("invalid string").is_none());
    }

    #[test]
    fn default_trusted_domains_includes_key_sites() {
        let trusted = default_trusted_domains();
        let domains: Vec<&str> = trusted.iter().map(|t| t.domain.as_str()).collect();

        // Key trusted domains should be present.
        assert!(domains.contains(&"wikipedia.org"));
        assert!(domains.contains(&"github.com"));
        assert!(domains.contains(&"stackoverflow.com"));
        assert!(domains.contains(&"arxiv.org"));

        // Low-quality domains should have weight < 1.0.
        let pinterest = trusted.iter().find(|t| t.domain == "pinterest.com").unwrap();
        assert!(pinterest.weight < 1.0);
    }

    #[test]
    fn cross_domain_dedup_penalty_applies() {
        // Two results from different domains with nearly identical titles.
        let mut results = vec![
            SearchResult {
                url: "https://news-site-a.test/article".into(),
                title: "Breaking News Major Event Happens Today".into(),
                content: "c".into(),
                engine: "a".into(),
                template: "default.html".into(),
                parsed_url: parsed_url("https://news-site-a.test/article"),
                img_src: String::new(),
                thumbnail: String::new(),
                priority: String::new(),
                engines: vec!["a".into()],
                positions: vec![1],
                score: 2.0,
                category: "news".into(),
                published_date: None,
                favicon: String::new(),
                cluster: None,
                summary: None,
                highlights: Vec::new(),
                publisher_url: String::new(),
            },
            SearchResult {
                url: "https://news-site-b.test/story".into(),
                title: "Breaking News Major Event Happens Today Updates".into(),
                content: "c".into(),
                engine: "b".into(),
                template: "default.html".into(),
                parsed_url: parsed_url("https://news-site-b.test/story"),
                img_src: String::new(),
                thumbnail: String::new(),
                priority: String::new(),
                engines: vec!["b".into()],
                positions: vec![1],
                score: 1.5,
                category: "news".into(),
                published_date: None,
                favicon: String::new(),
                cluster: None,
                summary: None,
                highlights: Vec::new(),
                publisher_url: String::new(),
            },
        ];

        let config = RankingConfig::default();
        apply_extended_ranking(&mut results, "breaking news", &config, true);

        // The second result (lower initial score, similar title) should be penalized.
        let second = results.iter().find(|r| r.url.contains("site-b")).unwrap();
        // Original score 1.5, after penalty should be 1.5 * 0.7 = 1.05.
        assert!(second.score < 1.5);
    }
}
