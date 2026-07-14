//! MCP stdio server exposing metasearch tools for Cursor / Claude Desktop.
//!
//! No query logging. Uses the library API directly (no HTTP port required).
//!
//! ```bash
//! cargo run --bin metasearch-mcp
//! ```

use std::io::{self, BufRead, Write};
use std::sync::Arc;

use metasearch::ai::FocusMode;
use metasearch::api::{self, ResearchRequest};
use metasearch::{search_all, Runtime, SearchParams, Settings};

#[derive(serde::Deserialize)]
struct RpcRequest {
    id: Option<serde_json::Value>,
    method: String,
    #[serde(default)]
    params: serde_json::Value,
}

#[tokio::main]
async fn main() {
    let path = std::env::var("METASEARCH_SETTINGS")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("settings.yml"));
    let settings = Settings::load_or_default(&path);
    let rt = Arc::new(Runtime::new(&settings));
    let settings = Arc::new(settings);

    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let req: RpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let _ = write_response(
                    &mut stdout,
                    None,
                    serde_json::json!({ "error": { "code": -32700, "message": format!("parse error: {e}") } }),
                );
                continue;
            }
        };
        let result = dispatch(&req.method, &req.params, &settings, &rt).await;
        let _ = write_response(&mut stdout, req.id, result);
    }
}

fn write_response(
    out: &mut impl Write,
    id: Option<serde_json::Value>,
    result: serde_json::Value,
) -> io::Result<()> {
    let msg = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
    writeln!(out, "{msg}")?;
    out.flush()
}

async fn dispatch(
    method: &str,
    params: &serde_json::Value,
    settings: &Settings,
    rt: &Runtime,
) -> serde_json::Value {
    match method {
        "initialize" => serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "metasearch-mcp", "version": env!("CARGO_PKG_VERSION") }
        }),
        "notifications/initialized" | "initialized" => serde_json::json!({}),
        "tools/list" => serde_json::json!({ "tools": tool_schemas() }),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or("");
            let args = &params["arguments"];
            match call_tool(name, args, settings, rt).await {
                Ok(text) => serde_json::json!({
                    "content": [{ "type": "text", "text": text }],
                    "isError": false
                }),
                Err(e) => serde_json::json!({
                    "content": [{ "type": "text", "text": e }],
                    "isError": true
                }),
            }
        }
        "ping" => serde_json::json!({}),
        _ => {
            serde_json::json!({ "error": { "code": -32601, "message": format!("unknown method: {method}") } })
        }
    }
}

fn tool_schemas() -> Vec<serde_json::Value> {
    vec![
        tool(
            "metasearch_search",
            "Run a metasearch query and return ranked JSON results (standard JSON shape).",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search query (supports !bangs)" },
                    "categories": { "type": "array", "items": { "type": "string" }, "description": "e.g. general, images, science" },
                    "rerank": { "type": "boolean", "description": "Semantic re-rank when AI enabled" }
                },
                "required": ["query"]
            }),
        ),
        tool(
            "metasearch_answer",
            "Search + synthesize a grounded cited answer (non-streaming).",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "focus": { "type": "string", "enum": ["general", "academic", "writing", "code"] },
                    "model": { "type": "string", "description": "Ollama model override" }
                },
                "required": ["query"]
            }),
        ),
        tool(
            "metasearch_image_search",
            "Search image engines (wikicommons, duckduckgo_images, bing_images, openverse).",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" }
                },
                "required": ["query"]
            }),
        ),
        tool(
            "metasearch_research",
            "Agent research API: search + answer + citations + follow-ups in one call.",
            serde_json::json!({
                "type": "object",
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
                    "focus": { "type": "string" },
                    "model": { "type": "string" },
                    "rerank": { "type": "boolean" },
                    "followups": { "type": "boolean" }
                },
                "required": ["query"]
            }),
        ),
        tool(
            "metasearch_status",
            "Check metasearch configuration and health status. Use this to help users troubleshoot or verify their setup.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        ),
    ]
}

fn tool(name: &str, description: &str, schema: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "description": description,
        "inputSchema": schema
    })
}

async fn call_tool(
    name: &str,
    args: &serde_json::Value,
    settings: &Settings,
    rt: &Runtime,
) -> Result<String, String> {
    match name {
        "metasearch_search" => {
            let query = args["query"].as_str().ok_or("missing query")?;
            let categories: Vec<String> = args["categories"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let rerank = args["rerank"].as_bool();
            let params = SearchParams {
                query: query.to_string(),
                categories,
                rerank,
                ..Default::default()
            };
            let resp = search_all(&params, settings, rt).await;
            serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
        }
        "metasearch_image_search" => {
            let query = args["query"].as_str().ok_or("missing query")?;
            let params = SearchParams {
                query: query.to_string(),
                categories: vec!["images".into()],
                ..Default::default()
            };
            let resp = search_all(&params, settings, rt).await;
            serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
        }
        "metasearch_answer" => {
            let query = args["query"].as_str().ok_or("missing query")?;
            let focus = FocusMode::parse(args["focus"].as_str().unwrap_or("general"));
            let model = args["model"].as_str();
            let params = SearchParams {
                query: query.to_string(),
                ai_answer: Some(false),
                ..Default::default()
            };
            let resp = search_all(&params, settings, rt).await;
            let citations = metasearch::build_citations(&resp.results, settings.ai.answer_top_n);
            let answer = if settings.ai.enabled && !resp.results.is_empty() {
                metasearch::ai::stream_answer_collect(
                    &settings.ai,
                    &rt.client,
                    &resp.query,
                    &resp.results,
                    focus,
                    model,
                )
                .await
                .ok()
            } else {
                None
            };
            let out = serde_json::json!({
                "query": resp.query,
                "number_of_results": resp.number_of_results,
                "answer": answer,
                "citations": citations,
            });
            serde_json::to_string_pretty(&out).map_err(|e| e.to_string())
        }
        "metasearch_research" => {
            let req: ResearchRequest =
                serde_json::from_value(args.clone()).map_err(|e| format!("bad args: {e}"))?;
            if req.query.trim().is_empty() {
                return Err("empty query".into());
            }
            let resp = api::run_research(&req, settings, rt).await;
            serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
        }
        "metasearch_status" => {
            let engines_enabled: Vec<&str> = settings
                .engines
                .iter()
                .filter(|e| e.enabled)
                .map(|e| e.name.as_str())
                .collect();
            let out = serde_json::json!({
                "status": "ok",
                "version": env!("CARGO_PKG_VERSION"),
                "ai": {
                    "enabled": settings.ai.enabled,
                    "base_url": &settings.ai.base_url,
                    "model": &settings.ai.model,
                    "has_api_key": settings.ai.api_key.is_some(),
                },
                "server": {
                    "port": settings.server.port,
                    "bind_address": &settings.server.bind_address,
                },
                "engines_enabled": engines_enabled.len(),
                "engines_sample": &engines_enabled[..engines_enabled.len().min(10)],
                "feeds_enabled": settings.feeds.enabled,
                "setup_tips": [
                    "For local AI: install Ollama and run 'ollama pull llama3.2:3b'",
                    "Set METASEARCH_AI_BASE_URL=http://localhost:11434 for local Ollama",
                    "Web UI available at http://localhost:8889",
                ],
            });
            serde_json::to_string_pretty(&out).map_err(|e| e.to_string())
        }
        _ => Err(format!("unknown tool: {name}")),
    }
}
