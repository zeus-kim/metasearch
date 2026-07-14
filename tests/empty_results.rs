//! Empty-result detection and SSRF regression tests.

use metasearch::engines;
use metasearch::health::{FailureClass, HealthTracker};

#[test]
fn scraper_engines_are_empty_sensitive() {
    assert!(engines::empty_result_sensitive("brave"));
    assert!(engines::empty_result_sensitive("duckduckgo_images"));
    assert!(engines::empty_result_sensitive("bing_images"));
    assert!(!engines::empty_result_sensitive("openverse"));
    assert!(!engines::empty_result_sensitive("wikipedia"));
    assert!(!engines::empty_result_sensitive("github"));
}

fn cooling(ht: &HealthTracker, engine: &str) -> bool {
    ht.info(engine).map(|i| i.cooling_down).unwrap_or(false)
}

#[test]
fn consecutive_empty_results_trip_cooldown() {
    let ht = HealthTracker::new(3, 60);
    for _ in 0..2 {
        assert!(!ht.record_failure("brave", FailureClass::EmptyResults));
    }
    assert!(ht.record_failure("brave", FailureClass::EmptyResults));
    assert!(cooling(&ht, "brave"));
}

#[test]
fn parse_failures_do_not_trip_cooldown_alone() {
    let ht = HealthTracker::new(3, 60);
    for _ in 0..5 {
        ht.record_failure("brave", FailureClass::Parse);
    }
    assert!(!cooling(&ht, "brave"));
}

#[test]
fn image_proxy_blocks_private_urls() {
    use metasearch::url_safety::is_safe_public_url;
    assert!(!is_safe_public_url("http://127.0.0.1/secret"));
    assert!(!is_safe_public_url(
        "http://169.254.169.254/latest/meta-data/"
    ));
}
