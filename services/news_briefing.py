#!/usr/bin/env python3
"""
News Briefing Service
- Fetches headlines from metasearch discover_snapshot
- Generates script using LLM (ollama)
- Produces audio using Edge TTS
"""

import asyncio
import json
import hashlib
import os
import time
from pathlib import Path
from datetime import datetime
import aiohttp
import edge_tts

# Config
METASEARCH_URL = "http://localhost:8889"
OLLAMA_URL = os.environ.get("OLLAMA_URL", "http://localhost:11434")
MODEL = "gemma4:e4b"
VOICE_KO = "ko-KR-SunHiNeural"
VOICE_EN = "en-US-JennyNeural"
OUTPUT_DIR = Path(__file__).parent.parent / "data" / "briefings"
OUTPUT_DIR.mkdir(parents=True, exist_ok=True)

SCRIPT_PROMPT_KO = """다음 뉴스 헤드라인들을 바탕으로 1분 분량의 뉴스 브리핑 스크립트를 작성하세요.
- 자연스러운 뉴스 앵커 톤으로
- 각 뉴스를 1-2문장으로 요약
- 인사말과 마무리 포함
- 총 200-300자 내외

헤드라인:
{headlines}

스크립트:"""

SCRIPT_PROMPT_EN = """Write a 1-minute news briefing script based on these headlines.
- Natural news anchor tone
- Summarize each news in 1-2 sentences
- Include greeting and closing
- About 150-200 words

Headlines:
{headlines}

Script:"""


async def fetch_news(lang: str = "ko", limit: int = 10) -> list:
    """Fetch news from metasearch"""
    async with aiohttp.ClientSession() as session:
        url = f"{METASEARCH_URL}/api/v1/discover_snapshot?lang={lang}&limit={limit}"
        async with session.get(url, timeout=30) as resp:
            if resp.status == 200:
                data = await resp.json()
                return data.get("articles", [])
    return []


async def generate_script(headlines: list, lang: str = "ko") -> str:
    """Generate script using LLM"""
    headlines_text = "\n".join(f"- {h['title']}" for h in headlines[:8])
    prompt = SCRIPT_PROMPT_KO if lang == "ko" else SCRIPT_PROMPT_EN
    prompt = prompt.format(headlines=headlines_text)

    async with aiohttp.ClientSession() as session:
        payload = {
            "model": MODEL,
            "prompt": prompt,
            "stream": False,
            "options": {"temperature": 0.7, "num_predict": 500}
        }
        try:
            async with session.post(f"{OLLAMA_URL}/api/generate", json=payload, timeout=60) as resp:
                if resp.status == 200:
                    data = await resp.json()
                    return data.get("response", "").strip()
        except Exception as e:
            print(f"[LLM Error] {e}")

    # Fallback: simple concatenation
    return "\n".join(h['title'] for h in headlines[:5])


async def generate_audio(text: str, lang: str = "ko", output_path: Path = None) -> Path:
    """Generate audio using Edge TTS"""
    voice = VOICE_KO if lang == "ko" else VOICE_EN

    if output_path is None:
        hash_id = hashlib.md5(text.encode()).hexdigest()[:8]
        output_path = OUTPUT_DIR / f"briefing_{lang}_{hash_id}.mp3"

    communicate = edge_tts.Communicate(text, voice)
    await communicate.save(str(output_path))
    return output_path


async def create_briefing(lang: str = "ko") -> dict:
    """Create a full news briefing"""
    print(f"[Briefing] Starting for lang={lang}")

    # 1. Fetch news
    articles = await fetch_news(lang, limit=10)
    if not articles:
        return {"error": "No articles found"}
    print(f"[Briefing] Fetched {len(articles)} articles")

    # 2. Generate script
    script = await generate_script(articles, lang)
    print(f"[Briefing] Generated script: {len(script)} chars")

    # 3. Generate audio
    timestamp = datetime.now().strftime("%Y%m%d_%H%M")
    audio_path = OUTPUT_DIR / f"briefing_{lang}_{timestamp}.mp3"
    await generate_audio(script, lang, audio_path)
    print(f"[Briefing] Audio saved: {audio_path}")

    # 4. Save metadata
    result = {
        "lang": lang,
        "created_at": datetime.now().isoformat(),
        "audio_file": str(audio_path),
        "audio_url": f"/audio/{audio_path.name}",
        "script": script,
        "local_headlines": [a["title"] for a in articles[:5]],
        "world_headlines": [a["title"] for a in articles if a.get("category") == "world"][:5],
        "article_count": len(articles)
    }

    meta_path = audio_path.with_suffix(".json")
    with open(meta_path, "w", encoding="utf-8") as f:
        json.dump(result, f, ensure_ascii=False, indent=2)

    return result


async def main():
    import sys
    lang = sys.argv[1] if len(sys.argv) > 1 else "ko"
    result = await create_briefing(lang)
    print(json.dumps(result, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    asyncio.run(main())
