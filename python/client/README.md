# metasearch-client

Python client for [Metasearch](https://github.com/zeus-kim/metasearch) - search 200+ engines with AI answers.

## Installation

```bash
pip install metasearch-client
```

## Quick Start

```python
from metasearch_client import Metasearch

# Connect to local instance
ms = Metasearch("http://localhost:8889")

# Search
results = ms.search("Rust async programming")
for r in results[:5]:
    print(f"{r.title} - {r.url}")

# AI Answer with citations
answer = ms.answer("What is the difference between Rust and Go?")
print(answer.text)
for cite in answer.citations:
    print(f"  [{cite['index']}] {cite['title']}")

# News feed
news = ms.discover(lang="ko", category="tech")
for article in news[:5]:
    print(f"{article['title']} - {article['source']}")

# Trending topics
trends = ms.trending(geo="KR")
for t in trends[:10]:
    print(t["title"])
```

## API Reference

### `Metasearch(base_url, timeout=30)`

Create a client instance.

### Methods

| Method | Description |
|--------|-------------|
| `search(query, categories, lang, page)` | Search across engines |
| `answer(query, focus, model, stream)` | Get AI-grounded answer |
| `research(query, focus, deep)` | Deep research with subqueries |
| `discover(lang, category, limit)` | Curated news feed |
| `trending(geo)` | Trending topics |
| `images(query, lang)` | Image search |
| `news_digest(query, lang)` | AI news summary |
| `health()` | Server health check |

### Focus modes for `answer()`

- `general` - Balanced answer
- `academic` - Scholarly sources
- `code` - Programming focus
- `news` - Recent events

## License

MIT
