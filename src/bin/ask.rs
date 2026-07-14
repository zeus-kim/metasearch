//! `ask` — a terminal client for metasearch with cited answers.
//!
//! Runs the same search pipeline as the server, then streams a grounded, cited
//! answer from the local Ollama to the terminal (token-by-token), followed by a
//! numbered source list. It reads the same `settings.yml` / environment as the
//! server, and talks only to the user-configured local model (privacy-first).
//!
//! ```bash
//! cargo run --bin ask -- "what is the fastest async runtime for rust?"
//! cargo run --bin ask -- --model llama3.2:3b "tokyo weather today"
//! ```
//!
//! Flags:
//!   -m, --model <NAME>      override the chat model (else config / default)
//!       --base-url <URL>    override the Ollama base URL
//!   -n, --top <N>           number of sources to ground on (default: config)
//!   -h, --help              print usage and exit
//!
//! Config resolution mirrors the server: `$METASEARCH_SETTINGS` or `./settings.yml`,
//! falling back to built-in defaults. AI answer synthesis is force-enabled for
//! the CLI (its whole purpose), but still degrades gracefully — with no reachable
//! model you get the ranked sources and a clear notice instead of an answer.

use std::io::Write;
use std::path::PathBuf;

use metasearch::{ai, build_citations, search_all, Runtime, SearchParams, Settings};

struct Args {
    question: String,
    model: Option<String>,
    base_url: Option<String>,
    top: Option<usize>,
}

fn print_usage() {
    eprintln!(
        "ask — cited answers from your terminal\n\n\
USAGE:\n  ask [OPTIONS] <question...>\n\n\
OPTIONS:\n  \
-m, --model <NAME>     chat model to use (default: config / llama3.2)\n  \
    --base-url <URL>   Ollama base URL (default: http://127.0.0.1:11434)\n  \
-n, --top <N>          number of sources to ground on (default: config)\n  \
-h, --help             show this help\n\n\
EXAMPLES:\n  \
ask \"what is the fastest async runtime for rust?\"\n  \
ask --model llama3.2:3b \"tokyo weather today\""
    );
}

fn parse_args() -> Result<Args, i32> {
    let mut model = None;
    let mut base_url = None;
    let mut top = None;
    let mut words: Vec<String> = Vec::new();

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_usage();
                return Err(0);
            }
            "-m" | "--model" => model = it.next(),
            "--base-url" => base_url = it.next(),
            "-n" | "--top" => top = it.next().and_then(|v| v.parse::<usize>().ok()),
            // Bare `--`: treat the remainder as the question verbatim.
            "--" => {
                words.extend(it.by_ref());
                break;
            }
            other => words.push(other.to_string()),
        }
    }

    let question = words.join(" ").trim().to_string();
    if question.is_empty() {
        print_usage();
        return Err(2);
    }
    Ok(Args {
        question,
        model,
        base_url,
        top,
    })
}

#[tokio::main]
async fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(code) => std::process::exit(code),
    };

    let path = std::env::var("METASEARCH_SETTINGS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("settings.yml"));
    let mut settings = Settings::load_or_default(path);

    // The CLI's whole purpose is a cited answer, so force the answer path on —
    // but keep it snappy and predictable by leaving the heavier always-on
    // enhancements (rerank/cluster/expand/vision) off for the terminal.
    settings.ai.enabled = true;
    settings.ai.answer = true;
    settings.ai.rerank = false;
    settings.ai.cluster = false;
    settings.ai.expand = false;
    settings.ai.vision = false;
    if let Some(m) = args.model {
        settings.ai.model = m;
    }
    if let Some(u) = args.base_url {
        settings.ai.base_url = u;
    }
    if let Some(n) = args.top {
        settings.ai.answer_top_n = n.max(1);
    }

    let rt = Runtime::new(&settings);

    // Status/progress on stderr so stdout carries only the answer + sources.
    eprintln!("· searching “{}” …", args.question);
    let mut params = SearchParams::new(&args.question);
    // Avoid a duplicate buffered synthesis — we stream the answer below.
    params.ai_answer = Some(false);
    let response = search_all(&params, &settings, &rt).await;

    if response.results.is_empty() {
        eprintln!("· no results.");
        std::process::exit(1);
    }

    let citations = build_citations(&response.results, settings.ai.answer_top_n);

    eprintln!("· answering with {} …\n", settings.ai.model);
    let mut stdout = std::io::stdout();
    let streamed = ai::stream_answer(
        &settings.ai,
        &rt.client,
        &response.query,
        &response.results,
        |tok| {
            let _ = stdout.write_all(tok.as_bytes());
            let _ = stdout.flush();
        },
    )
    .await;

    match streamed {
        Ok(_) => {
            println!();
        }
        Err(e) => {
            // Graceful degradation: no model → still show the ranked sources.
            eprintln!("· no AI answer ({e}); showing sources only.");
        }
    }

    println!("\nSources:");
    for c in &citations {
        println!("  [{}] {}\n      {}", c.index, c.title, c.url);
    }
}
