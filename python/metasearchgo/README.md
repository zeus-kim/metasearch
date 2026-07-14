# metasearchgo

Privacy-first search with 200+ engines and AI answers.

## Installation

```bash
pip install metasearchgo
```

## Quick Start

```python
from metasearchgo import Metasearch, BriefingGenerator

# Search
ms = Metasearch("http://localhost:8889")
results = ms.search("Rust async")
answer = ms.answer("What is Rust?")

# News feed
news = ms.discover(lang="ko")
trends = ms.trending(geo="KR")

# Audio briefing
import asyncio
gen = BriefingGenerator()
briefing = asyncio.run(gen.create("ko"))
print(briefing.audio_path)
```

## Features

- **Search**: 200+ engines, images, news, videos
- **AI Answers**: Grounded answers with citations
- **News Feed**: Curated discover feed
- **Trending**: Real-time trending topics
- **Briefing**: AI-generated audio news (10 languages)

## Supported Languages

ko, en, ja, zh, es, fr, de, pt, ru, ar

## License

MIT
