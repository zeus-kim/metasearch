//! Live AI end-to-end test against a real Ollama-compatible server.
//!
//! GATED: this test only does real work when `METASEARCH_AI_E2E=1` is set in
//! the environment (and an Ollama is reachable). Without the gate it returns
//! immediately, so the default `cargo test` stays hermetic and offline.
//!
//! Run it with a local Ollama:
//!
//! ```bash
//! # defaults: chat=llama3.2:3b embed=nomic-embed-text base=http://127.0.0.1:11434
//! METASEARCH_AI_E2E=1 cargo test --test ai_e2e -- --nocapture
//!
//! # override models / endpoint:
//! METASEARCH_AI_E2E=1 \
//!   METASEARCH_AI_E2E_MODEL=gemma3:4b \
//!   METASEARCH_AI_E2E_EMBED=bge-m3 \
//!   METASEARCH_AI_BASE_URL=http://127.0.0.1:11434 \
//!   cargo test --test ai_e2e -- --nocapture
//! ```
//!
//! It exercises, against the live model: (a) answer synthesis, (b) query
//! expansion, (c) semantic re-ranking via embeddings, (d) clustering, and
//! (e) graceful degradation when a model is missing.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use metasearch::ai;
use metasearch::config::AiSettings;
use metasearch::{EngineResult, SearchResult};

fn gate() -> bool {
    std::env::var("METASEARCH_AI_E2E").as_deref() == Ok("1")
}

fn base_url() -> String {
    std::env::var("METASEARCH_AI_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:11434".into())
}

fn chat_model() -> String {
    std::env::var("METASEARCH_AI_E2E_MODEL").unwrap_or_else(|_| "llama3.2:3b".into())
}

fn embed_model() -> String {
    std::env::var("METASEARCH_AI_E2E_EMBED").unwrap_or_else(|_| "nomic-embed-text".into())
}

fn ai_settings() -> AiSettings {
    AiSettings {
        enabled: true,
        base_url: base_url(),
        model: chat_model(),
        article_model: chat_model(),
        embedding_model: embed_model(),
        answer: true,
        expand: true,
        rerank: true,
        cluster: true,
        conversational: true,
        vision: false,
        vision_model: "llava".into(),
        answer_top_n: 5,
        timeout_secs: 60,
    }
}

/// Confirm Ollama is actually reachable before asserting live behaviour, so a
/// gated run on a machine without Ollama fails with a clear message.
async fn ollama_reachable(client: &reqwest::Client) -> bool {
    client
        .get(format!("{}/api/tags", base_url().trim_end_matches('/')))
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

/// A few fixture "search results" about a coherent topic so the model has real
/// material to synthesize / rank / cluster.
fn fixture_results() -> Vec<SearchResult> {
    let engine_results = vec![
        EngineResult::new(
            "https://tokio.rs/",
            "Tokio - An asynchronous Rust runtime",
            "Tokio is an asynchronous runtime for the Rust programming language. It provides \
             the building blocks needed for writing network applications and is one of the most \
             widely used async runtimes.",
        ),
        EngineResult::new(
            "https://docs.rs/async-std/latest/async_std/",
            "async-std - asynchronous standard library for Rust",
            "async-std is an async runtime that mirrors the Rust standard library API, offering \
             an alternative to Tokio for asynchronous programming.",
        ),
        EngineResult::new(
            "https://smol-rs.github.io/smol/",
            "smol - a small and fast async runtime for Rust",
            "smol is a small and fast async runtime for Rust. It is lightweight and can run \
             futures with minimal overhead.",
        ),
        EngineResult::new(
            "https://en.wikipedia.org/wiki/Rust_(programming_language)",
            "Rust (programming language) - Wikipedia",
            "Rust is a multi-paradigm, general-purpose programming language that emphasizes \
             performance, type safety, and concurrency.",
        ),
        EngineResult::new(
            "https://www.gnu.org/software/emacs/",
            "GNU Emacs",
            "An extensible, customizable, free/libre text editor and computing environment.",
        ),
    ];
    let per_engine = vec![("fixture".to_string(), engine_results)];
    let mut weights = HashMap::new();
    weights.insert("fixture".to_string(), 1.0);
    metasearch::aggregate::aggregate(per_engine, &weights)
}

#[tokio::test]
async fn ai_answer_synthesis_live() {
    if !gate() {
        eprintln!("ai_e2e: skipped (set METASEARCH_AI_E2E=1 to run against live Ollama)");
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

    let started = Instant::now();
    let answer = ai::synthesize_answer(
        &ai,
        &client,
        "what is the most popular async runtime for rust?",
        &results,
    )
    .await;
    let elapsed = started.elapsed();

    let answer = answer.expect("model should synthesize an answer from the fixture sources");
    assert!(!answer.answer.trim().is_empty(), "answer must be non-empty");
    assert!(!answer.answer.contains("NO_ANSWER"));
    assert_eq!(answer.engine, "ai");
    assert_eq!(answer.template, "answer/ai.html");
    eprintln!(
        "ai_e2e[answer] model={} latency={:?}\n--- synthesized answer ---\n{}\n--------------------------",
        chat_model(),
        elapsed,
        answer.answer
    );
}

#[tokio::test]
async fn ai_query_expansion_live() {
    if !gate() {
        eprintln!("ai_e2e: skipped");
        return;
    }
    let client = reqwest::Client::new();
    assert!(ollama_reachable(&client).await, "Ollama not reachable");
    let ai = ai_settings();

    let suggestions = ai::expand_query(&ai, &client, "rust web framework").await;
    eprintln!(
        "ai_e2e[expand] model={} suggestions={:?}",
        chat_model(),
        suggestions
    );
    assert!(
        !suggestions.is_empty(),
        "expansion should yield suggestions"
    );
    assert!(suggestions.len() <= 4, "expansion is capped at 4");
    for s in &suggestions {
        assert!(!s.trim().is_empty(), "each suggestion must be non-empty");
    }
}

#[tokio::test]
async fn ai_semantic_rerank_live() {
    if !gate() {
        eprintln!("ai_e2e: skipped");
        return;
    }
    let client = reqwest::Client::new();
    assert!(ollama_reachable(&client).await, "Ollama not reachable");
    let ai = ai_settings();
    let mut results = fixture_results();
    let before: Vec<String> = results.iter().map(|r| r.url.clone()).collect();

    let started = Instant::now();
    ai::rerank(&ai, &client, "async runtime for rust", &mut results, None).await;
    eprintln!(
        "ai_e2e[rerank] model={} latency={:?} order={:?}",
        embed_model(),
        started.elapsed(),
        results.iter().map(|r| r.url.clone()).collect::<Vec<_>>()
    );

    // Re-rank must preserve the result set (a permutation), never drop/dup.
    assert_eq!(results.len(), before.len());
    let after: std::collections::HashSet<String> = results.iter().map(|r| r.url.clone()).collect();
    for url in &before {
        assert!(after.contains(url), "rerank dropped {url}");
    }
    // The off-topic Emacs result should not be ranked first for an async query.
    assert!(
        !results[0].url.contains("emacs"),
        "off-topic result ranked first: {:?}",
        results[0].url
    );
}

#[tokio::test]
async fn ai_clustering_live() {
    if !gate() {
        eprintln!("ai_e2e: skipped");
        return;
    }
    let client = reqwest::Client::new();
    assert!(ollama_reachable(&client).await, "Ollama not reachable");
    let ai = ai_settings();
    let mut results = fixture_results();

    ai::cluster(&ai, &client, &mut results, None).await;
    let clusters: Vec<Option<u32>> = results.iter().map(|r| r.cluster).collect();
    eprintln!(
        "ai_e2e[cluster] model={} clusters={:?}",
        embed_model(),
        clusters
    );
    assert!(
        results.iter().all(|r| r.cluster.is_some()),
        "every result should receive a cluster id"
    );
}

#[tokio::test]
async fn ai_missing_model_degrades_live() {
    if !gate() {
        eprintln!("ai_e2e: skipped");
        return;
    }
    let client = reqwest::Client::new();
    assert!(ollama_reachable(&client).await, "Ollama not reachable");
    // A bogus model name → Ollama errors → every feature degrades to None/no-op,
    // bounded by the configured timeout (never hangs).
    let mut ai = ai_settings();
    ai.model = "this-model-does-not-exist:0b".into();
    ai.embedding_model = "this-embed-does-not-exist:0b".into();
    ai.timeout_secs = 10;
    let results = fixture_results();

    let started = Instant::now();
    let answer = ai::synthesize_answer(&ai, &client, "anything", &results).await;
    assert!(answer.is_none(), "missing model must degrade to no answer");
    assert!(ai::expand_query(&ai, &client, "anything").await.is_empty());

    let mut to_rank = results.clone();
    let before: Vec<String> = to_rank.iter().map(|r| r.url.clone()).collect();
    ai::rerank(&ai, &client, "anything", &mut to_rank, None).await;
    let after: Vec<String> = to_rank.iter().map(|r| r.url.clone()).collect();
    assert_eq!(
        before, after,
        "rerank must leave order untouched when embeddings fail"
    );

    assert!(
        started.elapsed() < Duration::from_secs(40),
        "degradation must be bounded, took {:?}",
        started.elapsed()
    );
    eprintln!(
        "ai_e2e[degrade] missing-model handled gracefully in {:?}",
        started.elapsed()
    );
}
