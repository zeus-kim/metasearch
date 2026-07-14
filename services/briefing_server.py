#!/usr/bin/env python3
"""
News Briefing Server
- Generates briefings for multiple languages
- Serves audio via HTTP API
- Auto-refreshes every hour
"""

import asyncio
import json
import hashlib
import os
import time
from pathlib import Path
from datetime import datetime, timedelta
from aiohttp import web
import aiohttp
import edge_tts

# Config (can be overridden via environment variables)
HOST = os.environ.get("BRIEFING_HOST", "0.0.0.0")
PORT = int(os.environ.get("BRIEFING_PORT", "8893"))
METASEARCH_URL = os.environ.get("METASEARCH_URL", "http://localhost:8889")
OLLAMA_URL = os.environ.get("OLLAMA_URL", "http://localhost:11434")
MODEL = os.environ.get("OLLAMA_MODEL", "gemma3:4b")
OUTPUT_DIR = Path(__file__).parent.parent / "data" / "briefings"
OUTPUT_DIR.mkdir(parents=True, exist_ok=True)

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
    "ko": """라디오 뉴스 브리핑을 작성하세요.

형식:
1. 시작: "{greeting}, 주요 뉴스입니다."
2. 국내 뉴스: "먼저 국내 뉴스입니다." 로 시작, 각 뉴스 1-2문장
3. 해외 뉴스: "다음은 해외 뉴스입니다." 로 시작, 각 뉴스 1-2문장
4. 끝: "지금까지 뉴스 브리핑이었습니다. 청취해 주셔서 감사합니다."

규칙: 마크다운 사용 금지, 300-400자

헤드라인:
{headlines}

브리핑:""",

    "en": """Write a voice-ready news briefing based on these headlines.

Rules:
- NO markdown, asterisks, or parenthetical directions
- Write natural sentences ready to be read aloud
- Start with greeting, end with closing
- 150-200 words

Headlines:
{headlines}

Briefing:""",

    "ja": """次のニュースヘッドラインを元に、ニュースブリーフィングを日本語で書いてください。マークダウンや記号は使わず、そのまま読める自然な文章で書いてください。

ヘッドライン:
{headlines}

ブリーフィング:""",

    "zh": """根据以下标题撰写语音播报新闻简报。

规则:
- 禁止使用markdown、星号或括号指示
- 只写可以直接朗读的自然句子
- 以问候开始，以结束语结尾
- 200-300字

标题:
{headlines}

简报:""",
}

# Cache for briefings
briefing_cache = {}
generation_lock = asyncio.Lock()


def is_valid_headline(title: str) -> bool:
    """Filter out garbage headlines"""
    if not title or len(title) < 10:
        return False
    if len(title) > 200:
        return False
    # Single word or very short
    if len(title.split()) < 3 and not any(ord(c) > 127 for c in title):
        return False
    # Common garbage patterns
    garbage = ["implementation", "undefined", "null", "error", "loading", "untitled"]
    if title.lower().strip() in garbage:
        return False
    # Looks like a fragment (no verb indicators for English)
    if title.islower() and len(title.split()) < 5:
        return False
    return True


def deduplicate_headlines(articles: list) -> list:
    """Remove similar headlines"""
    seen_keywords = set()
    result = []

    for a in articles:
        title = a.get("title", "")
        # Extract key terms (first 3 significant words)
        words = [w for w in title.split()[:6] if len(w) > 2]
        key = " ".join(words[:3]).lower()

        # Check for similarity
        is_dup = False
        for seen in seen_keywords:
            # Simple overlap check
            seen_words = set(seen.split())
            key_words = set(key.split())
            if len(seen_words & key_words) >= 2:
                is_dup = True
                break

        if not is_dup:
            seen_keywords.add(key)
            result.append(a)

    return result


def limit_per_source(articles: list, max_per_source: int = 2) -> list:
    """Limit articles from same source"""
    source_count = {}
    result = []

    for a in articles:
        source = a.get("source", "unknown")
        if source_count.get(source, 0) < max_per_source:
            result.append(a)
            source_count[source] = source_count.get(source, 0) + 1

    return result


def balance_categories(articles: list, limit: int) -> list:
    """Balance articles across categories"""
    # Priority categories for hard news
    priority_cats = ["politics", "economy", "business", "national", "world", "international", "society"]
    secondary_cats = ["science", "tech", "health", "culture"]
    low_priority = ["sports", "entertainment", "lifestyle"]

    # Group by category
    by_cat = {}
    for a in articles:
        cat = (a.get("category") or "news").lower()
        if cat not in by_cat:
            by_cat[cat] = []
        by_cat[cat].append(a)

    result = []
    # First pass: pick from priority categories
    for cat in priority_cats:
        if cat in by_cat and len(result) < limit:
            result.append(by_cat[cat].pop(0))

    # Second pass: pick from secondary
    for cat in secondary_cats:
        if cat in by_cat and len(result) < limit:
            result.append(by_cat[cat].pop(0))

    # Fill remaining from any category except low priority
    for cat, items in by_cat.items():
        if cat not in low_priority:
            while items and len(result) < limit:
                result.append(items.pop(0))

    # If still not enough, use low priority
    for cat in low_priority:
        if cat in by_cat:
            while by_cat[cat] and len(result) < limit:
                result.append(by_cat[cat].pop(0))

    return result


async def fetch_news(lang: str, limit: int = 10) -> list:
    """Fetch news from metasearch with filtering and balancing"""
    async with aiohttp.ClientSession() as session:
        url = f"{METASEARCH_URL}/api/v1/discover_snapshot?lang={lang}&limit={limit * 5}"
        try:
            async with session.get(url, timeout=30) as resp:
                if resp.status == 200:
                    data = await resp.json()
                    articles = data.get("articles", [])
                    # 1. Filter valid headlines
                    articles = [a for a in articles if is_valid_headline(a.get("title", ""))]
                    # 2. Limit per source (diversity)
                    articles = limit_per_source(articles, max_per_source=2)
                    # 3. Remove duplicates
                    articles = deduplicate_headlines(articles)
                    # 4. Balance categories
                    articles = balance_categories(articles, limit)
                    return articles
        except Exception as e:
            print(f"[fetch_news] Error: {e}")
    return []


import re

def clean_script(script: str) -> str:
    """Remove stage directions and formatting from script"""
    # Remove parenthetical stage directions
    script = re.sub(r'\([^)]*(?:톤|tone|sound|music|읽어|fade|theme|anchor|swells|차분|명료)[^)]*\)', '', script, flags=re.IGNORECASE)
    # Remove square bracket directives
    script = re.sub(r'\[[^\]]*(?:스크립트|script|시작|끝|start|end|intro|outro)[^\]]*\]', '', script, flags=re.IGNORECASE)
    # Remove all asterisks (bold/italic markers)
    script = re.sub(r'\*+', '', script)
    # Remove hash headers
    script = re.sub(r'^#{1,6}\s*', '', script, flags=re.MULTILINE)
    # Remove dividers
    script = re.sub(r'^[-=_]{3,}$', '', script, flags=re.MULTILINE)
    # Remove role markers
    script = re.sub(r'^(Anchor|앵커|アンカー|主播|Host|진행자):\s*', '', script, flags=re.MULTILINE)
    # Remove empty parentheses/brackets that might remain
    script = re.sub(r'\(\s*\)', '', script)
    script = re.sub(r'\[\s*\]', '', script)
    # Clean up extra whitespace
    script = re.sub(r'\n{3,}', '\n\n', script)
    script = re.sub(r'^\s+', '', script)
    script = re.sub(r'\s+$', '', script, flags=re.MULTILINE)
    return script.strip()


FALLBACK_MODEL = os.environ.get("OLLAMA_FALLBACK_MODEL", "llama3.2:3b")

def get_time_greeting(lang: str) -> str:
    """Get time-appropriate greeting"""
    hour = datetime.now().hour
    if lang == "ko":
        if 5 <= hour < 12:
            return "좋은 아침입니다"
        elif 12 <= hour < 18:
            return "안녕하십니까"
        else:
            return "안녕하십니까"
    elif lang == "ja":
        if 5 <= hour < 12:
            return "おはようございます"
        elif 12 <= hour < 18:
            return "こんにちは"
        else:
            return "こんばんは"
    elif lang == "zh":
        if 5 <= hour < 12:
            return "早上好"
        elif 12 <= hour < 18:
            return "下午好"
        else:
            return "晚上好"
    else:
        if 5 <= hour < 12:
            return "Good morning"
        elif 12 <= hour < 18:
            return "Good afternoon"
        else:
            return "Good evening"


async def generate_script(headlines: list, lang: str, world_headlines: list = None) -> str:
    """Generate script using LLM"""
    greeting = get_time_greeting(lang)

    if world_headlines and lang in ["ko", "ja", "zh"]:
        # Separate local and world news for Asian languages
        local_text = "\n".join(f"- {h['title']}" for h in headlines[:6])
        world_text = "\n".join(f"- {h['title']}" for h in world_headlines[:4])
        headlines_text = f"[국내/로컬]\n{local_text}\n\n[해외/월드]\n{world_text}"
    else:
        headlines_text = "\n".join(f"- {h['title']}" for h in headlines[:8])

    prompt_template = PROMPTS.get(lang, PROMPTS["en"])
    try:
        prompt = prompt_template.format(headlines=headlines_text, greeting=greeting)
    except KeyError:
        prompt = prompt_template.format(headlines=headlines_text)

    timeout = aiohttp.ClientTimeout(total=120)
    async with aiohttp.ClientSession(timeout=timeout) as session:
        # Try primary model first
        for model in [MODEL, FALLBACK_MODEL]:
            payload = {
                "model": model,
                "prompt": prompt,
                "stream": False,
                "options": {"temperature": 0.7, "num_predict": 600}
            }
            try:
                async with session.post(f"{OLLAMA_URL}/api/generate", json=payload) as resp:
                    if resp.status == 200:
                        data = await resp.json()
                        script = data.get("response", "").strip()
                        script = clean_script(script)
                        if len(script) > 50:  # Valid script
                            print(f"[generate_script] Success with {model}: {len(script)} chars")
                            return script
                        print(f"[generate_script] {model} returned short response, trying fallback")
            except Exception as e:
                print(f"[generate_script] Error with {model}: {e}")

    # Fallback to headlines
    return "\n".join(h['title'] for h in headlines[:5])


async def generate_audio(text: str, lang: str) -> Path:
    """Generate audio using Edge TTS"""
    voice = VOICES.get(lang, VOICES["en"])
    timestamp = datetime.now().strftime("%Y%m%d_%H%M")
    output_path = OUTPUT_DIR / f"briefing_{lang}_{timestamp}.mp3"

    try:
        communicate = edge_tts.Communicate(text, voice)
        await communicate.save(str(output_path))
        return output_path
    except Exception as e:
        print(f"[generate_audio] Error: {e}")
        return None


async def create_briefing(lang: str) -> dict:
    """Create a news briefing for a language"""
    print(f"[create_briefing] Starting for {lang}")

    # Fetch local news
    articles = await fetch_news(lang, limit=10)
    if not articles:
        return {"error": f"No articles for {lang}"}

    # Fetch world news from English sources (if not already English)
    world_articles = []
    if lang != "en":
        world_articles = await fetch_news("en", limit=5)

    # Generate script with local and world news separately labeled
    script = await generate_script(articles[:6], lang, world_headlines=world_articles)
    if not script:
        return {"error": "Script generation failed"}

    audio_path = await generate_audio(script, lang)
    if not audio_path:
        return {"error": "Audio generation failed"}

    result = {
        "lang": lang,
        "created_at": datetime.now().isoformat(),
        "audio_url": f"/audio/{audio_path.name}",
        "script": script,
        "local_headlines": [a["title"] for a in articles[:10]],
        "world_headlines": [a["title"] for a in world_articles[:5]],
        "headlines": [a["title"] for a in articles[:10]],
        "article_count": len(articles),
        "_audio_path": str(audio_path),
    }

    # Cache it
    briefing_cache[lang] = {
        "data": result,
        "ts": time.time(),
    }

    # Save metadata
    meta_path = audio_path.with_suffix(".json")
    with open(meta_path, "w", encoding="utf-8") as f:
        json.dump(result, f, ensure_ascii=False, indent=2)

    print(f"[create_briefing] Done for {lang}: {audio_path.name}")
    return result


async def get_or_create_briefing(lang: str, max_age: int = 3600) -> dict:
    """Get cached briefing or create new one"""
    cached = briefing_cache.get(lang)
    if cached and time.time() - cached["ts"] < max_age:
        return cached["data"]

    async with generation_lock:
        # Double check after acquiring lock
        cached = briefing_cache.get(lang)
        if cached and time.time() - cached["ts"] < max_age:
            return cached["data"]
        return await create_briefing(lang)


# HTTP Handlers
async def handle_briefing(request):
    """GET /briefing?lang=ko&refresh=1"""
    lang = request.query.get("lang", "ko")
    if lang not in VOICES:
        lang = "en"

    refresh = request.query.get("refresh", "0") == "1"
    if refresh and lang in briefing_cache:
        del briefing_cache[lang]

    try:
        result = await get_or_create_briefing(lang)
        # Remove internal fields
        result = {k: v for k, v in result.items() if not k.startswith("_")}
        return web.json_response(result)
    except Exception as e:
        return web.json_response({"error": str(e)}, status=500)


async def handle_audio(request):
    """GET /audio/{filename}"""
    filename = request.match_info.get("filename", "")
    audio_path = OUTPUT_DIR / filename

    if not audio_path.exists():
        return web.Response(status=404, text="Not found")

    return web.FileResponse(audio_path, headers={
        "Content-Type": "audio/mpeg",
        "Cache-Control": "public, max-age=3600",
    })


async def handle_health(request):
    """GET /health"""
    return web.json_response({
        "status": "ok",
        "cached_langs": list(briefing_cache.keys()),
        "voices": list(VOICES.keys()),
    })


async def background_generator():
    """Background task to pre-generate briefings"""
    priority_langs = ["ko", "en", "ja", "zh"]

    while True:
        for lang in priority_langs:
            try:
                await get_or_create_briefing(lang, max_age=1800)  # 30 min cache for priority
            except Exception as e:
                print(f"[background] Error for {lang}: {e}")
            await asyncio.sleep(10)

        await asyncio.sleep(1800)  # Run every 30 minutes


async def on_startup(app):
    """Start background generator"""
    app["bg_task"] = asyncio.create_task(background_generator())


async def on_cleanup(app):
    """Stop background generator"""
    app["bg_task"].cancel()
    try:
        await app["bg_task"]
    except asyncio.CancelledError:
        pass


def main():
    app = web.Application()
    app.router.add_get("/briefing", handle_briefing)
    app.router.add_get("/audio/{filename}", handle_audio)
    app.router.add_get("/health", handle_health)

    app.on_startup.append(on_startup)
    app.on_cleanup.append(on_cleanup)

    print(f"[briefing_server] Starting on {HOST}:{PORT}")
    print(f"[briefing_server] Supported languages: {list(VOICES.keys())}")
    web.run_app(app, host=HOST, port=PORT)


if __name__ == "__main__":
    main()
