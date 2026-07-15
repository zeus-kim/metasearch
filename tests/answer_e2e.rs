//! Live end-to-end tests for the answer-style answer engine core.
//!
//! GATED: real work only runs with `METASEARCH_AI_E2E=1` (and a reachable
//! Ollama). Without the gate each test returns immediately, so the default
//! `cargo test` stays hermetic and offline.
//!
//! These exercise the exact `ai::stream_answer` / `ai::build_citations` /
//! `ai::suggest_followups` path that the streaming `/answer` SSE endpoint and
//! the `ask` CLI use, against fixture sources (no live search network), so they
//! isolate the model behaviour from flaky upstream engines.
//!
//! ```bash
//! METASEARCH_AI_E2E=1 cargo test --test answer_e2e -- --nocapture
//! # override model / endpoint:
//! METASEARCH_AI_E2E=1 METASEARCH_AI_E2E_MODEL=gemma3:4b \
//!   METASEARCH_AI_BASE_URL=http://127.0.0.1:11434 \
//!   cargo test --test answer_e2e -- --nocapture
//! ```

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use metasearch::config::AiSettings;
use metasearch::{ai, EngineResult, SearchResult};

fn gate() -> bool {
    std::env::var("METASEARCH_AI_E2E").as_deref() == Ok("1")
}

fn base_url() -> String {
    std::env::var("METASEARCH_AI_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:11434".into())
}

fn chat_model() -> String {
    std::env::var("METASEARCH_AI_E2E_MODEL").unwrap_or_else(|_| "llama3.2:3b".into())
}

fn ai_settings() -> AiSettings {
    AiSettings {
        enabled: true,
        base_url: base_url(),
        model: chat_model(),
        answer: true,
        answer_top_n: 5,
        timeout_secs: 60,
        ..AiSettings::default()
    }
}

async fn ollama_reachable(client: &reqwest::Client) -> bool {
    client
        .get(format!("{}/api/tags", base_url().trim_end_matches('/')))
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

fn fixture_results() -> Vec<SearchResult> {
    let engine_results = vec![
        EngineResult::new(
            "https://tokio.rs/",
            "Tokio - An asynchronous Rust runtime",
            "Tokio is an asynchronous runtime for the Rust programming language. It is one of the \
             most widely used async runtimes and provides the building blocks for network apps.",
        ),
        EngineResult::new(
            "https://docs.rs/async-std/latest/async_std/",
            "async-std - asynchronous standard library for Rust",
            "async-std is an async runtime that mirrors the Rust standard library API, offering \
             an alternative to Tokio.",
        ),
        EngineResult::new(
            "https://smol-rs.github.io/smol/",
            "smol - a small and fast async runtime for Rust",
            "smol is a small and fast async runtime for Rust, lightweight with minimal overhead.",
        ),
    ];
    let per_engine = vec![("fixture".to_string(), engine_results)];
    let mut weights = HashMap::new();
    weights.insert("fixture".to_string(), 1.0);
    metasearch::aggregate::aggregate(per_engine, &weights)
}

#[tokio::test]
async fn stream_answer_live() {
    if !gate() {
        eprintln!("answer_e2e: skipped (set METASEARCH_AI_E2E=1 to run against live Ollama)");
        return;
    }
    let client = reqwest::Client::new();
    assert!(
        ollama_reachable(&client).await,
        "METASEARCH_AI_E2E=1 but Ollama is not reachable at {}",
        base_url()
    );
    let ai = ai_settings();
    let results = fixture_results();
    let citations = ai::build_citations(&results, ai.answer_top_n);
    assert_eq!(citations.len(), results.len());

    // Capture streamed tokens; the callback must fire incrementally.
    let tokens = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = tokens.clone();
    let started = Instant::now();
    let full = ai::stream_answer(
        &ai,
        &client,
        "fastest async runtime for rust",
        &results,
        |t| {
            sink.lock().unwrap().push(t.to_string());
        },
    )
    .await
    .expect("model should stream a grounded answer from the fixture sources")
    .article;
    let elapsed = started.elapsed();

    let chunks = tokens.lock().unwrap().clone();
    assert!(!chunks.is_empty(), "must emit at least one streamed token");
    assert!(
        !full.trim().is_empty(),
        "accumulated answer must be non-empty"
    );
    // The streamed deltas must reconstruct the returned answer.
    assert_eq!(chunks.concat().trim(), full.trim());
    assert!(!full.contains("NO_ANSWER"));

    eprintln!(
        "answer_e2e[stream] model={} latency={:?} tokens={} \n--- answer ---\n{}\n--- citations ---",
        chat_model(),
        elapsed,
        chunks.len(),
        full
    );
    for c in &citations {
        eprintln!("[{}] {} — {}", c.index, c.title, c.url);
    }
}

#[tokio::test]
async fn suggest_followups_live() {
    if !gate() {
        eprintln!("answer_e2e: skipped");
        return;
    }
    let client = reqwest::Client::new();
    assert!(ollama_reachable(&client).await, "Ollama not reachable");
    let ai = ai_settings();
    let results = fixture_results();

    let fu = ai::suggest_followups(&ai, &client, "rust async runtimes", &results).await;
    eprintln!("answer_e2e[followups] model={} => {:?}", chat_model(), fu);
    assert!(!fu.is_empty(), "should propose at least one follow-up");
    assert!(fu.len() <= 5, "follow-ups are capped at 5");
    for q in &fu {
        assert!(!q.trim().is_empty());
    }
}

#[tokio::test]
async fn stream_answer_missing_model_degrades_live() {
    if !gate() {
        eprintln!("answer_e2e: skipped");
        return;
    }
    let client = reqwest::Client::new();
    assert!(ollama_reachable(&client).await, "Ollama not reachable");
    let mut ai = ai_settings();
    ai.model = "this-model-does-not-exist:0b".into();
    ai.timeout_secs = 10;
    let results = fixture_results();

    let mut tokens = 0usize;
    let started = Instant::now();
    let out = ai::stream_answer(&ai, &client, "anything", &results, |_| tokens += 1).await;
    assert!(out.is_err(), "missing model must yield a clear error");
    assert_eq!(tokens, 0, "no tokens on a missing model");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "degradation must be bounded, took {:?}",
        started.elapsed()
    );
    eprintln!(
        "answer_e2e[degrade] missing model handled in {:?}",
        started.elapsed()
    );
}
