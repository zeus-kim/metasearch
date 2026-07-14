# Metasearch

**Self-hostable, privacy-first answer engine with real-time news and multi-language support — single Rust binary.**

## Features

- **AI-powered answers** — streaming cited answers from local Ollama
- **335K+ sources** — 56 search engines, 311K news feeds, 24K radio stations
- **Privacy-first** — no query logging, no telemetry, no accounts
- **Multi-language** — 63 UI languages, auto-detect query language
- **News aggregation** — Discover feed with trending topics
- **Boost mode** — tier-1 news sources indexed in 2-3 minutes on first run
- **Offline-capable** — PWA with service worker caching

## Quick Start

```bash
# Build and run
cargo build --release
./target/release/metasearch

# Or use the helper script
./scripts/run.sh
```

Open http://127.0.0.1:8889/

## Requirements

- Rust 1.75+
- Ollama (optional, for AI answers)

```bash
# Install Ollama and pull a model
ollama pull llama3.2:3b
```

## Configuration

Copy and edit `settings.yml`:

```yaml
server:
  bind_address: "127.0.0.1"
  port: 8889

ai:
  enabled: true
  model: "llama3.2:3b"
  base_url: "http://127.0.0.1:11434"

search:
  default_lang: "auto"
  safe_search: 1
```

### Environment Variables

| Variable | Description |
|----------|-------------|
| `METASEARCH_SETTINGS` | Path to settings file |
| `METASEARCH_BIND` | Bind address |
| `METASEARCH_PORT` | Server port |
| `METASEARCH_AI_BASE_URL` | Ollama URL |
| `METASEARCH_AI_MODEL` | AI model (e.g. gemma4:e4b, llama3.2:3b) |
| `OPENAI_API_KEY` | API key for AI features (OpenAI or Ollama) |

### Using Cloud APIs

**OpenAI GPT:**
```bash
export OPENAI_API_KEY=sk-your-key
export METASEARCH_AI_MODEL=gpt-4o-mini
```

**Other providers** (Claude, Gemini) require an OpenAI-compatible proxy:
```bash
# Using LiteLLM as proxy
export METASEARCH_AI_BASE_URL=http://localhost:4000
export METASEARCH_AI_MODEL=claude-3-haiku
```

## MCP Integration

Metasearch includes an MCP server for Claude Desktop / Cursor:

```bash
cargo build --release --bin metasearch-mcp
```

Add to `~/.claude/claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "metasearch": {
      "command": "/path/to/metasearch-mcp",
      "env": {
        "METASEARCH_SETTINGS": "/path/to/settings.yml"
      }
    }
  }
}
```

**Available tools:** `metasearch_search`, `metasearch_answer`, `metasearch_image_search`, `metasearch_research`

## News Briefing (Optional)

AI-generated audio news briefings:

```bash
pip install aiohttp edge-tts
export OLLAMA_URL=http://localhost:11434
python services/briefing_server.py
```

Languages: ko, en, ja, zh, es, fr, de, pt, ru, ar

## API Endpoints

| Route | Description |
|-------|-------------|
| `GET /` | Answer UI (main interface) |
| `GET /search?q=&format=json` | Search API |
| `GET /answer?q=` | Streaming AI answer (SSE) |
| `GET /api/v1/search` | Agent search API |
| `GET /api/v1/answer` | Non-streaming answer |
| `GET /api/v1/trending?geo=US` | Trending topics |
| `GET /api/v1/news_digest?q=` | News digest |
| `GET /api/v1/health` | Health check |

## Docker

```bash
docker build -t metasearch .
docker run -p 8889:8889 -e METASEARCH_BIND=0.0.0.0 metasearch
```

## Testing

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```

## License

MIT License
