//! Hermetic tests for multi-hop deep research (no live Ollama / network).

use metasearch::aggregate;
use metasearch::{search_all, Runtime, SearchParams, Settings};

#[test]
fn merge_aggregated_dedupes_across_hops() {
    use metasearch::types::SearchResult;
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
    let merged = aggregate::merge_aggregated(
        vec![
            vec![mk("https://shared.test/", 2.0), mk("https://a.test/", 1.0)],
            vec![mk("https://shared.test/", 3.0), mk("https://b.test/", 0.5)],
        ],
        40,
    );
    assert_eq!(merged.len(), 3);
    assert_eq!(merged[0].url, "https://shared.test/");
    assert!((merged[0].score - 5.0).abs() < 1e-9);
}

#[tokio::test]
async fn deep_zero_matches_standard_search_shape() {
    let settings = Settings::default();
    let rt = Runtime::new(&settings);
    let params = SearchParams::new("rust programming language");
    let normal = search_all(&params, &settings, &rt).await;
    let mut deep_off = params.clone();
    deep_off.deep = Some(false);
    let again = search_all(&deep_off, &settings, &rt).await;
    assert_eq!(normal.query, again.query);
    assert_eq!(normal.number_of_results, again.number_of_results);
    assert!(again.deep_subqueries.is_empty());
}

#[tokio::test]
async fn deep_without_ollama_degrades_to_single_hop() {
    let mut settings = Settings::default();
    settings.ai.enabled = true;
    settings.ai.base_url = "http://127.0.0.1:1".into();
    settings.ai.timeout_secs = 1;
    let rt = Runtime::new(&settings);
    let mut params = SearchParams::new("tokio rust runtime");
    params.deep = Some(true);
    let started = std::time::Instant::now();
    let resp = search_all(&params, &settings, &rt).await;
    assert!(
        started.elapsed() < std::time::Duration::from_secs(120),
        "deep search must complete without hanging"
    );
    assert!(resp.deep_subqueries.is_empty());
    assert!(!resp.query.is_empty());
}
