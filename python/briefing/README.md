# metasearch-briefing

AI-generated audio news briefings with Edge TTS.

## Installation

```bash
pip install metasearch-briefing
```

## Quick Start

```python
import asyncio
from metasearch_briefing import BriefingGenerator

async def main():
    gen = BriefingGenerator(
        metasearch_url="http://localhost:8889",
        ollama_url="http://localhost:11434",
        model="gemma3:4b",
    )
    
    briefing = await gen.create(lang="ko")
    print(briefing.script)
    print(f"Audio: {briefing.audio_path}")

asyncio.run(main())
```

## CLI

```bash
metasearch-briefing ko  # Korean
metasearch-briefing en  # English
metasearch-briefing ja  # Japanese
```

## Supported Languages

| Code | Language | Voice |
|------|----------|-------|
| ko | Korean | SunHiNeural |
| en | English | JennyNeural |
| ja | Japanese | NanamiNeural |
| zh | Chinese | XiaoxiaoNeural |
| es | Spanish | ElviraNeural |
| fr | French | DeniseNeural |
| de | German | KatjaNeural |
| pt | Portuguese | FranciscaNeural |
| ru | Russian | SvetlanaNeural |
| ar | Arabic | ZariyahNeural |

## Requirements

- Metasearch server running
- Ollama with a model (gemma3:4b recommended)

## License

MIT
