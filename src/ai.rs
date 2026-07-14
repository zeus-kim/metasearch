//! Optional LLM-backed enhancements.
//!
//! Every function here is opt-in via [`crate::config::AiSettings`] and degrades
//! gracefully: any network/parse failure returns `None`/leaves results
//! untouched, so the engine works fully with no model running.
//!
//! PRIVACY: the only outbound destination is the user-configured local model
//! (`ai.base_url`, Ollama-compatible). No third party sees the query. We never
//! log query text here.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;

use crate::config::AiSettings;
use crate::types::{Answer, SearchResult};

/// In-process LRU cache for Ollama embedding vectors. Avoids re-embedding
/// identical query/snippet texts during rerank/cluster on every request.
type EmbedKey = (String, [u8; 32]);
type EmbedStore = (HashMap<EmbedKey, Vec<f32>>, Vec<EmbedKey>);

pub struct EmbeddingCache {
    max: usize,
    inner: Mutex<EmbedStore>,
}

impl EmbeddingCache {
    pub fn new(max: usize) -> Self {
        EmbeddingCache {
            max: max.max(1),
            inner: Mutex::new((HashMap::new(), Vec::new())),
        }
    }

    fn key(model: &str, text: &str) -> EmbedKey {
        let mut hasher = Sha256::new();
        hasher.update(text.as_bytes());
        let hash: [u8; 32] = hasher.finalize().into();
        (model.to_string(), hash)
    }

    pub fn get(&self, model: &str, text: &str) -> Option<Vec<f32>> {
        let key = Self::key(model, text);
        self.inner.lock().ok().and_then(|g| g.0.get(&key).cloned())
    }

    pub fn put(&self, model: &str, text: &str, vec: Vec<f32>) {
        if vec.is_empty() {
            return;
        }
        let key = Self::key(model, text);
        if let Ok(mut g) = self.inner.lock() {
            let (map, order) = &mut *g;
            if !map.contains_key(&key) {
                order.push(key.clone());
                while order.len() > self.max {
                    if let Some(old) = order.first().cloned() {
                        order.remove(0);
                        map.remove(&old);
                    }
                }
            }
            map.insert(key, vec);
        }
    }
}

/// A single grounded source backing an answer, mapped to an inline `[n]` marker.
///
/// `index` is the 1-based citation number used in the answer text (`[1]`, `[2]`,
/// …). The full set is built from the same top-N results that are fed to the
/// model, so a citation marker always resolves to a real, supplied source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Citation {
    pub index: usize,
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub engine: String,
}

/// A synthesized answer plus the structured citation list it was grounded on.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroundedAnswer {
    /// Prose answer with inline `[n]` markers referencing `citations`.
    pub answer: String,
    pub citations: Vec<Citation>,
}

/// An incremental event emitted while streaming an answer. Mirrors the SSE
/// event taxonomy the server exposes on `/answer`, but is transport-agnostic so
/// the CLI can consume the same stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnswerChunk {
    /// A delta of answer text (one or more tokens).
    Token { text: String },
    /// The model finished; carries the full accumulated answer text.
    Done {
        answer: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<TokenUsage>,
    },
    /// The model was unreachable / errored; plain search results still stand.
    Error { message: String },
}

/// Token usage statistics for API calls.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Number of input/prompt tokens.
    pub input_tokens: u64,
    /// Number of output/completion tokens.
    pub output_tokens: u64,
    /// Total tokens (input + output).
    pub total_tokens: u64,
    /// Estimated cost in USD (if pricing configured).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// Model name used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl TokenUsage {
    pub fn new(input: u64, output: u64) -> Self {
        Self {
            input_tokens: input,
            output_tokens: output,
            total_tokens: input + output,
            cost_usd: None,
            model: None,
        }
    }

    pub fn with_cost(mut self, input_cost_per_m: f64, output_cost_per_m: f64) -> Self {
        if input_cost_per_m > 0.0 || output_cost_per_m > 0.0 {
            let input_cost = (self.input_tokens as f64 / 1_000_000.0) * input_cost_per_m;
            let output_cost = (self.output_tokens as f64 / 1_000_000.0) * output_cost_per_m;
            self.cost_usd = Some(input_cost + output_cost);
        }
        self
    }

    pub fn with_model(mut self, model: &str) -> Self {
        self.model = Some(model.to_string());
        self
    }
}

/// Number of leading characters of a result's content kept as a citation snippet.
const CITATION_SNIPPET_CHARS: usize = 280;

/// Synthesis preset for answer prompts .
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FocusMode {
    #[default]
    General,
    Academic,
    Writing,
    Code,
}

impl FocusMode {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "academic" | "scholar" => FocusMode::Academic,
            "writing" | "write" => FocusMode::Writing,
            "code" | "dev" | "programming" => FocusMode::Code,
            _ => FocusMode::General,
        }
    }

    fn prompt_suffix(self) -> &'static str {
        match self {
            FocusMode::General => "",
            FocusMode::Academic => {
                "\n\nFocus: academic — use precise, scholarly tone; \
emphasize peer-reviewed or authoritative sources when present; note uncertainty \
where results disagree."
            }
            FocusMode::Writing => {
                "\n\nFocus: writing — help the user write better: \
clarity, structure, tone, and actionable phrasing; still cite sources inline."
            }
            FocusMode::Code => {
                "\n\nFocus: code — prioritize technical accuracy, APIs, \
syntax, and implementation details from the results; mention libraries and \
versions when the sources do."
            }
        }
    }
}

fn effective_model(ai: &AiSettings, override_model: Option<&str>) -> String {
    override_model
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| ai.model.clone())
}

/// Model used for full-page article rewrites: explicit `?model=` override wins,
/// then `ai.article_model` (a larger, multilingual-strong default), then the
/// general `ai.model`.
fn effective_article_model(ai: &AiSettings, override_model: Option<&str>) -> String {
    if let Some(m) = override_model.map(str::trim).filter(|m| !m.is_empty()) {
        return m.to_string();
    }
    let article = ai.article_model.trim();
    if !article.is_empty() {
        article.to_string()
    } else {
        ai.model.clone()
    }
}

/// Build the structured citation list (1-indexed) from the top-N results that
/// will be / were fed to the model. This is the single source of truth that
/// inline `[n]` markers map onto, so grounding stays strict: a marker can only
/// ever point at a real supplied source.
pub fn build_citations(results: &[SearchResult], top_n: usize) -> Vec<Citation> {
    let n = results.len().min(top_n.max(1));
    results
        .iter()
        .take(n)
        .enumerate()
        .map(|(i, r)| Citation {
            index: i + 1,
            title: r.title.clone(),
            url: r.url.clone(),
            snippet: r.content.chars().take(CITATION_SNIPPET_CHARS).collect(),
            engine: r.engine.clone(),
        })
        .collect()
}

/// Build the numbered context block fed to the model (shared by the one-shot
/// and streaming synthesizers so they stay in lock-step).
fn answer_context(results: &[SearchResult], top_n: usize) -> String {
    let n = results.len().min(top_n.max(1));
    let mut context = String::new();
    for (i, r) in results.iter().take(n).enumerate() {
        let snippet: String = r.content.chars().take(500).collect();
        let domain = extract_domain(&r.url);
        // When snippet is empty, add domain hint to help AI infer source type
        let content_line = if snippet.trim().is_empty() {
            format!("(Source: {})", domain)
        } else {
            snippet
        };
        context.push_str(&format!(
            "[{}] {}\n{}\n{}\n\n",
            i + 1,
            r.title,
            r.url,
            content_line
        ));
    }
    context
}

/// Build enhanced context for deep research with source metadata.
fn deep_answer_context(results: &[SearchResult], top_n: usize) -> String {
    let n = results.len().min(top_n.max(1));
    let mut context = String::new();
    for (i, r) in results.iter().take(n).enumerate() {
        let snippet: String = r.content.chars().take(600).collect();
        let domain = extract_domain(&r.url);
        let date_info = r.published_date.as_ref()
            .map(|d| format!(" ({})", d))
            .unwrap_or_default();
        context.push_str(&format!(
            "[{}] {}\nSource: {}{}\nURL: {}\n{}\n\n",
            i + 1,
            r.title,
            domain,
            date_info,
            r.url,
            snippet
        ));
    }
    context
}

/// Extract domain from URL for source identification.
fn extract_domain(url: &str) -> String {
    url.split("://")
        .nth(1)
        .and_then(|s| s.split('/').next())
        .map(|s| s.trim_start_matches("www.").to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Whether `text` contains at least one inline `[n]` citation marker.
pub fn has_inline_citations(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'[' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 && j < bytes.len() && bytes[j] == b']' {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// When the model omitted inline `[n]` markers, append a numbered footnote list
/// so the UI can still link sources.
pub fn append_citation_footnotes(answer: &str, citations: &[Citation]) -> String {
    if citations.is_empty() || has_inline_citations(answer) {
        return answer.to_string();
    }
    let mut out = answer.trim_end().to_string();
    out.push_str("\n\nSources:\n");
    for c in citations {
        out.push_str(&format!("[{}] {}\n", c.index, c.title));
    }
    out.trim_end().to_string()
}

fn query_has_hangul(query: &str) -> bool {
    query
        .chars()
        .any(|c| ('\u{AC00}'..='\u{D7A3}').contains(&c) || ('\u{1100}'..='\u{11FF}').contains(&c))
}

/// Auto-detect a search **locale** (BCP-47-ish `lang` or `lang-REGION`, e.g.
/// `ko-KR`) from the query's *script*, so the query's own language/region can
/// drive engine results without any explicit `language=` parameter or UI.
///
/// This is deliberately script-based (not a full statistical language
/// detector): scripts that map cleanly to a single dominant language/region are
/// recognised with high confidence, and everything else (notably plain Latin
/// text, which is ambiguous across English/German/Spanish/…) returns `None` so
/// the caller falls back to the configured default. Hangul → `ko-KR`, Japanese
/// kana → `ja-JP`, etc. Japanese is checked before Han because Japanese mixes
/// kana with Han ideographs.
pub fn detect_locale(query: &str) -> Option<&'static str> {
    let (mut kana, mut han, mut cyrillic) = (false, false, false);
    let (mut greek, mut arabic, mut hebrew) = (false, false, false);
    let (mut thai, mut devanagari) = (false, false);
    for c in query.chars() {
        match c {
            // Hangul (Korean) — highest confidence, return immediately.
            '\u{AC00}'..='\u{D7A3}' | '\u{1100}'..='\u{11FF}' => return Some("ko-KR"),
            // Hiragana / Katakana (Japanese).
            '\u{3040}'..='\u{309F}' | '\u{30A0}'..='\u{30FF}' => kana = true,
            // CJK unified ideographs (Han) — Chinese unless kana also present.
            '\u{4E00}'..='\u{9FFF}' | '\u{3400}'..='\u{4DBF}' => han = true,
            '\u{0400}'..='\u{04FF}' => cyrillic = true,
            '\u{0370}'..='\u{03FF}' => greek = true,
            '\u{0600}'..='\u{06FF}' | '\u{0750}'..='\u{077F}' => arabic = true,
            '\u{0590}'..='\u{05FF}' => hebrew = true,
            '\u{0E00}'..='\u{0E7F}' => thai = true,
            '\u{0900}'..='\u{097F}' => devanagari = true,
            _ => {}
        }
    }
    if kana {
        Some("ja-JP")
    } else if han {
        Some("zh-CN")
    } else if cyrillic {
        Some("ru-RU")
    } else if greek {
        Some("el-GR")
    } else if arabic {
        Some("ar-EG")
    } else if hebrew {
        Some("he-IL")
    } else if thai {
        Some("th-TH")
    } else if devanagari {
        Some("hi-IN")
    } else {
        None
    }
}

/// Human-readable name of the query's language when we can detect it with high
/// confidence, used to give the model an explicit, firm language constraint.
/// Returns `None` when detection is inconclusive so the prompt falls back to a
/// generic "same language as the query" instruction.
fn query_language(query: &str) -> Option<&'static str> {
    if query_has_hangul(query) {
        Some("Korean")
    } else {
        None
    }
}

/// Build the firm language-consistency hint for [`answer_prompt`]. Detected
/// languages get an explicit name ("Write the ENTIRE answer in Korean ..."),
/// otherwise we instruct the model to mirror the query's language/script.
fn language_hint(query: &str) -> String {
    match query_language(query) {
        Some(name) => format!(
            "\nLANGUAGE: Write the ENTIRE answer in {name}. Do NOT use any other \
language or script — do not mix in English, Vietnamese, Chinese, or any other \
words. Every sentence, term, and word must be written in {name}."
        ),
        None => String::from(
            "\nLANGUAGE: Write the ENTIRE answer in the SAME language and script as \
the query. Do NOT mix in any other language — answer an English query in \
English, a Korean query in Korean, a Japanese query in Japanese, and so on. \
Use no other language or script anywhere in the answer.",
        ),
    }
}

/// The grounded-summary prompt shared by `synthesize_answer` and `stream_answer`.
fn answer_prompt(query: &str, context: &str, focus: FocusMode) -> String {
    let language_hint = language_hint(query);
    format!(
        "You are a search-result summarizer. Read the numbered web results below \
and write a concise, neutral prose summary (2-5 sentences) of what they \
collectively say about the query. You MUST cite the sources you use inline with \
bracketed numbers that refer to the numbered results, like [1] or [2][3]; cite \
at least one source, and place each marker right after the claim it supports. \
RELEVANCE: cite ONLY sources that are directly about the query's subject. Some \
results may be unrelated to the query — ignore those completely and do NOT cite \
them or pull facts from them, even though they appear in the numbered list. \
{language_hint} Use ONLY information \
present in the results — never invent facts, URLs, or citation numbers that are \
not in the list. The query may be a topic \
or keyword rather than a question; in that case, summarize the most relevant, \
salient information about it. Begin directly with the summary (no preamble like \
\"Here is a summary\"). \
IMPORTANT: Even if snippets are brief or missing, USE the titles and URLs to \
infer what each source is about and provide a helpful summary. The title often \
contains the key information. Only reply NO_ANSWER if ALL results are completely \
unrelated to the query AND you cannot infer anything useful from any title.{focus}\n\n\
Query: {query}\n\nSearch results:\n{context}\nSummary:",
        focus = focus.prompt_suffix(),
    )
}

/// Enhanced prompt for deep research mode with multi-source analysis.
fn deep_answer_prompt(query: &str, context: &str, focus: FocusMode) -> String {
    let language_hint = language_hint(query);
    format!(
        "You are a research analyst synthesizing information from multiple sources. \
Read the numbered web results below and provide a comprehensive analysis.\n\n\
INSTRUCTIONS:\n\
1. SYNTHESIZE information across all relevant sources (not just summarize each)\n\
2. CITE sources inline with [n] markers after each claim\n\
3. NOTE any CONTRADICTIONS between sources explicitly (e.g., \"Source [1] claims X, \
while [3] states Y\")\n\
4. PRIORITIZE recent information and authoritative sources\n\
5. If sources disagree, present both viewpoints with citations\n\
{language_hint}\n\n\
FORMAT your response as:\n\
- Start with the main finding/answer (2-3 sentences)\n\
- Add supporting details with citations\n\
- End with any caveats or contradictions found\n\n\
Use ONLY information from the results. Never invent facts. \
IMPORTANT: Even with minimal snippets, USE titles and URLs to infer source content. \
Reply NO_ANSWER only if ALL results are completely unrelated AND titles provide no \
useful information.{focus}\n\n\
Query: {query}\n\nSearch results:\n{context}\nAnalysis:",
        focus = focus.prompt_suffix(),
    )
}

/// Synthesize a concise, cited prose summary of the top results (RAG-lite).
///
/// This is framed as a *summary of the search results* rather than a strict
/// question-answer, so it works for ordinary topic/keyword queries (e.g.
/// `선거`, `tokyo weather`) and not only for well-posed questions — the previous
/// "answer the question, else NO_ANSWER" framing made the model abstain on bare
/// keywords, which suppressed the card on perfectly good result sets. It stays
/// grounded (use only the supplied results, cite inline, no fabrication) and
/// only abstains (`None`) when there are no results at all or the model judges
/// the results entirely unrelated.
///
/// Returns an [`Answer`] tagged with the `answer/ai.html` template, or `None`.
pub async fn synthesize_answer(
    ai: &AiSettings,
    client: &reqwest::Client,
    query: &str,
    results: &[SearchResult],
) -> Option<Answer> {
    synthesize_answer_with_options(ai, client, query, results, FocusMode::General, None).await
}

/// Like [`synthesize_answer`] with focus preset and optional model override.
pub async fn synthesize_answer_with_options(
    ai: &AiSettings,
    client: &reqwest::Client,
    query: &str,
    results: &[SearchResult],
    focus: FocusMode,
    model_override: Option<&str>,
) -> Option<Answer> {
    if results.is_empty() {
        return None;
    }
    let context = answer_context(results, ai.answer_top_n);
    let prompt = answer_prompt(query, &context, focus);

    let text = generate_with_model(ai, client, &prompt, model_override).await?;
    let trimmed = text.trim();
    if is_no_answer(trimmed) {
        return None;
    }
    Some(Answer {
        answer: trimmed.to_string(),
        url: None,
        engine: "ai".to_string(),
        template: "answer/ai.html".to_string(),
    })
}

/// Whether a model reply is the explicit "no answer" sentinel (exact / leading),
/// not a mere substring — a real summary may legitimately mention the token.
pub fn is_no_answer(text: &str) -> bool {
    let trimmed = text.trim();
    let upper = trimmed.to_ascii_uppercase();
    trimmed.is_empty() || upper == "NO_ANSWER" || upper.starts_with("NO_ANSWER")
}

/// Stream a grounded, cited answer token-by-token from the local model.
///
/// This is the streaming twin of [`synthesize_answer`]: identical prompt and
/// grounding rules, but it proxies Ollama's `/api/generate` with `stream:true`
/// and invokes `on_token` for every text delta as it arrives. The accumulated
/// answer text is returned on success.
///
/// Graceful degradation: if there are no results, or the model is unreachable /
/// Stream a weather explanation based on API data (not search results).
pub async fn stream_weather_answer<F>(
    ai: &AiSettings,
    client: &reqwest::Client,
    _query: &str,
    weather_data: &str,
    mut on_token: F,
) -> Result<String, String>
where
    F: FnMut(&str),
{
    let prompt = format!(
        "날씨 안내원으로서 아래 데이터를 자연스러운 한국어로 설명해. 코드나 마크다운 없이 순수 텍스트로만.

현재 기온, 체감온도, 습도, 바람을 포함하고, 오늘/내일 예보와 외출 팁을 3-4문장으로 간결하게.

{weather_data}"
    );

    let model = effective_model(ai, None);
    let timeout = std::time::Duration::from_secs(ai.timeout_secs.max(1));
    let mut answer = String::new();

    if let Some(ref api_key) = ai.api_key {
        let url = format!("{}/chat/completions", ai.base_url.trim_end_matches('/'));
        let body = json!({
            "model": model,
            "messages": [{"role": "user", "content": prompt}],
            "stream": true
        });
        let resp = client
            .post(&url)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| format!("model unreachable: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("model returned status {}", resp.status()));
        }

        let mut resp = resp;
        let mut buf: Vec<u8> = Vec::new();
        loop {
            let chunk = match resp.chunk().await {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => return Err(format!("stream read error: {e}")),
            };
            buf.extend_from_slice(&chunk);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line).trim().to_string();
                if line.is_empty() || line == "data: [DONE]" {
                    continue;
                }
                if let Some(json_str) = line.strip_prefix("data: ") {
                    if let Ok(v) = serde_json::from_str::<Value>(json_str) {
                        if let Some(tok) = v["choices"][0]["delta"]["content"].as_str() {
                            if !tok.is_empty() {
                                answer.push_str(tok);
                                on_token(tok);
                            }
                        }
                    }
                }
            }
        }
    } else {
        // Fallback to Ollama
        let url = format!("{}/api/generate", ai.base_url.trim_end_matches('/'));
        let body = json!({ "model": model, "prompt": prompt, "stream": true });
        let resp = client
            .post(&url)
            .json(&body)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| format!("model unreachable: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("model returned status {}", resp.status()));
        }

        let mut resp = resp;
        let mut buf: Vec<u8> = Vec::new();
        loop {
            let chunk = match resp.chunk().await {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => return Err(format!("stream read error: {e}")),
            };
            buf.extend_from_slice(&chunk);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let line = &line[..line.len().saturating_sub(1)];
                if line.is_empty() {
                    continue;
                }
                if let Ok(v) = serde_json::from_slice::<Value>(line) {
                    if let Some(tok) = v["response"].as_str() {
                        if !tok.is_empty() {
                            answer.push_str(tok);
                            on_token(tok);
                        }
                    }
                    if v["done"].as_bool() == Some(true) {
                        break;
                    }
                }
            }
        }
    }

    Ok(answer.trim().to_string())
}

/// errors / never produces text, an `Err(message)` is returned and **no**
/// partial answer is emitted — callers surface a clear terminal state and fall
/// back to plain search. We never log the query here (privacy).
///
/// Transport note: the NDJSON stream is read incrementally via
/// [`reqwest::Response::chunk`], so this needs no extra reqwest feature/crate.
pub async fn stream_answer<F>(
    ai: &AiSettings,
    client: &reqwest::Client,
    query: &str,
    results: &[SearchResult],
    on_token: F,
) -> Result<StreamArticleResult, String>
where
    F: FnMut(&str),
{
    stream_answer_with_options(
        ai,
        client,
        query,
        results,
        FocusMode::General,
        None,
        on_token,
    )
    .await
}

/// Streaming twin of [`synthesize_answer_with_options`].
pub async fn stream_answer_with_options<F>(
    ai: &AiSettings,
    client: &reqwest::Client,
    query: &str,
    results: &[SearchResult],
    focus: FocusMode,
    model_override: Option<&str>,
    mut on_token: F,
) -> Result<StreamArticleResult, String>
where
    F: FnMut(&str),
{
    if results.is_empty() {
        return Err("no results to summarize".to_string());
    }
    let context = answer_context(results, ai.answer_top_n);
    let prompt = answer_prompt(query, &context, focus);
    let model = effective_model(ai, model_override);
    let timeout = std::time::Duration::from_secs(ai.timeout_secs.max(1));

    let mut answer = String::new();
    let mut usage: Option<TokenUsage> = None;

    // Use OpenAI-compatible API if api_key is set (Groq, Together, OpenAI, etc.)
    if let Some(ref api_key) = ai.api_key {
        let url = format!("{}/chat/completions", ai.base_url.trim_end_matches('/'));
        let body = json!({
            "model": model,
            "messages": [{"role": "user", "content": prompt}],
            "stream": true,
            "stream_options": {"include_usage": true}
        });
        let resp = client
            .post(&url)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| format!("model unreachable: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("model returned status {}", resp.status()));
        }

        let mut resp = resp;
        let mut buf: Vec<u8> = Vec::new();
        // OpenAI streams SSE: "data: {...}\n\n" or "data: [DONE]\n\n"
        loop {
            let chunk = match resp.chunk().await {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => return Err(format!("stream read error: {e}")),
            };
            buf.extend_from_slice(&chunk);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line).trim().to_string();
                if line.is_empty() || line == "data: [DONE]" {
                    continue;
                }
                if let Some(json_str) = line.strip_prefix("data: ") {
                    if let Ok(v) = serde_json::from_str::<Value>(json_str) {
                        if let Some(tok) = v["choices"][0]["delta"]["content"].as_str() {
                            if !tok.is_empty() {
                                answer.push_str(tok);
                                on_token(tok);
                            }
                        }
                        // Parse usage from final chunk (OpenAI sends with stream_options)
                        if let Some(u) = v.get("usage") {
                            let input = u["prompt_tokens"].as_u64().unwrap_or(0);
                            let output = u["completion_tokens"].as_u64().unwrap_or(0);
                            if input > 0 || output > 0 {
                                let mut token_usage = TokenUsage::new(input, output)
                                    .with_model(&model);
                                if ai.input_cost_per_million > 0.0 || ai.output_cost_per_million > 0.0 {
                                    token_usage = token_usage.with_cost(
                                        ai.input_cost_per_million,
                                        ai.output_cost_per_million,
                                    );
                                }
                                usage = Some(token_usage);
                            }
                        }
                    }
                }
            }
        }
    } else {
        // Fallback to Ollama API
        let url = format!("{}/api/generate", ai.base_url.trim_end_matches('/'));
        let body = json!({ "model": model, "prompt": prompt, "stream": true });
        let resp = client
            .post(&url)
            .json(&body)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| format!("model unreachable: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("model returned status {}", resp.status()));
        }

        let mut resp = resp;
        let mut buf: Vec<u8> = Vec::new();
        // Ollama streams newline-delimited JSON objects
        loop {
            let chunk = match resp.chunk().await {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => return Err(format!("stream read error: {e}")),
            };
            buf.extend_from_slice(&chunk);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let line = &line[..line.len().saturating_sub(1)];
                if line.is_empty() {
                    continue;
                }
                if let Ok(v) = serde_json::from_slice::<Value>(line) {
                    if let Some(tok) = v["response"].as_str() {
                        if !tok.is_empty() {
                            answer.push_str(tok);
                            on_token(tok);
                        }
                    }
                    // Ollama provides usage in the final "done" response
                    if v["done"].as_bool() == Some(true) {
                        if let (Some(input), Some(output)) = (
                            v["prompt_eval_count"].as_u64(),
                            v["eval_count"].as_u64(),
                        ) {
                            usage = Some(TokenUsage::new(input, output).with_model(&model));
                        }
                        buf.clear();
                        break;
                    }
                }
            }
        }
        // Flush any trailing object not terminated by a newline.
        if !buf.is_empty() {
            if let Ok(v) = serde_json::from_slice::<Value>(&buf) {
                if let Some(tok) = v["response"].as_str() {
                    if !tok.is_empty() {
                        answer.push_str(tok);
                        on_token(tok);
                    }
                }
            }
        }
    }

    if is_no_answer(&answer) {
        return Err("model declined (no grounded answer)".to_string());
    }
    Ok(StreamArticleResult {
        article: answer.trim().to_string(),
        usage,
    })
}

/// Non-streaming collect helper for the research API (discards incremental tokens).
pub async fn stream_answer_collect(
    ai: &AiSettings,
    client: &reqwest::Client,
    query: &str,
    results: &[SearchResult],
    focus: FocusMode,
    model_override: Option<&str>,
) -> Result<String, String> {
    stream_answer_with_options(ai, client, query, results, focus, model_override, |_| {})
        .await
        .map(|r| r.article)
}

/// Stream a deep research answer with enhanced multi-source analysis.
///
/// This variant uses:
/// - Enhanced context with source domains and dates
/// - Deep analysis prompt that requests contradiction detection
/// - Longer snippet allowance for more comprehensive analysis
pub async fn stream_answer_deep<F>(
    ai: &AiSettings,
    client: &reqwest::Client,
    query: &str,
    results: &[SearchResult],
    focus: FocusMode,
    model_override: Option<&str>,
    mut on_token: F,
) -> Result<StreamArticleResult, String>
where
    F: FnMut(&str),
{
    if results.is_empty() {
        return Err("no results to summarize".to_string());
    }
    // Use enhanced context with more metadata
    let context = deep_answer_context(results, ai.answer_top_n.max(10));
    let prompt = deep_answer_prompt(query, &context, focus);
    let model = effective_model(ai, model_override);
    let timeout = std::time::Duration::from_secs(ai.timeout_secs.max(1) * 2); // Allow more time for deep analysis

    let mut answer = String::new();
    let mut usage: Option<TokenUsage> = None;

    // Use OpenAI-compatible API if api_key is set
    if let Some(ref api_key) = ai.api_key {
        let url = format!("{}/chat/completions", ai.base_url.trim_end_matches('/'));
        let body = json!({
            "model": model,
            "messages": [{"role": "user", "content": prompt}],
            "stream": true,
            "stream_options": {"include_usage": true}
        });
        let resp = client
            .post(&url)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| format!("model unreachable: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("model returned status {}", resp.status()));
        }

        let mut resp = resp;
        let mut buf: Vec<u8> = Vec::new();
        loop {
            let chunk = match resp.chunk().await {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => return Err(format!("stream read error: {e}")),
            };
            buf.extend_from_slice(&chunk);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let line = &line[..line.len().saturating_sub(1)];
                if line.is_empty() || line.starts_with(b":") {
                    continue;
                }
                let line = if line.starts_with(b"data: ") { &line[6..] } else { line };
                if line == b"[DONE]" {
                    break;
                }
                if let Ok(v) = serde_json::from_slice::<Value>(line) {
                    if let Some(tok) = v["choices"][0]["delta"]["content"].as_str() {
                        if !tok.is_empty() {
                            answer.push_str(tok);
                            on_token(tok);
                        }
                    }
                    if let Some(u) = v.get("usage") {
                        let input = u["prompt_tokens"].as_u64().unwrap_or(0);
                        let output = u["completion_tokens"].as_u64().unwrap_or(0);
                        if input > 0 || output > 0 {
                            usage = Some(TokenUsage::new(input, output).with_model(&model));
                        }
                    }
                }
            }
        }
    } else {
        // Ollama streaming
        let url = format!("{}/api/generate", ai.base_url.trim_end_matches('/'));
        let body = json!({
            "model": model,
            "prompt": prompt,
            "stream": true
        });
        let resp = client
            .post(&url)
            .json(&body)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| format!("model unreachable: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("model returned status {}", resp.status()));
        }
        let mut resp = resp;
        let mut buf: Vec<u8> = Vec::new();
        loop {
            let chunk = match resp.chunk().await {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => return Err(format!("stream read error: {e}")),
            };
            buf.extend_from_slice(&chunk);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let line = &line[..line.len().saturating_sub(1)];
                if line.is_empty() {
                    continue;
                }
                if let Ok(v) = serde_json::from_slice::<Value>(line) {
                    if let Some(tok) = v["response"].as_str() {
                        if !tok.is_empty() {
                            answer.push_str(tok);
                            on_token(tok);
                        }
                    }
                    if v["done"].as_bool() == Some(true) {
                        break;
                    }
                }
            }
        }
    }

    if is_no_answer(&answer) {
        return Err("model declined (no grounded answer)".to_string());
    }
    Ok(StreamArticleResult {
        article: answer.trim().to_string(),
        usage,
    })
}

/// Generate 3-5 suggested follow-up questions for a result set, so a UI can
/// offer "related questions". Strictly grounded in the query +
/// supplied result titles/snippets; returns an empty list on any failure.
pub async fn suggest_followups(
    ai: &AiSettings,
    client: &reqwest::Client,
    query: &str,
    results: &[SearchResult],
) -> Vec<String> {
    if results.is_empty() {
        return Vec::new();
    }
    let context = answer_context(results, ai.answer_top_n.min(results.len()).max(1));
    let prompt = format!(
        "Based on the user's query and the web results below, propose 3 to 5 \
natural follow-up questions the user might ask next. Write each question in the \
SAME language as the query. Make them specific and grounded in the results — do \
not invent facts. Output ONLY a JSON array of question strings, no prose.\n\n\
Query: {query}\n\nSearch results:\n{context}\nFollow-up questions:"
    );
    let Some(text) = generate(ai, client, &prompt).await else {
        return Vec::new();
    };
    parse_string_array(&text)
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case(query))
        .take(5)
        .collect()
}

/// Article tone/bias analysis result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArticleTone {
    /// 0-100: how fact-based vs opinion-based (100 = pure facts)
    pub factual_score: u8,
    /// 0-100: emotional intensity (0 = neutral, 100 = highly emotional)
    pub emotional_score: u8,
    /// -100 to 100: political lean (-100 = far left, 100 = far right, 0 = neutral)
    pub bias_score: i8,
    /// Brief description of the tone
    pub tone_label: String,
}

/// Perspective lens for article rewriting
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArticlePerspective {
    /// Default balanced analysis
    Analyst,
    /// Conservative/right-leaning interpretation
    Conservative,
    /// Progressive/left-leaning interpretation
    Progressive,
    /// Academic/scholarly analysis
    Scholar,
    /// Philosophical interpretation
    Philosopher,
    /// Economic/market perspective
    Economist,
    /// Critical/skeptical analysis
    Critical,
    /// Simplified for general audience
    Casual,
    /// Technical deep-dive
    Technical,
    /// Humorous/satirical take
    Satirical,
}

impl ArticlePerspective {
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "conservative" | "right" | "보수" | "우파" => Self::Conservative,
            "progressive" | "left" | "진보" | "좌파" => Self::Progressive,
            "scholar" | "academic" | "학자" => Self::Scholar,
            "philosopher" | "philosophy" | "철학" | "철학자" => Self::Philosopher,
            "economist" | "economic" | "경제" | "경제학자" => Self::Economist,
            "critical" | "skeptic" | "비판" | "비판적" => Self::Critical,
            "casual" | "simple" | "일반" | "쉽게" => Self::Casual,
            "technical" | "tech" | "기술" | "전문가" => Self::Technical,
            "satirical" | "humor" | "풍자" | "유머" => Self::Satirical,
            _ => Self::Analyst,
        }
    }

    pub fn prompt_instruction(&self, lang: &str) -> &'static str {
        match (self, lang) {
            (Self::Conservative, "ko") => "보수적/우파 관점에서 해석하세요. 전통적 가치, 시장 자유, 개인 책임을 강조하세요.",
            (Self::Conservative, _) => "Interpret from a conservative perspective. Emphasize traditional values, free markets, and individual responsibility.",
            (Self::Progressive, "ko") => "진보적/좌파 관점에서 해석하세요. 사회 정의, 평등, 제도적 문제를 강조하세요.",
            (Self::Progressive, _) => "Interpret from a progressive perspective. Emphasize social justice, equality, and systemic issues.",
            (Self::Scholar, "ko") => "학자의 관점에서 분석하세요. 학술적 맥락, 연구 결과, 이론적 프레임워크를 인용하세요.",
            (Self::Scholar, _) => "Analyze as an academic scholar. Reference scholarly context, research findings, and theoretical frameworks.",
            (Self::Philosopher, "ko") => "철학자의 관점에서 해석하세요. 윤리적 함의, 존재론적 질문, 인간 조건과의 연관성을 탐구하세요.",
            (Self::Philosopher, _) => "Interpret as a philosopher. Explore ethical implications, existential questions, and connections to the human condition.",
            (Self::Economist, "ko") => "경제학자 관점에서 분석하세요. 시장 영향, 인센티브 구조, 비용-편익을 강조하세요.",
            (Self::Economist, _) => "Analyze as an economist. Focus on market impacts, incentive structures, and cost-benefit analysis.",
            (Self::Critical, "ko") => "비판적 관점에서 분석하세요. 숨겨진 의도, 생략된 정보, 잠재적 문제점을 지적하세요.",
            (Self::Critical, _) => "Analyze critically. Point out hidden agendas, omitted information, and potential problems.",
            (Self::Casual, "ko") => "일반인이 쉽게 이해할 수 있도록 설명하세요. 전문 용어를 피하고 일상적 비유를 사용하세요.",
            (Self::Casual, _) => "Explain for a general audience. Avoid jargon and use everyday analogies.",
            (Self::Technical, "ko") => "기술 전문가 관점에서 깊이 분석하세요. 세부 사항과 기술적 정확성을 강조하세요.",
            (Self::Technical, _) => "Analyze as a technical expert. Emphasize details and technical accuracy.",
            (Self::Satirical, "ko") => "풍자적으로 재해석하세요. 아이러니와 유머를 사용하되 핵심 사실은 유지하세요.",
            (Self::Satirical, _) => "Reinterpret satirically. Use irony and humor while maintaining core facts.",
            (Self::Analyst, "ko") => "균형 잡힌 분석가 관점에서 객관적으로 분석하세요.",
            (Self::Analyst, _) => "Analyze objectively as a balanced analyst.",
        }
    }

    pub fn label(&self, lang: &str) -> &'static str {
        match (self, lang) {
            (Self::Analyst, "ko") => "🔬 분석가",
            (Self::Conservative, "ko") => "🏛️ 보수 관점",
            (Self::Progressive, "ko") => "🌱 진보 관점",
            (Self::Scholar, "ko") => "📚 학자 관점",
            (Self::Philosopher, "ko") => "🎭 철학자",
            (Self::Economist, "ko") => "💼 경제학자",
            (Self::Critical, "ko") => "🎯 비판적",
            (Self::Casual, "ko") => "👤 쉽게 설명",
            (Self::Technical, "ko") => "🔧 전문가",
            (Self::Satirical, "ko") => "😏 풍자",
            (Self::Analyst, _) => "🔬 Analyst",
            (Self::Conservative, _) => "🏛️ Conservative",
            (Self::Progressive, _) => "🌱 Progressive",
            (Self::Scholar, _) => "📚 Scholar",
            (Self::Philosopher, _) => "🎭 Philosopher",
            (Self::Economist, _) => "💼 Economist",
            (Self::Critical, _) => "🎯 Critical",
            (Self::Casual, _) => "👤 Casual",
            (Self::Technical, _) => "🔧 Technical",
            (Self::Satirical, _) => "😏 Satirical",
        }
    }

    pub fn all() -> &'static [ArticlePerspective] {
        &[
            Self::Analyst, Self::Conservative, Self::Progressive,
            Self::Scholar, Self::Philosopher, Self::Economist,
            Self::Critical, Self::Casual, Self::Technical, Self::Satirical,
        ]
    }
}

/// Analyze article tone and bias
pub async fn analyze_article_tone(
    ai: &AiSettings,
    client: &reqwest::Client,
    title: &str,
    content: &str,
) -> Option<ArticleTone> {
    if title.trim().is_empty() {
        return None;
    }
    let snippet: String = content.chars().take(500).collect();

    let prompt = format!(
        r#"Analyze this news article's tone and bias. Respond ONLY with JSON, no other text.

Title: {title}
Content: {snippet}

JSON format:
{{"factual": 0-100, "emotional": 0-100, "bias": -100 to 100, "label": "brief description"}}

- factual: 100=pure facts, 0=pure opinion
- emotional: 0=neutral, 100=highly emotional
- bias: -100=far left, 0=neutral, 100=far right
- label: one of "factual", "emotional", "left-leaning", "right-leaning", "neutral", "opinion"

Respond with JSON only:"#
    );

    let budget = std::time::Duration::from_secs(ai.timeout_secs.clamp(2, 10));
    let text = match tokio::time::timeout(budget, generate(ai, client, &prompt)).await {
        Ok(Some(t)) => t,
        _ => return None,
    };

    // Parse JSON response
    let text = text.trim();
    let json_start = text.find('{')?;
    let json_end = text.rfind('}')?;
    let json_str = &text[json_start..=json_end];

    let v: serde_json::Value = serde_json::from_str(json_str).ok()?;
    Some(ArticleTone {
        factual_score: v["factual"].as_u64().unwrap_or(50) as u8,
        emotional_score: v["emotional"].as_u64().unwrap_or(30) as u8,
        bias_score: v["bias"].as_i64().unwrap_or(0) as i8,
        tone_label: v["label"].as_str().unwrap_or("neutral").to_string(),
    })
}

/// Classify a news article into one of the predefined categories using AI model.
/// Returns the best matching category, or None if classification fails.
/// Categories: news, politics, business, finance, tech, world, sports,
/// entertainment, health, science, culture, opinion, lifestyle, auto
pub async fn classify_article_category(
    ai: &AiSettings,
    client: &reqwest::Client,
    title: &str,
    content: &str,
    language: Option<&str>,
) -> Option<String> {
    if title.trim().is_empty() {
        return None;
    }
    let lang_hint = language.unwrap_or("en");
    let content_snippet = if content.len() > 300 {
        &content[..300]
    } else {
        content
    };

    let prompt = format!(
        r#"Classify this news article into ONE category.

Categories: politics, business, finance, tech, world, sports, entertainment, health, science, culture, opinion, lifestyle, society, news

Title: {title}
Content: {content_snippet}
Language: {lang_hint}

Respond with ONLY the category name, nothing else. If uncertain, respond "news"."#
    );

    let budget = std::time::Duration::from_secs(ai.timeout_secs.clamp(2, 10));
    let text = match tokio::time::timeout(budget, generate(ai, client, &prompt)).await {
        Ok(Some(t)) => t,
        _ => return None,
    };

    let category = text.trim().to_lowercase();
    let valid_cats = [
        "politics", "business", "finance", "tech", "world", "sports",
        "entertainment", "health", "science", "culture", "opinion",
        "lifestyle", "society", "news"
    ];

    if valid_cats.contains(&category.as_str()) {
        Some(category)
    } else {
        None
    }
}

/// Batch classify multiple articles. More efficient than calling
/// classify_article_category repeatedly for large sets.
pub async fn classify_articles_batch(
    ai: &AiSettings,
    client: &reqwest::Client,
    articles: &[(String, String)], // (title, content)
    language: Option<&str>,
) -> Vec<Option<String>> {
    if articles.is_empty() {
        return Vec::new();
    }

    // Process in parallel with concurrency limit
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(5));
    let futures: Vec<_> = articles.iter().map(|(title, content)| {
        let sem = semaphore.clone();
        let ai = ai.clone();
        let client = client.clone();
        let title = title.clone();
        let content = content.clone();
        let lang = language.map(|s| s.to_string());

        async move {
            let _permit = sem.acquire().await.ok();
            classify_article_category(&ai, &client, &title, &content, lang.as_deref()).await
        }
    }).collect();

    futures_util::future::join_all(futures).await
}

/// Plan 2–4 focused sub-queries for multi-hop deep research. Unlike
/// [`expand_query`] (spelling / phrasing variants), these target distinct angles
/// on the topic. Returns an empty list on timeout or when the model is
/// unavailable — callers fall back to a single-query search.
pub async fn plan_subqueries(
    ai: &AiSettings,
    client: &reqwest::Client,
    query: &str,
) -> Vec<String> {
    if query.trim().is_empty() {
        return Vec::new();
    }
    let prompt = format!(
        "Break this research question into 2 to 4 focused web search sub-queries \
that together cover different angles (definitions, comparisons, recent news, \
specific aspects). Each sub-query must be a standalone search string. Output \
ONLY a JSON array of strings, no prose.\nQuery: {query}"
    );
    let budget = std::time::Duration::from_secs(ai.timeout_secs.clamp(2, 8));
    let text = match tokio::time::timeout(budget, generate(ai, client, &prompt)).await {
        Ok(Some(t)) => t,
        _ => return Vec::new(),
    };
    parse_string_array(&text)
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case(query))
        .take(4)
        .collect()
}

/// Ask the model for alternative phrasings / spelling corrections of the query.
/// Returns a de-duplicated suggestion list (empty on failure).
pub async fn expand_query(ai: &AiSettings, client: &reqwest::Client, query: &str) -> Vec<String> {
    let prompt = format!(
        "Suggest up to 4 alternative search queries (spelling fixes or related \
phrasings) for the query below. Output ONLY a JSON array of strings, no prose.\n\
Query: {query}"
    );
    let Some(text) = generate(ai, client, &prompt).await else {
        return Vec::new();
    };
    parse_string_array(&text)
        .into_iter()
        .filter(|s| !s.trim().is_empty() && !s.eq_ignore_ascii_case(query))
        .take(4)
        .collect()
}

/// Rewrite a conversational follow-up into a standalone search query, using the
/// previous query as context (multi-turn refinement).
///
/// E.g. previous `"rust async runtimes"` + follow-up `"which is fastest?"` →
/// `"fastest rust async runtime"`. Returns `None` (caller falls back to the raw
/// follow-up) when the model is unavailable or declines.
pub async fn refine_query(
    ai: &AiSettings,
    client: &reqwest::Client,
    previous: &str,
    followup: &str,
) -> Option<String> {
    if previous.trim().is_empty() || followup.trim().is_empty() {
        return None;
    }
    let prompt = format!(
        "Rewrite the user's FOLLOW-UP into a single standalone web search query, \
using the PREVIOUS query for context. Output ONLY the rewritten query on one \
line, no quotes or prose. If the follow-up is already standalone, echo it.\n\n\
PREVIOUS: {previous}\nFOLLOW-UP: {followup}\nQUERY:"
    );
    let text = generate(ai, client, &prompt).await?;
    let line = text.trim().lines().next().unwrap_or("").trim();
    let line = line.trim_matches('"').trim();
    if line.is_empty() || line.len() > 256 {
        return None;
    }
    Some(line.to_string())
}

/// Caption / analyse an image with a vision model (Ollama-compatible, e.g.
/// `llava`). Fetches the image, base64-encodes it, and asks the model for a
/// one-line description. Returns `None` on any failure (offline-safe).
pub async fn caption_image(
    ai: &AiSettings,
    client: &reqwest::Client,
    image_url: &str,
) -> Option<String> {
    if image_url.is_empty() {
        return None;
    }
    if !crate::url_safety::is_safe_public_url(image_url) {
        return None;
    }
    let fetch = crate::url_safety::safe_fetch_client();
    let resp = fetch
        .get(image_url)
        .timeout(std::time::Duration::from_secs(ai.timeout_secs.max(1)))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let bytes = resp.bytes().await.ok()?;
    if bytes.is_empty() || bytes.len() > 4 * 1024 * 1024 {
        return None;
    }
    let b64 = base64_encode(&bytes);
    let url = format!("{}/api/generate", ai.base_url.trim_end_matches('/'));
    let body = json!({
        "model": ai.vision_model,
        "prompt": "Describe this image in one concise sentence.",
        "images": [b64],
        "stream": false,
    });
    let resp = client
        .post(&url)
        .json(&body)
        .timeout(std::time::Duration::from_secs(ai.timeout_secs.max(1)))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: Value = resp.json().await.ok()?;
    let caption = v["response"].as_str()?.trim().to_string();
    if caption.is_empty() {
        None
    } else {
        Some(caption)
    }
}

/// Standard base64 encoder (no external crate).
pub fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((n >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(n & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Re-rank `results` in place by a hybrid of embedding similarity to the query
/// and the original positional positional score.
///
/// Pure semantic order can bury an authoritative top hit behind a chatty
/// off-topic page, so we blend: `combined = α·semantic + (1-α)·positional`,
/// where `positional` is the result's normalized positional score. This keeps the
/// positional signal as a backbone while letting semantics reorder ties — and
/// it degrades to the untouched positional order if embeddings are unavailable.
pub async fn rerank(
    ai: &AiSettings,
    client: &reqwest::Client,
    query: &str,
    results: &mut [SearchResult],
    cache: Option<&EmbeddingCache>,
) {
    const MAX: usize = 30;
    const ALPHA: f64 = 0.65; // weight of semantic similarity vs positional score
    let Some(q_emb) = embed(ai, client, query, cache).await else {
        return;
    };
    let n = results.len().min(MAX);
    let max_score = results
        .iter()
        .take(n)
        .map(|r| r.score)
        .fold(0.0f64, f64::max)
        .max(f64::MIN_POSITIVE);

    let mut scored: Vec<(usize, f64)> = Vec::with_capacity(n);
    for (i, r) in results.iter().take(n).enumerate() {
        let text = format!("{} {}", r.title, r.content);
        let sim = match embed(ai, client, &text, cache).await {
            Some(e) => cosine(&q_emb, &e),
            None => return, // bail out; keep positional order
        };
        let positional = r.score / max_score;
        let combined = ALPHA * sim + (1.0 - ALPHA) * positional;
        scored.push((i, combined));
    }
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let head: Vec<SearchResult> = scored.iter().map(|(i, _)| results[*i].clone()).collect();
    for (slot, r) in results.iter_mut().take(n).zip(head) {
        *slot = r;
    }
}

/// Greedy embedding-based clustering: assigns a `cluster` id to each result and
/// groups same-cluster results together (preserving intra-cluster order).
pub async fn cluster(
    ai: &AiSettings,
    client: &reqwest::Client,
    results: &mut [SearchResult],
    cache: Option<&EmbeddingCache>,
) {
    const MAX: usize = 30;
    const THRESHOLD: f64 = 0.8;
    let n = results.len().min(MAX);
    let mut embeddings: Vec<Vec<f32>> = Vec::with_capacity(n);
    for r in results.iter().take(n) {
        let text = format!("{} {}", r.title, r.content);
        match embed(ai, client, &text, cache).await {
            Some(e) => embeddings.push(e),
            None => return,
        }
    }
    let assignments = assign_clusters(&embeddings, THRESHOLD);
    for (i, cid) in assignments.iter().enumerate() {
        results[i].cluster = Some(*cid);
    }
    // Stable-group results so members of the same cluster are adjacent, in the
    // order the clusters first appeared (keeps the top result on top).
    let mut order: Vec<u32> = Vec::new();
    for cid in &assignments {
        if !order.contains(cid) {
            order.push(*cid);
        }
    }
    let mut grouped: Vec<SearchResult> = Vec::with_capacity(n);
    for cid in &order {
        for (i, a) in assignments.iter().enumerate() {
            if a == cid {
                grouped.push(results[i].clone());
            }
        }
    }
    for (slot, r) in results.iter_mut().take(n).zip(grouped) {
        *slot = r;
    }
}

/// Greedy single-pass clustering with running-average centroids. A result joins
/// the first centroid within `threshold` cosine similarity; otherwise it starts
/// a new cluster. Centroids are updated to the running mean of their members,
/// which tightens clusters versus comparing against only the seed embedding.
/// Pure and unit-testable (no network).
pub fn assign_clusters(embeddings: &[Vec<f32>], threshold: f64) -> Vec<u32> {
    let mut centroids: Vec<(Vec<f32>, usize)> = Vec::new(); // (mean, count)
    let mut out = Vec::with_capacity(embeddings.len());
    for emb in embeddings {
        let mut best: Option<(usize, f64)> = None;
        for (cid, (c, _)) in centroids.iter().enumerate() {
            let sim = cosine(emb, c);
            if sim >= threshold && best.map(|(_, s)| sim > s).unwrap_or(true) {
                best = Some((cid, sim));
            }
        }
        let cid = match best {
            Some((cid, _)) => {
                let (mean, count) = &mut centroids[cid];
                let n = *count as f32;
                for (m, e) in mean.iter_mut().zip(emb.iter()) {
                    *m = (*m * n + *e) / (n + 1.0);
                }
                *count += 1;
                cid
            }
            None => {
                centroids.push((emb.clone(), 1));
                centroids.len() - 1
            }
        };
        out.push(cid as u32);
    }
    out
}

/// Cosine similarity of two equal-length vectors.
pub fn cosine(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f64;
    let mut na = 0.0f64;
    let mut nb = 0.0f64;
    for i in 0..a.len() {
        dot += a[i] as f64 * b[i] as f64;
        na += (a[i] as f64).powi(2);
        nb += (b[i] as f64).powi(2);
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

/// Rewrite RSS/article text into 2–3 neutral sentences for a news card.
pub async fn rewrite_news_card_summary(
    ai: &AiSettings,
    client: &reqwest::Client,
    title: &str,
    source_text: &str,
    model_override: Option<&str>,
) -> Option<String> {
    if !ai.enabled || source_text.trim().len() < 40 {
        return None;
    }
    let text: String = source_text.chars().take(4000).collect();
    let prompt = format!(
        "Rewrite this news article in 2-3 sentences for a reader, neutral tone, same language as source, include key facts.\n\nTitle: {title}\n\nSource text:\n{text}"
    );
    generate_with_model(ai, client, &prompt, model_override)
        .await
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

// ------------------------------------------------------- news article rewrite

/// Minimum timeout (seconds) for a streamed full-page rewrite — long-form
/// generation needs more headroom than the default short-answer timeout.
const ARTICLE_REWRITE_MIN_TIMEOUT: u64 = 90;

/// Default Korean news analysis prompt.
const DEFAULT_NEWS_PROMPT_KO: &str = "당신은 전문 뉴스 분석가입니다. 아래 원문을 바탕으로 간결한 뉴스 분석을 작성하세요.\n\n\
구조 (정확히 따르세요):\n\
1. `## 핵심 요약`으로 시작하여 핵심 사실 3개를 불릿 포인트로 요약.\n\
2. 그 다음 2-3문장의 리드 문단 작성.\n\
3. `## 배경과 맥락` 섹션: 이 사건의 배경과 왜 중요한지 1개 문단.\n\
4. `## 시사점` 섹션: 향후 전망이나 영향을 1개 문단.\n\
5. `## 더 알아보기` 섹션: 독자가 추가로 검색해볼 만한 질문 3개를 불릿 포인트로.\n\
   - 이 기사의 배경 지식이나 관련 인물/기관에 대한 질문\n\
   - 유사 사례나 역사적 맥락을 찾는 질문\n\
   - 향후 전개나 관련 이슈에 대한 질문\n\
6. 총 길이 250-400 단어 (간결하게).\n\n\
언어:\n\
- 자연스러운 한국어로 작성.\n\
- 외국 인명/지명은 한글로 표기 (원어 병기 가능).\n\n\
인용:\n\
- 원문 기사 [1]만 참조. 다른 번호 사용 금지.\n\n\
제목: {title}\n\n\
원문:\n{excerpt}";

/// Default English news analysis prompt.
const DEFAULT_NEWS_PROMPT_EN: &str = "You are a professional news analyst. Write a concise news analysis based on the source below.\n\n\
STRUCTURE (follow exactly):\n\
1. Start with `## KEY POINTS` followed by 3 bullet points summarizing core facts.\n\
2. Then write a 2-3 sentence lede paragraph.\n\
3. `## CONTEXT` section: 1 paragraph on background and why this matters.\n\
4. `## IMPLICATIONS` section: 1 paragraph on future outlook or impact.\n\
5. `## EXPLORE MORE` section: 3 bullet point questions for the reader to research further:\n\
   - A question about background knowledge, key people, or organizations involved\n\
   - A question about similar cases or historical context\n\
   - A question about future developments or related issues\n\
6. Total length 250-400 words (be concise).\n\n\
LANGUAGE: Write in English only.\n\n\
CITATIONS: Use only [1] for the source article. No other numbers.\n\n\
Headline: {title}\n\n\
Source text:\n{excerpt}";

fn news_full_page_prompt(
    ai: &AiSettings,
    title: &str,
    _source_url: &str,
    text: &str,
    perspective: Option<ArticlePerspective>,
) -> String {
    let excerpt: String = text.chars().take(18_000).collect();

    // Determine target language
    let target_lang = if ai.answer_language.is_empty() || ai.answer_language == "auto" {
        // Auto-detect from content
        if crate::article::has_hangul(title) || crate::article::has_hangul(text) {
            "ko"
        } else {
            "en"
        }
    } else {
        &ai.answer_language
    };

    let base_template = match target_lang {
        "ko" => {
            if ai.news_prompt_ko.is_empty() { DEFAULT_NEWS_PROMPT_KO } else { &ai.news_prompt_ko }
        }
        _ => {
            if ai.news_prompt_en.is_empty() { DEFAULT_NEWS_PROMPT_EN } else { &ai.news_prompt_en }
        }
    };

    // Add perspective instruction if specified
    let perspective_instruction = perspective
        .filter(|p| *p != ArticlePerspective::Analyst)
        .map(|p| format!("\n\n⚡ PERSPECTIVE INSTRUCTION:\n{}\n", p.prompt_instruction(target_lang)))
        .unwrap_or_default();

    // For non-English/Korean languages, add strong language instruction
    let template = if !["en", "ko"].contains(&target_lang) {
        let lang_name = match target_lang {
            "ja" => "Japanese (日本語)",
            "zh" => "Chinese (中文)",
            "es" => "Spanish (Español)",
            "fr" => "French (Français)",
            "de" => "German (Deutsch)",
            "pt" => "Portuguese (Português)",
            "it" => "Italian (Italiano)",
            "ru" => "Russian (Русский)",
            "ar" => "Arabic (العربية)",
            "vi" => "Vietnamese (Tiếng Việt)",
            "th" => "Thai (ไทย)",
            "id" => "Indonesian (Bahasa Indonesia)",
            _ => target_lang,
        };
        format!(
            "⚠️ CRITICAL LANGUAGE REQUIREMENT ⚠️\n\
            You MUST write the ENTIRE response in {} ONLY.\n\
            Do NOT use English. Every single word must be in {}.\n\
            This is absolutely mandatory - responses in English will be rejected.\n\n\
            {}{}\n\n\
            REMINDER: Write everything in {} only. No English allowed.",
            lang_name, lang_name, base_template, perspective_instruction, lang_name
        )
    } else {
        format!("{}{}", base_template, perspective_instruction)
    };

    template.replace("{title}", title).replace("{excerpt}", &excerpt)
}

/// Non-streaming full-page news rewrite (~250-400 words, ## sections).
pub async fn rewrite_news_full_page(
    ai: &AiSettings,
    client: &reqwest::Client,
    title: &str,
    source_url: &str,
    text: &str,
    model_override: Option<&str>,
    perspective: Option<ArticlePerspective>,
) -> Result<String, String> {
    if text.trim().len() < 40 {
        return Err("source text too short".into());
    }
    let prompt = news_full_page_prompt(ai, title, source_url, text, perspective);
    let model = effective_article_model(ai, model_override);
    let timeout = ai.timeout_secs.max(ARTICLE_REWRITE_MIN_TIMEOUT);
    generate_resolved(ai, client, &prompt, &model, timeout)
        .await
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "model unreachable or empty response".into())
}

/// Result of streaming article generation including usage stats.
pub struct StreamArticleResult {
    pub article: String,
    pub usage: Option<TokenUsage>,
}

/// Stream a full-page news rewrite token-by-token.
pub async fn stream_news_article(
    ai: &AiSettings,
    client: &reqwest::Client,
    title: &str,
    source_url: &str,
    text: &str,
    model_override: Option<&str>,
    perspective: Option<ArticlePerspective>,
    mut on_token: impl FnMut(&str),
) -> Result<StreamArticleResult, String> {
    if text.trim().len() < 40 {
        return Err("source text too short".into());
    }
    let prompt = news_full_page_prompt(ai, title, source_url, text, perspective);
    let model = effective_article_model(ai, model_override);
    let timeout = std::time::Duration::from_secs(ai.timeout_secs.max(ARTICLE_REWRITE_MIN_TIMEOUT));
    let mut article = String::new();
    let mut usage: Option<TokenUsage> = None;

    // Use OpenAI-compatible API if api_key is set
    if let Some(ref api_key) = ai.api_key {
        let url = format!("{}/chat/completions", ai.base_url.trim_end_matches('/'));
        let body = json!({
            "model": model,
            "messages": [{"role": "user", "content": prompt}],
            "stream": true,
            "stream_options": {"include_usage": true}
        });
        let resp = client
            .post(&url)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| format!("model unreachable: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("model returned status {}", resp.status()));
        }

        let mut resp = resp;
        let mut buf: Vec<u8> = Vec::new();
        loop {
            let chunk = match resp.chunk().await {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => return Err(format!("stream read error: {e}")),
            };
            buf.extend_from_slice(&chunk);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line).trim().to_string();
                if line.is_empty() || line == "data: [DONE]" {
                    continue;
                }
                if let Some(json_str) = line.strip_prefix("data: ") {
                    if let Ok(v) = serde_json::from_str::<Value>(json_str) {
                        // Parse content tokens
                        if let Some(tok) = v["choices"][0]["delta"]["content"].as_str() {
                            if !tok.is_empty() {
                                article.push_str(tok);
                                on_token(tok);
                            }
                        }
                        // Parse usage from final chunk (OpenAI sends this with stream_options)
                        if let Some(u) = v.get("usage") {
                            let input = u["prompt_tokens"].as_u64().unwrap_or(0);
                            let output = u["completion_tokens"].as_u64().unwrap_or(0);
                            if input > 0 || output > 0 {
                                let mut token_usage = TokenUsage::new(input, output)
                                    .with_model(&model);
                                if ai.input_cost_per_million > 0.0 || ai.output_cost_per_million > 0.0 {
                                    token_usage = token_usage.with_cost(
                                        ai.input_cost_per_million,
                                        ai.output_cost_per_million,
                                    );
                                }
                                usage = Some(token_usage);
                            }
                        }
                    }
                }
            }
        }
    } else {
        // Fallback to Ollama API
        let url = format!("{}/api/generate", ai.base_url.trim_end_matches('/'));
        let body = json!({ "model": model, "prompt": prompt, "stream": true });
        let resp = client
            .post(&url)
            .json(&body)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| format!("model unreachable: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("model returned status {}", resp.status()));
        }

        let mut resp = resp;
        let mut buf: Vec<u8> = Vec::new();
        loop {
            let chunk = match resp.chunk().await {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => return Err(format!("stream read error: {e}")),
            };
            buf.extend_from_slice(&chunk);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let line = &line[..line.len().saturating_sub(1)];
                if line.is_empty() {
                    continue;
                }
                if let Ok(v) = serde_json::from_slice::<Value>(line) {
                    if let Some(tok) = v["response"].as_str() {
                        if !tok.is_empty() {
                            article.push_str(tok);
                            on_token(tok);
                        }
                    }
                    // Ollama provides usage in the final "done" response
                    if v["done"].as_bool() == Some(true) {
                        if let (Some(input), Some(output)) = (
                            v["prompt_eval_count"].as_u64(),
                            v["eval_count"].as_u64(),
                        ) {
                            usage = Some(TokenUsage::new(input, output).with_model(&model));
                        }
                        buf.clear();
                        break;
                    }
                }
            }
        }
        if !buf.is_empty() {
            if let Ok(v) = serde_json::from_slice::<Value>(&buf) {
                if let Some(tok) = v["response"].as_str() {
                    if !tok.is_empty() {
                        article.push_str(tok);
                        on_token(tok);
                    }
                }
            }
        }
    }

    let trimmed = article.trim().to_string();
    if trimmed.is_empty() {
        return Err("model produced empty article".into());
    }
    Ok(StreamArticleResult {
        article: trimmed,
        usage,
    })
}

// ---------------------------------------------------------- Ollama transport

/// Call the Ollama-compatible `/api/generate` endpoint (non-streaming).
async fn generate(ai: &AiSettings, client: &reqwest::Client, prompt: &str) -> Option<String> {
    generate_with_model(ai, client, prompt, None).await
}

async fn generate_with_model(
    ai: &AiSettings,
    client: &reqwest::Client,
    prompt: &str,
    model_override: Option<&str>,
) -> Option<String> {
    let model = effective_model(ai, model_override);
    generate_resolved(ai, client, prompt, &model, ai.timeout_secs.max(1)).await
}

/// Non-streaming generate against an already-resolved model name and timeout.
/// Uses OpenAI-compatible API when api_key is set, otherwise falls back to Ollama.
async fn generate_resolved(
    ai: &AiSettings,
    client: &reqwest::Client,
    prompt: &str,
    model: &str,
    timeout_secs: u64,
) -> Option<String> {
    let timeout = std::time::Duration::from_secs(timeout_secs.max(1));

    // Use OpenAI-compatible API if api_key is set (Groq, Together, OpenAI, etc.)
    if let Some(ref api_key) = ai.api_key {
        let url = format!("{}/chat/completions", ai.base_url.trim_end_matches('/'));
        let body = json!({
            "model": model,
            "messages": [{"role": "user", "content": prompt}],
            "stream": false
        });
        let resp = client
            .post(&url)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .timeout(timeout)
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let v: Value = resp.json().await.ok()?;
        return v["choices"][0]["message"]["content"].as_str().map(|s| s.to_string());
    }

    // Fallback to Ollama API
    let url = format!("{}/api/generate", ai.base_url.trim_end_matches('/'));
    let body = json!({ "model": model, "prompt": prompt, "stream": false });
    let resp = client
        .post(&url)
        .json(&body)
        .timeout(timeout)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: Value = resp.json().await.ok()?;
    v["response"].as_str().map(|s| s.to_string())
}

/// Call the Ollama-compatible `/api/embeddings` endpoint (with optional cache).
async fn embed(
    ai: &AiSettings,
    client: &reqwest::Client,
    text: &str,
    cache: Option<&EmbeddingCache>,
) -> Option<Vec<f32>> {
    if let Some(c) = cache {
        if let Some(hit) = c.get(&ai.embedding_model, text) {
            return Some(hit);
        }
    }
    let url = format!("{}/api/embeddings", ai.base_url.trim_end_matches('/'));
    let body = json!({ "model": ai.embedding_model, "prompt": text });
    let resp = client
        .post(&url)
        .json(&body)
        .timeout(std::time::Duration::from_secs(ai.timeout_secs.max(1)))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: Value = resp.json().await.ok()?;
    let arr = v["embedding"].as_array()?;
    let vec: Vec<f32> = arr
        .iter()
        .filter_map(|x| x.as_f64().map(|f| f as f32))
        .collect();
    if vec.is_empty() {
        None
    } else {
        if let Some(c) = cache {
            c.put(&ai.embedding_model, text, vec.clone());
        }
        Some(vec)
    }
}

/// Best-effort extraction of a JSON string array from a model reply (which may
/// wrap the array in prose or code fences).
fn parse_string_array(text: &str) -> Vec<String> {
    let start = text.find('[');
    let end = text.rfind(']');
    if let (Some(s), Some(e)) = (start, end) {
        if e > s {
            if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&text[s..=e]) {
                return items
                    .into_iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect();
            }
        }
    }
    Vec::new()
}

/// Select the best "top story" from a list of news articles using AI.
/// Returns the 0-based index of the most newsworthy article for the featured position.
/// Falls back to 0 (first article) if AI is unavailable or fails.
pub async fn select_top_story(
    ai: &AiSettings,
    client: &reqwest::Client,
    articles: &[(String, String)], // (title, teaser)
) -> usize {
    if articles.is_empty() || !ai.enabled {
        return 0;
    }

    // Limit to top 5 candidates to keep prompt short
    let candidates: Vec<_> = articles.iter().take(5).collect();
    if candidates.len() <= 1 {
        return 0;
    }

    let mut prompt = String::from(
        "You are a news editor selecting the TOP STORY for a news homepage.\n\
         Select the MOST IMPORTANT and NEWSWORTHY article from this list.\n\
         Criteria:\n\
         - Major political, economic, or social impact\n\
         - Breaking news over routine announcements\n\
         - National/international news over local government press releases\n\
         - Avoid: advertisements, promotions, opinion pieces, webtoons, episode content\n\n\
         Articles:\n"
    );

    for (i, (title, teaser)) in candidates.iter().enumerate() {
        prompt.push_str(&format!("{}. {} - {}\n", i + 1, title, teaser.chars().take(100).collect::<String>()));
    }

    prompt.push_str("\nRespond with ONLY the number (1-5) of the best top story. Nothing else.");

    let result = generate_with_model(ai, client, &prompt, None).await;

    if let Some(text) = result {
        // Parse the number from response
        let num: Option<usize> = text.trim().chars()
            .filter(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .ok();

        if let Some(n) = num {
            if n >= 1 && n <= candidates.len() {
                return n - 1; // Convert to 0-based index
            }
        }
    }

    0 // Fallback to first article
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_identical_is_one() {
        let a = vec![1.0, 2.0, 3.0];
        assert!((cosine(&a, &a) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn cosine_orthogonal_is_zero() {
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-9);
    }

    #[test]
    fn parses_json_array_from_noisy_reply() {
        let got = parse_string_array("Sure! [\"a\", \"b\", \"c\"] hope that helps");
        assert_eq!(got, vec!["a", "b", "c"]);
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    /// Build a couple of fixture results without any network.
    fn fixture_results() -> Vec<SearchResult> {
        use crate::types::EngineResult;
        let per_engine = vec![(
            "fixture".to_string(),
            vec![
                EngineResult::new("https://a.example/", "Alpha", "first result about rust"),
                EngineResult::new("https://b.example/", "Beta", "second result about rust"),
            ],
        )];
        let mut weights = std::collections::HashMap::new();
        weights.insert("fixture".to_string(), 1.0);
        crate::aggregate::aggregate(per_engine, &weights)
    }

    /// AiSettings pointed at an unreachable endpoint with a short timeout, so
    /// every AI feature must degrade cleanly (no hang) — exercised offline.
    fn unreachable_ai() -> AiSettings {
        AiSettings {
            enabled: true,
            // Port 1 is reserved/refused on loopback → immediate connection
            // error, no network round-trip.
            base_url: "http://127.0.0.1:1".into(),
            timeout_secs: 2,
            answer: true,
            expand: true,
            rerank: true,
            cluster: true,
            ..AiSettings::default()
        }
    }

    #[test]
    fn article_model_prefers_override_then_article_then_model() {
        let mut ai = AiSettings {
            model: "chat-test-model".into(),
            article_model: "article-test-model".into(),
            ..AiSettings::default()
        };
        // Explicit override wins.
        assert_eq!(
            effective_article_model(&ai, Some("gpt-oss:20b")),
            "gpt-oss:20b"
        );
        // Blank override falls back to article_model.
        assert_eq!(
            effective_article_model(&ai, Some("  ")),
            "article-test-model"
        );
        assert_eq!(effective_article_model(&ai, None), "article-test-model");
        // Empty article_model falls back to the general model.
        ai.article_model = "".into();
        assert_eq!(effective_article_model(&ai, None), "chat-test-model");
    }

    #[test]
    fn news_prompt_demands_structure_language_and_citation() {
        let p = news_full_page_prompt("Headline", "https://example.com/a", &"x".repeat(80));
        assert!(p.contains("## "));
        assert!(p.contains("[1]"));
        assert!(p.to_lowercase().contains("same language"));
        let ko = news_full_page_prompt(
            "박근혜·이명박 '동시 등판'",
            "https://news.kbs.co.kr/x",
            &"x".repeat(80),
        );
        assert!(ko.contains("한글"));
    }

    #[tokio::test]
    async fn synthesize_answer_degrades_when_unreachable() {
        let client = reqwest::Client::new();
        let results = fixture_results();
        let started = std::time::Instant::now();
        let answer = synthesize_answer(&unreachable_ai(), &client, "rust", &results).await;
        assert!(
            answer.is_none(),
            "must degrade to no answer when unreachable"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "degradation must be bounded, took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn expand_and_refine_degrade_when_unreachable() {
        let client = reqwest::Client::new();
        let ai = unreachable_ai();
        assert!(expand_query(&ai, &client, "rust").await.is_empty());
        assert!(plan_subqueries(&ai, &client, "rust async runtimes")
            .await
            .is_empty());
        assert!(
            refine_query(&ai, &client, "rust async", "which is fastest?")
                .await
                .is_none()
        );
    }

    #[test]
    fn build_citations_indexes_from_top_n() {
        let results = fixture_results();
        let cites = build_citations(&results, 1);
        assert_eq!(cites.len(), 1);
        assert_eq!(cites[0].index, 1);
        assert_eq!(cites[0].url, "https://a.example/");
    }

    #[test]
    fn append_footnotes_when_no_inline_markers() {
        let cites = vec![Citation {
            index: 1,
            title: "Alpha".into(),
            url: "https://a.example/".into(),
            snippet: "s".into(),
            engine: "fixture".into(),
        }];
        let out = append_citation_footnotes("Summary without markers.", &cites);
        assert!(out.contains("Sources:"));
        assert!(out.contains("[1] Alpha"));
        let kept = append_citation_footnotes("Already cited [1].", &cites);
        assert_eq!(kept, "Already cited [1].");
    }

    #[test]
    fn has_inline_citations_detects_markers() {
        assert!(has_inline_citations("See [1] and [2][3]."));
        assert!(!has_inline_citations("No markers here."));
    }

    #[test]
    fn query_language_detects_korean() {
        assert_eq!(query_language("테슬라 주가"), Some("Korean"));
        assert_eq!(query_language("tesla stock"), None);
    }

    #[test]
    fn detect_locale_maps_scripts_to_locales() {
        // Korean Hangul → ko-KR (the primary fix); mixed Latin still detects.
        assert_eq!(detect_locale("양자컴퓨팅"), Some("ko-KR"));
        assert_eq!(detect_locale("삼성 Galaxy 신제품"), Some("ko-KR"));
        // Japanese kana wins over the Han ideographs it mixes with.
        assert_eq!(detect_locale("東京の天気はどう"), Some("ja-JP"));
        // Pure Han → Chinese.
        assert_eq!(detect_locale("量子计算机"), Some("zh-CN"));
        assert_eq!(detect_locale("Москва погода"), Some("ru-RU"));
        // Plain Latin text is ambiguous → no detection (caller uses default).
        assert_eq!(detect_locale("tesla stock price"), None);
        assert_eq!(detect_locale(""), None);
    }

    #[test]
    fn language_hint_is_firm_and_named_for_korean() {
        let hint = language_hint("테슬라 최신 소식");
        assert!(
            hint.contains("ENTIRE answer in Korean"),
            "Korean queries must pin the answer language explicitly: {hint}"
        );
        assert!(
            hint.contains("Do NOT use any other language"),
            "hint must forbid mixing in other languages: {hint}"
        );
    }

    #[test]
    fn language_hint_mirrors_query_when_undetected() {
        let hint = language_hint("tesla stock price");
        assert!(
            hint.contains("SAME language and script as"),
            "undetected queries must mirror the query language: {hint}"
        );
        assert!(hint.contains("Do NOT mix in any other language"));
    }

    #[test]
    fn answer_prompt_embeds_language_and_relevance_rules() {
        let prompt = answer_prompt(
            "테슬라 실적",
            "[1] x\nhttps://x\nbody\n",
            FocusMode::General,
        );
        assert!(
            prompt.contains("ENTIRE answer in Korean"),
            "prompt must carry the firm Korean language constraint"
        );
        assert!(
            prompt.contains("cite ONLY sources that are directly about the query's subject"),
            "prompt must instruct the model to ignore irrelevant sources"
        );
    }

    #[tokio::test]
    async fn rerank_and_cluster_leave_results_untouched_when_unreachable() {
        let client = reqwest::Client::new();
        let ai = unreachable_ai();
        let mut results = fixture_results();
        let before: Vec<String> = results.iter().map(|r| r.url.clone()).collect();
        rerank(&ai, &client, "rust", &mut results, None).await;
        let after: Vec<String> = results.iter().map(|r| r.url.clone()).collect();
        assert_eq!(before, after, "rerank must not reorder on embed failure");

        cluster(&ai, &client, &mut results, None).await;
        assert!(
            results.iter().all(|r| r.cluster.is_none()),
            "cluster must be a no-op on embed failure"
        );
    }

    #[tokio::test]
    async fn caption_image_degrades_when_unreachable() {
        let client = reqwest::Client::new();
        let ai = unreachable_ai();
        // Even with a syntactically valid image URL, an unreachable fetch/model
        // returns None rather than hanging.
        let cap = caption_image(&ai, &client, "http://127.0.0.1:1/x.png").await;
        assert!(cap.is_none());
    }

    #[test]
    fn embedding_cache_serves_repeat_lookup() {
        let cache = EmbeddingCache::new(4);
        let vec = vec![0.1, 0.2, 0.3];
        cache.put("nomic-embed-text", "rust async", vec.clone());
        assert_eq!(cache.get("nomic-embed-text", "rust async"), Some(vec));
        assert!(cache.get("nomic-embed-text", "other text").is_none());
    }

    #[test]
    fn clustering_groups_similar_and_separates_distinct() {
        // Two tight groups along orthogonal axes.
        let embeddings = vec![
            vec![1.0, 0.0],
            vec![0.98, 0.02],
            vec![0.0, 1.0],
            vec![0.02, 0.98],
        ];
        let ids = assign_clusters(&embeddings, 0.9);
        assert_eq!(ids[0], ids[1]); // first pair clustered together
        assert_eq!(ids[2], ids[3]); // second pair clustered together
        assert_ne!(ids[0], ids[2]); // the two groups are distinct
    }

    // ------------------------------------------------ citations & follow-ups

    #[test]
    fn build_citations_maps_index_to_source() {
        let results = fixture_results();
        let cites = build_citations(&results, 5);
        assert_eq!(cites.len(), results.len());
        // 1-indexed, contiguous, and each marker resolves to the matching source.
        for (i, c) in cites.iter().enumerate() {
            assert_eq!(c.index, i + 1, "citations must be 1-indexed and contiguous");
            assert_eq!(c.url, results[i].url);
            assert_eq!(c.title, results[i].title);
            assert_eq!(c.engine, results[i].engine);
            assert!(!c.engine.is_empty(), "engine should be populated");
        }
    }

    #[test]
    fn build_citations_respects_top_n_cap() {
        let results = fixture_results();
        let cites = build_citations(&results, 1);
        assert_eq!(cites.len(), 1, "top_n caps the citation count");
        assert_eq!(cites[0].index, 1);
        // top_n of 0 is clamped to at least 1 (never panics / empties a non-empty set).
        assert_eq!(build_citations(&results, 0).len(), 1);
        assert!(build_citations(&[], 5).is_empty());
    }

    #[test]
    fn build_citations_truncates_snippet() {
        use crate::types::EngineResult;
        let long = "x".repeat(1000);
        let per_engine = vec![(
            "fixture".to_string(),
            vec![EngineResult::new("https://a.example/", "Alpha", long)],
        )];
        let mut weights = std::collections::HashMap::new();
        weights.insert("fixture".to_string(), 1.0);
        let results = crate::aggregate::aggregate(per_engine, &weights);
        let cites = build_citations(&results, 5);
        assert_eq!(
            cites[0].snippet.chars().count(),
            CITATION_SNIPPET_CHARS,
            "snippet must be truncated to the citation budget"
        );
    }

    #[test]
    fn is_no_answer_detects_sentinel_not_substring() {
        assert!(is_no_answer(""));
        assert!(is_no_answer("   "));
        assert!(is_no_answer("NO_ANSWER"));
        assert!(is_no_answer("no_answer"));
        assert!(is_no_answer("NO_ANSWER — nothing relevant"));
        // A real answer that merely mentions the token must NOT be treated as a
        // refusal.
        assert!(!is_no_answer(
            "The results explain that NO_ANSWER is a sentinel string [1]."
        ));
        assert!(!is_no_answer("Rust has several async runtimes [1][2]."));
    }

    #[test]
    fn answer_context_is_numbered_and_capped() {
        let results = fixture_results();
        let ctx = answer_context(&results, 1);
        assert!(ctx.starts_with("[1] "), "context must be 1-indexed");
        assert!(ctx.contains("https://a.example/"));
        // Capped at top_n = 1, so the second source must be absent.
        assert!(!ctx.contains("https://b.example/"));
    }

    #[test]
    fn answer_chunk_serializes_with_type_tag() {
        let tok = serde_json::to_string(&AnswerChunk::Token { text: "hi".into() }).unwrap();
        assert_eq!(tok, r#"{"type":"token","text":"hi"}"#);
        let err = serde_json::to_string(&AnswerChunk::Error {
            message: "x".into(),
        })
        .unwrap();
        assert_eq!(err, r#"{"type":"error","message":"x"}"#);
    }

    #[tokio::test]
    async fn stream_answer_degrades_when_unreachable() {
        let client = reqwest::Client::new();
        let results = fixture_results();
        let mut tokens = 0usize;
        let started = std::time::Instant::now();
        let out = stream_answer(&unreachable_ai(), &client, "rust", &results, |_| {
            tokens += 1;
        })
        .await;
        assert!(out.is_err(), "unreachable model must yield an error");
        assert_eq!(tokens, 0, "no partial tokens may be emitted on failure");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "degradation must be bounded, took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn stream_answer_errors_on_empty_results() {
        let client = reqwest::Client::new();
        let mut tokens = 0usize;
        let out = stream_answer(&unreachable_ai(), &client, "rust", &[], |_| tokens += 1).await;
        assert!(out.is_err());
        assert_eq!(tokens, 0);
    }

    #[tokio::test]
    async fn suggest_followups_degrades_when_unreachable() {
        let client = reqwest::Client::new();
        let results = fixture_results();
        let fu = suggest_followups(&unreachable_ai(), &client, "rust", &results).await;
        assert!(
            fu.is_empty(),
            "must degrade to no follow-ups when unreachable"
        );
        // Empty result set short-circuits to no follow-ups (no model call).
        assert!(suggest_followups(&unreachable_ai(), &client, "rust", &[])
            .await
            .is_empty());
    }
}
