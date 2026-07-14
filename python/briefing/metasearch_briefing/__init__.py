"""Metasearch Briefing - AI-generated audio news briefings."""

import asyncio
import hashlib
import os
from pathlib import Path
from datetime import datetime
from typing import Optional, List, Dict, Any
from dataclasses import dataclass

import aiohttp
import edge_tts

__version__ = "0.1.0"

# Voice mapping by language
VOICES = {
    "ko": "ko-KR-SunHiNeural",
    "en": "en-US-JennyNeural",
    "ja": "ja-JP-NanamiNeural",
    "zh": "zh-CN-XiaoxiaoNeural",
    "es": "es-ES-ElviraNeural",
    "fr": "fr-FR-DeniseNeural",
    "de": "de-DE-KatjaNeural",
    "pt": "pt-BR-FranciscaNeural",
    "ru": "ru-RU-SvetlanaNeural",
    "ar": "ar-SA-ZariyahNeural",
}

PROMPTS = {
    "ko": """라디오 뉴스 브리핑 스크립트를 작성하세요.
시작: 인사말, 국내 뉴스, 해외 뉴스, 마무리 인사
규칙: 마크다운 금지, 자연스러운 문장, 300-400자

헤드라인:
{headlines}

브리핑:""",

    "en": """Write a voice-ready news briefing.
Rules: NO markdown, natural sentences, 150-200 words

Headlines:
{headlines}

Briefing:""",

    "ja": """ニュースブリーフィングを書いてください。マークダウン禁止、自然な文章で。

ヘッドライン:
{headlines}

ブリーフィング:""",

    "zh": """撰写语音新闻简报。禁止markdown，自然句子，200-300字。

标题:
{headlines}

简报:""",
}


@dataclass
class Briefing:
    lang: str
    script: str
    audio_path: Optional[str]
    headlines: List[str]
    created_at: str


class BriefingGenerator:
    """Generate audio news briefings."""

    def __init__(
        self,
        metasearch_url: str = "http://localhost:8889",
        ollama_url: str = "http://localhost:11434",
        model: str = "gemma3:4b",
        output_dir: str = "data/briefings",
    ):
        self.metasearch_url = metasearch_url.rstrip("/")
        self.ollama_url = ollama_url.rstrip("/")
        self.model = model
        self.output_dir = Path(output_dir)
        self.output_dir.mkdir(parents=True, exist_ok=True)

    async def fetch_news(self, lang: str, limit: int = 15) -> List[Dict[str, Any]]:
        """Fetch news from metasearch."""
        async with aiohttp.ClientSession() as session:
            url = f"{self.metasearch_url}/api/v1/discover_snapshot"
            params = {"lang": lang, "limit": limit}
            async with session.get(url, params=params, timeout=30) as resp:
                if resp.status == 200:
                    data = await resp.json()
                    return data.get("articles", [])
        return []

    async def generate_script(self, headlines: List[str], lang: str) -> str:
        """Generate briefing script using LLM."""
        prompt_template = PROMPTS.get(lang, PROMPTS["en"])
        headlines_text = "\n".join(f"- {h}" for h in headlines[:10])
        prompt = prompt_template.format(headlines=headlines_text)

        async with aiohttp.ClientSession() as session:
            payload = {
                "model": self.model,
                "prompt": prompt,
                "stream": False,
                "options": {"temperature": 0.7, "num_predict": 500},
            }
            try:
                async with session.post(
                    f"{self.ollama_url}/api/generate",
                    json=payload,
                    timeout=60,
                ) as resp:
                    if resp.status == 200:
                        data = await resp.json()
                        return self._clean_script(data.get("response", ""))
            except Exception as e:
                print(f"[Briefing] LLM error: {e}")

        return "\n".join(headlines[:5])

    def _clean_script(self, text: str) -> str:
        """Remove markdown and stage directions."""
        import re
        text = re.sub(r"\*+", "", text)
        text = re.sub(r"#+\s*", "", text)
        text = re.sub(r"\([^)]*\)", "", text)
        text = re.sub(r"\[[^\]]*\]", "", text)
        return text.strip()

    async def generate_audio(self, text: str, lang: str) -> str:
        """Generate audio using Edge TTS."""
        voice = VOICES.get(lang, VOICES["en"])
        hash_id = hashlib.md5(text.encode()).hexdigest()[:8]
        timestamp = datetime.now().strftime("%Y%m%d_%H%M")
        output_path = self.output_dir / f"briefing_{lang}_{timestamp}_{hash_id}.mp3"

        communicate = edge_tts.Communicate(text, voice)
        await communicate.save(str(output_path))
        return str(output_path)

    async def create(self, lang: str = "en") -> Briefing:
        """Create a full news briefing."""
        print(f"[Briefing] Creating for {lang}...")

        articles = await self.fetch_news(lang)
        if not articles:
            raise ValueError(f"No articles found for {lang}")

        headlines = [a.get("title", "") for a in articles if a.get("title")]
        print(f"[Briefing] {len(headlines)} headlines")

        script = await self.generate_script(headlines, lang)
        print(f"[Briefing] Script: {len(script)} chars")

        audio_path = await self.generate_audio(script, lang)
        print(f"[Briefing] Audio: {audio_path}")

        return Briefing(
            lang=lang,
            script=script,
            audio_path=audio_path,
            headlines=headlines[:10],
            created_at=datetime.now().isoformat(),
        )


async def main():
    """CLI entry point."""
    import sys
    lang = sys.argv[1] if len(sys.argv) > 1 else "en"

    generator = BriefingGenerator()
    briefing = await generator.create(lang)

    print(f"\n=== Briefing ({briefing.lang}) ===")
    print(briefing.script)
    print(f"\nAudio: {briefing.audio_path}")


if __name__ == "__main__":
    asyncio.run(main())
