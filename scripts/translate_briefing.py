#!/usr/bin/env python3
"""Translate briefing anchor lines to all supported languages using GPT-4o-mini."""

import json
import os
from pathlib import Path
from openai import OpenAI

client = OpenAI()

INTRO_EN = "Hello, I'm Hera Kim. Here are today's top stories."
OUTRO_EN = "That's all for now. I'm Hera Kim. Have a great day."

LANG_DIR = Path(__file__).parent.parent / "static" / "lang"

# Some languages we already have good translations for
KNOWN_TRANSLATIONS = {
    "ko": {
        "intro": "안녕하세요, 헤라 킴입니다. 오늘의 주요 뉴스를 전해드립니다.",
        "outro": "이상 헤라 킴이었습니다. 좋은 하루 되세요."
    },
    "ja": {
        "intro": "こんにちは、ヘラ・キムです。本日の主要ニュースをお伝えします。",
        "outro": "以上、ヘラ・キムがお伝えしました。良い一日を。"
    },
    "zh": {
        "intro": "大家好，我是赫拉·金。以下是今日要闻。",
        "outro": "以上就是今天的新闻。我是赫拉·金，祝您愉快。"
    }
}

LANG_NAMES = {
    "af": "Afrikaans", "ar": "Arabic", "bg": "Bulgarian", "bn": "Bengali",
    "ca": "Catalan", "cs": "Czech", "da": "Danish", "de": "German",
    "el": "Greek", "en": "English", "es": "Spanish", "et": "Estonian",
    "eu": "Basque", "fa": "Persian", "fi": "Finnish", "fr": "French",
    "gl": "Galician", "he": "Hebrew", "hi": "Hindi", "hr": "Croatian",
    "hu": "Hungarian", "hy": "Armenian", "id": "Indonesian", "is": "Icelandic",
    "it": "Italian", "ja": "Japanese", "ka": "Georgian", "kk": "Kazakh",
    "km": "Khmer", "ko": "Korean", "lt": "Lithuanian", "lv": "Latvian",
    "mk": "Macedonian", "ml": "Malayalam", "mn": "Mongolian", "mr": "Marathi",
    "ms": "Malay", "my": "Burmese", "ne": "Nepali", "nl": "Dutch",
    "no": "Norwegian", "pa": "Punjabi", "pl": "Polish", "pt": "Portuguese",
    "ro": "Romanian", "ru": "Russian", "si": "Sinhala", "sk": "Slovak",
    "sl": "Slovenian", "sq": "Albanian", "sr": "Serbian", "sv": "Swedish",
    "sw": "Swahili", "ta": "Tamil", "te": "Telugu", "th": "Thai",
    "tl": "Tagalog", "tr": "Turkish", "uk": "Ukrainian", "ur": "Urdu",
    "uz": "Uzbek", "vi": "Vietnamese", "zh": "Chinese"
}

def translate(text: str, target_lang: str) -> str:
    """Translate text using GPT-4o-mini."""
    lang_name = LANG_NAMES.get(target_lang, target_lang)

    response = client.chat.completions.create(
        model="gpt-4o-mini",
        messages=[
            {"role": "system", "content": f"You are a professional translator. Translate the following news anchor script to {lang_name}. Keep the name 'Hera Kim' as is (or transliterate it naturally for the target language). Keep the tone professional but friendly, like a real news anchor. Return ONLY the translation, no explanations."},
            {"role": "user", "content": text}
        ],
        temperature=0.3,
        max_tokens=200
    )
    return response.choices[0].message.content.strip()

def main():
    lang_files = sorted(LANG_DIR.glob("*.json"))

    for lang_file in lang_files:
        lang_code = lang_file.stem
        print(f"Processing {lang_code}...")

        # Load existing language pack
        with open(lang_file, "r", encoding="utf-8") as f:
            data = json.load(f)

        # Skip if briefing section already exists with both keys
        if "briefing" in data and "anchor_intro" in data.get("briefing", {}) and "anchor_outro" in data.get("briefing", {}):
            print(f"  Skipping {lang_code} - already has briefing translations")
            continue

        # Use known translations or translate
        if lang_code in KNOWN_TRANSLATIONS:
            intro = KNOWN_TRANSLATIONS[lang_code]["intro"]
            outro = KNOWN_TRANSLATIONS[lang_code]["outro"]
        elif lang_code == "en":
            intro = INTRO_EN
            outro = OUTRO_EN
        else:
            print(f"  Translating to {LANG_NAMES.get(lang_code, lang_code)}...")
            intro = translate(INTRO_EN, lang_code)
            outro = translate(OUTRO_EN, lang_code)

        # Add to briefing section
        if "briefing" not in data:
            data["briefing"] = {}
        data["briefing"]["anchor_intro"] = intro
        data["briefing"]["anchor_outro"] = outro

        # Save
        with open(lang_file, "w", encoding="utf-8") as f:
            json.dump(data, f, ensure_ascii=False, indent=2)

        print(f"  ✓ {lang_code}: {intro[:50]}...")

if __name__ == "__main__":
    main()
