//! Versioned HTTP API for agents and integrations (`/api/v1/*`).
//!
//! The primary agent surface is [`ResearchRequest`] → [`ResearchResponse`] via
//! `POST /api/v1/research` (JSON body) or `GET /api/v1/research?q=…`. Streaming
//! variants emit SSE with a `results` event before tokens.

use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::ai::Citation;
use crate::ai::{self, FocusMode};
use crate::config::Settings;
use crate::search::{search_all, Runtime, SearchParams, SearchResponse};
use crate::types::SearchResult;

/// Agent-friendly search result (stable field names for tool clients).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentResult {
    pub engine: String,
    pub url: String,
    pub title: String,
    pub content: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub snippet: String,
    pub score: f64,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub category: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub img_src: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub thumbnail: String,
    #[serde(rename = "publishedDate", skip_serializing_if = "Option::is_none", default)]
    pub published_date: Option<String>,
}

impl From<&SearchResult> for AgentResult {
    fn from(r: &SearchResult) -> Self {
        AgentResult {
            engine: r.engine.clone(),
            url: r.url.clone(),
            title: r.title.clone(),
            content: r.content.clone(),
            snippet: r.content.clone(),
            score: r.score,
            category: r.category.clone(),
            img_src: r.img_src.clone(),
            thumbnail: r.thumbnail.clone(),
            published_date: r.published_date.clone(),
        }
    }
}

/// Stable JSON search response for `GET /api/v1/search`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSearchResponse {
    pub query: String,
    pub number_of_results: usize,
    pub pageno: usize,
    pub results: Vec<AgentResult>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub answers: Vec<crate::types::Answer>,
    pub unresponsive_engines: Vec<(String, String)>,
}

/// Non-streaming grounded answer for `GET /api/v1/answer`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentAnswerResponse {
    pub query: String,
    pub number_of_results: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    pub citations: Vec<Citation>,
    /// Top sources (same shape as citations; alias for agent clients).
    pub sources: Vec<Citation>,
    pub followups: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Health payload for `GET /api/v1/health`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentHealthResponse {
    pub status: String,
    pub uptime_secs: u64,
    pub engine_health_enabled: bool,
    pub cooling_down: Vec<String>,
}

/// Convert a [`SearchResponse`] into the agent search schema.
pub fn agent_search_response(response: &SearchResponse) -> AgentSearchResponse {
    AgentSearchResponse {
        query: response.query.clone(),
        number_of_results: response.number_of_results,
        pageno: response.pageno,
        results: response.results.iter().map(AgentResult::from).collect(),
        answers: response.answers.clone(),
        unresponsive_engines: response.unresponsive_engines.clone(),
    }
}

/// Run search and return the agent JSON schema.
pub async fn run_agent_search(
    params: &SearchParams,
    settings: &Settings,
    rt: &Runtime,
) -> AgentSearchResponse {
    let response = search_all(params, settings, rt).await;
    agent_search_response(&response)
}

/// Run search + one-shot answer synthesis for agents.
pub async fn run_agent_answer(
    params: &SearchParams,
    settings: &Settings,
    rt: &Runtime,
    include_followups: bool,
    focus: crate::ai::FocusMode,
    model: Option<&str>,
) -> AgentAnswerResponse {
    let mut params = params.clone();
    params.ai_answer = Some(false);
    let response = search_all(&params, settings, rt).await;
    let citations = ai::build_citations(&response.results, settings.ai.answer_top_n);
    let sources = citations.clone();

    let mut answer = None;
    let mut error = None;
    let mut followups = Vec::new();

    if !settings.ai.enabled {
        error = Some("AI disabled (set ai.enabled / ai.base_url)".into());
    } else if response.results.is_empty() {
        error = Some("no results to summarize".into());
    } else if let Ok(text) = ai::stream_answer_collect(
        &settings.ai,
        &rt.client,
        &response.query,
        &response.results,
        focus,
        model,
    )
    .await
    {
        answer = Some(ai::append_citation_footnotes(&text, &citations));
    } else {
        error = Some("model unreachable or synthesis failed".into());
    }

    if include_followups && settings.ai.enabled && !response.results.is_empty() {
        followups =
            ai::suggest_followups(&settings.ai, &rt.client, &response.query, &response.results)
                .await;
    }

    AgentAnswerResponse {
        query: response.query,
        number_of_results: response.number_of_results,
        answer,
        citations,
        sources,
        followups,
        error,
    }
}

/// Agent-oriented research request (one call = search + optional synthesis).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ResearchRequest {
    pub query: String,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub conversation: Option<ConversationContext>,
    #[serde(default)]
    pub stream: bool,
    /// Synthesis preset: `general` | `academic` | `writing` | `code`.
    #[serde(default)]
    pub focus: Option<String>,
    /// Override chat model for this request (Ollama tag).
    #[serde(default)]
    pub model: Option<String>,
    /// Semantic re-rank via embeddings (`1` / `true` forces on when AI enabled).
    #[serde(default)]
    pub rerank: Option<bool>,
    /// Include follow-up question suggestions.
    #[serde(default = "default_true")]
    pub followups: bool,
    /// Multi-hop deep research: plan sub-queries, merge results (cap 40).
    #[serde(default)]
    pub deep: Option<bool>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ConversationContext {
    pub previous_query: String,
    #[serde(default)]
    pub previous_answer: String,
}

/// Non-streaming research response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchResponse {
    pub query: String,
    pub results: Vec<SearchResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    pub citations: Vec<Citation>,
    pub followups: Vec<String>,
    pub engines_used: Vec<String>,
    pub latency_ms: u64,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub deep_subqueries: Vec<String>,
}

/// Parse a research request from a JSON body or URL-encoded query string.
pub fn parse_research_request(body: &str, is_json: bool) -> Result<ResearchRequest, String> {
    if is_json {
        serde_json::from_str(body).map_err(|e| format!("invalid JSON: {e}"))
    } else {
        let q = form_param(body, "q");
        if q.is_empty() {
            return Err("missing query (use `query` in JSON or `q` in query string)".into());
        }
        let categories: Vec<String> = form_param(body, "categories")
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let stream = matches!(
            form_param(body, "stream").as_str(),
            "1" | "true" | "on" | "yes"
        );
        let rerank = parse_optional_bool(form_param(body, "rerank").as_str());
        let deep = parse_optional_bool(form_param(body, "deep").as_str());
        let followups = !matches!(
            form_param(body, "followups").as_str(),
            "0" | "false" | "off" | "no"
        );
        let prev_q = form_param(body, "previous_query");
        let prev_a = form_param(body, "prev_answer");
        let conversation = if prev_q.is_empty() {
            None
        } else {
            Some(ConversationContext {
                previous_query: prev_q,
                previous_answer: prev_a,
            })
        };
        Ok(ResearchRequest {
            query: q,
            categories,
            conversation,
            stream,
            focus: opt_str(form_param(body, "focus")),
            model: opt_str(form_param(body, "model")),
            rerank,
            deep,
            followups,
        })
    }
}

fn opt_str(s: String) -> Option<String> {
    if s.trim().is_empty() {
        None
    } else {
        Some(s)
    }
}

fn parse_optional_bool(s: &str) -> Option<bool> {
    match s {
        "" => None,
        "1" | "true" | "on" | "yes" => Some(true),
        "0" | "false" | "off" | "no" => Some(false),
        _ => None,
    }
}

fn form_param(query: &str, key: &str) -> String {
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

/// Build [`SearchParams`] from a research request.
pub fn research_to_search_params(req: &ResearchRequest) -> SearchParams {
    let mut context = None;
    if let Some(conv) = &req.conversation {
        if !conv.previous_answer.trim().is_empty() {
            let snippet: String = conv.previous_answer.chars().take(600).collect();
            context = Some(
                format!("{}\nPrevious answer: {snippet}", conv.previous_query.trim())
                    .trim()
                    .to_string(),
            );
        } else if !conv.previous_query.trim().is_empty() {
            context = Some(conv.previous_query.clone());
        }
    }
    SearchParams {
        query: req.query.clone(),
        categories: req.categories.clone(),
        ai_answer: Some(false),
        context,
        rerank: req.rerank.or_else(|| req.deep.filter(|&d| d).map(|_| true)),
        deep: req.deep,
        ..Default::default()
    }
}

/// Engines that contributed results or were attempted (from per-request timings).
pub fn engines_used(response: &SearchResponse) -> Vec<String> {
    let mut names: Vec<String> = response.timings.iter().map(|t| t.engine.clone()).collect();
    names.sort();
    names.dedup();
    names
}

/// Run the full research pipeline (search + optional one-shot answer).
pub async fn run_research(
    req: &ResearchRequest,
    settings: &Settings,
    rt: &Runtime,
) -> ResearchResponse {
    let started = Instant::now();
    let focus = FocusMode::parse(req.focus.as_deref().unwrap_or("general"));
    let model = req.model.as_deref();
    let params = research_to_search_params(req);
    let settings = settings.clone();
    let response = search_all(&params, &settings, rt).await;
    let citations = ai::build_citations(&response.results, settings.ai.answer_top_n);
    let engines = engines_used(&response);

    let mut answer = None;
    let mut followups = Vec::new();

    if settings.ai.enabled && !response.results.is_empty() {
        if let Ok(text) = ai::stream_answer_collect(
            &settings.ai,
            &rt.client,
            &response.query,
            &response.results,
            focus,
            model,
        )
        .await
        {
            answer = Some(ai::append_citation_footnotes(&text, &citations));
        }
        if req.followups {
            followups =
                ai::suggest_followups(&settings.ai, &rt.client, &response.query, &response.results)
                    .await;
        }
    }

    ResearchResponse {
        query: response.query,
        results: response.results,
        answer,
        citations,
        followups,
        engines_used: engines,
        latency_ms: started.elapsed().as_millis() as u64,
        deep_subqueries: response.deep_subqueries,
    }
}

/// List models from the configured Ollama-compatible host (safe proxy).
pub async fn fetch_models(settings: &Settings, client: &reqwest::Client) -> serde_json::Value {
    let base = settings.ai.base_url.trim_end_matches('/');
    let is_openai = base.contains("openai.com") || base.contains("api.openai");
    let url = if is_openai {
        format!("{base}/models")
    } else {
        format!("{base}/api/tags")
    };
    let mut req = client.get(&url).timeout(std::time::Duration::from_secs(5));
    if is_openai {
        if let Some(key) = &settings.ai.api_key {
            if !key.is_empty() {
                req = req.header("Authorization", format!("Bearer {}", key));
            }
        }
    }
    match req.send().await {
        Ok(r) if r.status().is_success() => {
            let json: serde_json::Value = r.json().await.unwrap_or_else(
                |_| serde_json::json!({ "models": [], "error": "invalid upstream JSON" }),
            );
            if is_openai {
                if let Some(data) = json.get("data").and_then(|d| d.as_array()) {
                    let models: Vec<_> = data.iter()
                        .filter_map(|m| m.get("id").and_then(|v| v.as_str()))
                        .map(|id| serde_json::json!({"name": id, "model": id}))
                        .collect();
                    return serde_json::json!({ "models": models });
                }
            }
            json
        }
        Ok(r) => serde_json::json!({
            "models": [],
            "error": format!("upstream status {}", r.status()),
            "base_url": base,
        }),
        Err(e) => serde_json::json!({
            "models": [],
            "error": format!("unreachable: {e}"),
            "base_url": base,
        }),
    }
}

/// Engine inventory with category matrix for `/api/v1/engines`.
pub fn engines_matrix(settings: &Settings) -> serde_json::Value {
    let mut engines: Vec<serde_json::Value> = settings
        .engines
        .iter()
        .map(|e| {
            serde_json::json!({
                "name": e.name,
                "enabled": e.enabled,
                "weight": e.weight,
                "categories": e.categories,
                "kind": "native",
            })
        })
        .collect();
    for e in &settings.custom_engines {
        engines.push(serde_json::json!({
            "name": e.name,
            "enabled": e.enabled,
            "weight": e.weight,
            "categories": e.categories,
            "kind": e.kind,
        }));
    }
    engines.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or("")
            .cmp(b["name"].as_str().unwrap_or(""))
    });
    serde_json::json!({
        "categories": settings.categories(),
        "engines": engines,
        "enabled_count": engines.iter().filter(|e| e["enabled"].as_bool() == Some(true)).count(),
    })
}

/// OpenAPI 3.0 description for `/api/v1/*`.
pub fn openapi_spec(settings: &Settings) -> serde_json::Value {
    let host = if settings.server.bind_address == "0.0.0.0" {
        "127.0.0.1"
    } else {
        settings.server.bind_address.as_str()
    };
    let port = settings.server.port;
    serde_json::json!({
        "openapi": "3.0.3",
        "info": {
            "title": "metasearch API",
            "version": "1.0.0",
            "description": "Agent-first research API: one call runs metasearch + optional local Ollama synthesis. Primary integration surface for MCP and automation."
        },
        "servers": [{ "url": format!("http://{host}:{port}") }],
        "paths": {
            "/api/v1/search": {
                "get": {
                    "summary": "Agent search (stable JSON schema)",
                    "parameters": [
                        { "name": "q", "in": "query", "required": true, "schema": { "type": "string" } },
                        { "name": "categories", "in": "query", "schema": { "type": "string", "description": "Comma-separated: general,images,videos,..." } },
                        { "name": "pageno", "in": "query", "schema": { "type": "integer", "minimum": 1 } },
                        { "name": "language", "in": "query", "schema": { "type": "string" } },
                        { "name": "safesearch", "in": "query", "schema": { "type": "integer", "enum": [0, 1, 2] } },
                        { "name": "rerank", "in": "query", "schema": { "type": "boolean" } }
                    ],
                    "responses": { "200": { "description": "AgentSearchResponse", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/AgentSearchResponse" } } } } }
                }
            },
            "/api/v1/answer": {
                "get": {
                    "summary": "Grounded answer (non-streaming JSON)",
                    "parameters": [
                        { "name": "q", "in": "query", "required": true, "schema": { "type": "string" } },
                        { "name": "categories", "in": "query", "schema": { "type": "string" } },
                        { "name": "followups", "in": "query", "schema": { "type": "boolean" } },
                        { "name": "focus", "in": "query", "schema": { "type": "string", "enum": ["general", "academic", "writing", "code"] } },
                        { "name": "model", "in": "query", "schema": { "type": "string" } }
                    ],
                    "responses": { "200": { "description": "AgentAnswerResponse" } }
                }
            },
            "/api/v1/followups": {
                "get": {
                    "summary": "Suggested follow-up questions",
                    "parameters": [
                        { "name": "q", "in": "query", "required": true, "schema": { "type": "string" } },
                        { "name": "categories", "in": "query", "schema": { "type": "string" } }
                    ],
                    "responses": { "200": { "description": "Follow-ups JSON" } }
                }
            },
            "/api/v1/health": {
                "get": {
                    "summary": "Health + cooling engines",
                    "responses": { "200": { "description": "AgentHealthResponse" } }
                }
            },
            "/api/v1/research": {
                "get": {
                    "summary": "Research (query string)",
                    "parameters": [
                        { "name": "q", "in": "query", "required": true, "schema": { "type": "string" } },
                        { "name": "stream", "in": "query", "schema": { "type": "boolean" } },
                        { "name": "focus", "in": "query", "schema": { "type": "string", "enum": ["general", "academic", "writing", "code"] } },
                        { "name": "rerank", "in": "query", "schema": { "type": "boolean" } },
                        { "name": "deep", "in": "query", "schema": { "type": "boolean", "description": "Multi-hop deep research (plan sub-queries, merge results)" } },
                        { "name": "categories", "in": "query", "schema": { "type": "string" } }
                    ],
                    "responses": { "200": { "description": "ResearchResponse JSON or SSE when stream=true" } }
                },
                "post": {
                    "summary": "Research (JSON body)",
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/json": {
                                "schema": { "$ref": "#/components/schemas/ResearchRequest" }
                            }
                        }
                    },
                    "responses": { "200": { "description": "ResearchResponse JSON or SSE when stream=true" } }
                }
            },
            "/api/v1/models": {
                "get": {
                    "summary": "List Ollama models (proxied from ai.base_url)",
                    "responses": { "200": { "description": "Ollama /api/tags shape" } }
                }
            },
            "/api/v1/engines": {
                "get": {
                    "summary": "Engine inventory and category matrix",
                    "responses": { "200": { "description": "Engines JSON" } }
                }
            },
            "/search": {
                "get": {
                    "summary": "compatible search",
                    "parameters": [
                        { "name": "q", "in": "query", "required": true, "schema": { "type": "string" } },
                        { "name": "format", "in": "query", "schema": { "type": "string", "enum": ["json", "html", "rss", "csv"] } },
                        { "name": "rerank", "in": "query", "schema": { "type": "boolean" } }
                    ]
                }
            },
            "/answer": {
                "get": {
                    "summary": "Streaming grounded answer (SSE)",
                    "parameters": [
                        { "name": "q", "in": "query", "required": true, "schema": { "type": "string" } },
                        { "name": "focus", "in": "query", "schema": { "type": "string" } },
                        { "name": "model", "in": "query", "schema": { "type": "string" } },
                        { "name": "deep", "in": "query", "schema": { "type": "boolean" } }
                    ]
                }
            }
        },
        "components": {
            "schemas": {
                "AgentResult": {
                    "type": "object",
                    "properties": {
                        "engine": { "type": "string" },
                        "url": { "type": "string", "format": "uri" },
                        "title": { "type": "string" },
                        "content": { "type": "string" },
                        "snippet": { "type": "string" },
                        "score": { "type": "number" },
                        "category": { "type": "string" },
                        "img_src": { "type": "string" },
                        "thumbnail": { "type": "string" }
                    }
                },
                "AgentSearchResponse": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" },
                        "number_of_results": { "type": "integer" },
                        "pageno": { "type": "integer" },
                        "results": { "type": "array", "items": { "$ref": "#/components/schemas/AgentResult" } },
                        "unresponsive_engines": { "type": "array", "items": { "type": "array", "items": { "type": "string" } } }
                    }
                },
                "AgentAnswerResponse": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" },
                        "number_of_results": { "type": "integer" },
                        "answer": { "type": "string" },
                        "citations": { "type": "array" },
                        "sources": { "type": "array" },
                        "followups": { "type": "array", "items": { "type": "string" } },
                        "error": { "type": "string" }
                    }
                },
                "ResearchRequest": {
                    "type": "object",
                    "required": ["query"],
                    "properties": {
                        "query": { "type": "string" },
                        "categories": { "type": "array", "items": { "type": "string" } },
                        "conversation": {
                            "type": "object",
                            "properties": {
                                "previous_query": { "type": "string" },
                                "previous_answer": { "type": "string" }
                            }
                        },
                        "stream": { "type": "boolean" },
                        "focus": { "type": "string" },
                        "model": { "type": "string" },
                        "rerank": { "type": "boolean" },
                        "followups": { "type": "boolean" },
                        "deep": { "type": "boolean" }
                    }
                },
                "ResearchResponse": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" },
                        "results": { "type": "array" },
                        "answer": { "type": "string" },
                        "citations": { "type": "array" },
                        "followups": { "type": "array", "items": { "type": "string" } },
                        "engines_used": { "type": "array", "items": { "type": "string" } },
                        "latency_ms": { "type": "integer" }
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_get_research() {
        let req = parse_research_request("q=rust+async&stream=1&focus=code", false).unwrap();
        assert_eq!(req.query, "rust async");
        assert!(req.stream);
        assert_eq!(req.focus.as_deref(), Some("code"));
    }

    #[test]
    fn openapi_includes_agent_routes() {
        let spec = openapi_spec(&Settings::default());
        assert!(spec["paths"]["/api/v1/search"].is_object());
        assert!(spec["paths"]["/api/v1/answer"].is_object());
        assert!(spec["paths"]["/api/v1/health"].is_object());
    }

    #[test]
    fn agent_result_maps_search_fields() {
        use crate::types::SearchResult;
        let r = SearchResult {
            url: "https://example.com".into(),
            title: "Example".into(),
            content: "snippet text".into(),
            engine: "wikipedia".into(),
            template: "default.html".into(),
            parsed_url: Default::default(),
            img_src: String::new(),
            thumbnail: String::new(),
            priority: String::new(),
            engines: vec!["wikipedia".into()],
            positions: vec![1],
            score: 1.5,
            category: "general".into(),
            published_date: None,
            favicon: String::new(),
            cluster: None,
            summary: None,
            highlights: Vec::new(),
            publisher_url: String::new(),
        };
        let a = AgentResult::from(&r);
        assert_eq!(a.engine, "wikipedia");
        assert_eq!(a.snippet, "snippet text");
        assert_eq!(a.score, 1.5);
    }

    #[test]
    fn parse_json_research() {
        let body =
            r#"{"query":"hello","conversation":{"previous_query":"a","previous_answer":"b"}}"#;
        let req = parse_research_request(body, true).unwrap();
        assert_eq!(req.query, "hello");
        assert!(req.conversation.is_some());
    }
}
