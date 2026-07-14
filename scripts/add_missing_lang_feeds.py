#!/usr/bin/env python3
"""Find and add news feeds for missing languages."""

import json
import requests
from concurrent.futures import ThreadPoolExecutor, as_completed

# Missing languages with known major news sources
# Format: (lang, url, source, country)
FEEDS_TO_ADD = [
    # af - Afrikaans (South Africa)
    ("af", "https://www.netwerk24.com/rss/artikels", "Netwerk24", "ZA"),
    ("af", "https://www.news24.com/news24/southafrica/rss", "News24 SA", "ZA"),

    # bg - Bulgarian
    ("bg", "https://www.dnevnik.bg/rss/", "Dnevnik", "BG"),
    ("bg", "https://www.mediapool.bg/rss.xml", "Mediapool", "BG"),
    ("bg", "https://nova.bg/rss", "Nova TV", "BG"),

    # ca - Catalan
    ("ca", "https://www.vilaweb.cat/feed/", "VilaWeb", "ES"),
    ("ca", "https://www.naciodigital.cat/rss", "NacióDigital", "ES"),
    ("ca", "https://www.ara.cat/rss/", "Ara", "ES"),

    # et - Estonian
    ("et", "https://www.postimees.ee/rss/", "Postimees", "EE"),
    ("et", "https://www.delfi.ee/rss", "Delfi Estonia", "EE"),
    ("et", "https://www.err.ee/rss", "ERR", "EE"),

    # eu - Basque
    ("eu", "https://www.naiz.eus/rss", "Naiz", "ES"),
    ("eu", "https://www.berria.eus/rss", "Berria", "ES"),

    # gl - Galician
    ("gl", "https://www.galiciaconfidencial.com/rss", "Galicia Confidencial", "ES"),
    ("gl", "https://www.lavozdegalicia.es/rss/galicia.xml", "La Voz de Galicia", "ES"),

    # hr - Croatian
    ("hr", "https://www.index.hr/rss", "Index.hr", "HR"),
    ("hr", "https://www.jutarnji.hr/rss", "Jutarnji list", "HR"),
    ("hr", "https://www.vecernji.hr/rss", "Vecernji list", "HR"),

    # hy - Armenian
    ("hy", "https://armenpress.am/eng/rss/", "Armenpress", "AM"),
    ("hy", "https://www.azatutyun.am/api/", "Radio Free Europe/RL Armenian", "AM"),

    # is - Icelandic
    ("is", "https://www.mbl.is/rss/", "Morgunbladid", "IS"),
    ("is", "https://www.visir.is/rss", "Visir", "IS"),
    ("is", "https://www.ruv.is/rss/", "RUV", "IS"),

    # it - Italian
    ("it", "https://www.ansa.it/sito/ansait_rss.xml", "ANSA", "IT"),
    ("it", "https://www.repubblica.it/rss/homepage/rss2.0.xml", "La Repubblica", "IT"),
    ("it", "https://www.corriere.it/rss/homepage.xml", "Corriere della Sera", "IT"),
    ("it", "https://www.ilsole24ore.com/rss/italia.xml", "Il Sole 24 Ore", "IT"),

    # ka - Georgian
    ("ka", "https://civil.ge/rss", "Civil.ge", "GE"),
    ("ka", "https://www.radiotavisupleba.ge/api/", "Radio Free Europe/RL Georgian", "GE"),

    # kk - Kazakh
    ("kk", "https://www.azattyq.org/api/", "Radio Free Europe/RL Kazakh", "KZ"),
    ("kk", "https://tengrinews.kz/rss/", "Tengrinews", "KZ"),

    # km - Khmer (Cambodia)
    ("km", "https://www.rfakkhmer.org/api/", "RFA Khmer", "KH"),
    ("km", "https://www.phnompenhpost.com/rss.xml", "Phnom Penh Post", "KH"),

    # lt - Lithuanian
    ("lt", "https://www.delfi.lt/rss", "Delfi Lithuania", "LT"),
    ("lt", "https://www.lrt.lt/rss", "LRT", "LT"),
    ("lt", "https://www.15min.lt/rss", "15min", "LT"),

    # lv - Latvian
    ("lv", "https://www.delfi.lv/rss", "Delfi Latvia", "LV"),
    ("lv", "https://www.tvnet.lv/rss", "TVNET", "LV"),
    ("lv", "https://www.lsm.lv/rss", "LSM", "LV"),

    # mk - Macedonian
    ("mk", "https://www.slobodnaevropa.mk/api/", "Radio Free Europe/RL Macedonian", "MK"),
    ("mk", "https://meta.mk/rss/", "Meta.mk", "MK"),

    # ml - Malayalam (India)
    ("ml", "https://www.manoramaonline.com/rss/news.xml", "Malayala Manorama", "IN"),
    ("ml", "https://www.mathrubhumi.com/rss/", "Mathrubhumi", "IN"),

    # mn - Mongolian
    ("mn", "https://www.mongolnews.mn/rss", "Mongol News", "MN"),
    ("mn", "https://www.news.mn/rss/", "News.mn", "MN"),

    # my - Burmese (Myanmar)
    ("my", "https://www.rfa.org/burmese/rss2.xml", "RFA Burmese", "MM"),
    ("my", "https://burmese.voanews.com/api/", "VOA Burmese", "MM"),

    # ne - Nepali
    ("ne", "https://www.onlinekhabar.com/rss", "Online Khabar", "NP"),
    ("ne", "https://www.setopati.com/rss", "Setopati", "NP"),

    # pa - Punjabi
    ("pa", "https://www.bbc.com/punjabi/topics/c7zp5709w5nt.rss", "BBC Punjabi", "IN"),
    ("pa", "https://punjabi.jagran.com/rss/punjab-news.xml", "Jagran Punjabi", "IN"),

    # si - Sinhala (Sri Lanka)
    ("si", "https://www.adaderana.lk/rss.php", "Ada Derana", "LK"),
    ("si", "https://www.lankadeepa.lk/rss", "Lanka Deepa", "LK"),

    # sk - Slovak
    ("sk", "https://www.sme.sk/rss", "SME", "SK"),
    ("sk", "https://www.aktuality.sk/rss", "Aktuality", "SK"),
    ("sk", "https://dennikn.sk/rss/", "Dennik N", "SK"),

    # sl - Slovenian
    ("sl", "https://www.rtvslo.si/rss", "RTV Slovenija", "SI"),
    ("sl", "https://www.24ur.com/rss", "24ur", "SI"),
    ("sl", "https://www.delo.si/rss/", "Delo", "SI"),

    # sq - Albanian
    ("sq", "https://www.top-channel.tv/rss/", "Top Channel", "AL"),
    ("sq", "https://shqiptarja.com/rss", "Shqiptarja", "AL"),

    # sr - Serbian
    ("sr", "https://www.rts.rs/rss/", "RTS", "RS"),
    ("sr", "https://www.blic.rs/rss/", "Blic", "RS"),
    ("sr", "https://www.b92.net/rss/", "B92", "RS"),

    # ur - Urdu
    ("ur", "https://www.bbc.com/urdu/topics/c7zp57r92wlt.rss", "BBC Urdu", "PK"),
    ("ur", "https://urdu.voanews.com/api/", "VOA Urdu", "PK"),
    ("ur", "https://www.dawn.com/feeds/urdu", "Dawn Urdu", "PK"),

    # uz - Uzbek
    ("uz", "https://www.ozodlik.org/api/", "Radio Free Europe/RL Uzbek", "UZ"),
    ("uz", "https://kun.uz/rss/", "Kun.uz", "UZ"),
]

def check_feed(feed_info):
    """Check if a feed URL is accessible and returns valid RSS/Atom."""
    lang, url, source, country = feed_info
    try:
        headers = {
            'User-Agent': 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36'
        }
        resp = requests.get(url, headers=headers, timeout=10, allow_redirects=True)
        if resp.status_code == 200:
            content = resp.text[:500].lower()
            if '<rss' in content or '<feed' in content or '<channel' in content or '<?xml' in content:
                return (lang, url, source, country, True, "OK")
            else:
                return (lang, url, source, country, False, "Not RSS/Atom")
        else:
            return (lang, url, source, country, False, f"HTTP {resp.status_code}")
    except Exception as e:
        return (lang, url, source, country, False, str(e)[:50])

def main():
    print("Checking feeds...")
    valid_feeds = []

    with ThreadPoolExecutor(max_workers=10) as executor:
        futures = {executor.submit(check_feed, feed): feed for feed in FEEDS_TO_ADD}
        for future in as_completed(futures):
            result = future.result()
            lang, url, source, country, ok, msg = result
            status = "✓" if ok else "✗"
            print(f"{status} [{lang}] {source}: {msg}")
            if ok:
                valid_feeds.append({
                    "lang": lang,
                    "url": url,
                    "category": "news",
                    "country": country,
                    "source": source,
                    "tier": 1
                })

    print(f"\n{len(valid_feeds)} valid feeds found")

    # Append to major_news_feeds.jsonl
    if valid_feeds:
        with open('/Users/dragon/Projects/metasearch/static/major_news_feeds.jsonl', 'a') as f:
            # Add section headers by language
            current_lang = None
            for feed in sorted(valid_feeds, key=lambda x: x['lang']):
                if feed['lang'] != current_lang:
                    current_lang = feed['lang']
                    f.write(f"\n# === {current_lang.upper()} ===\n")
                f.write(json.dumps(feed, ensure_ascii=False) + "\n")
        print(f"Added {len(valid_feeds)} feeds to major_news_feeds.jsonl")

if __name__ == "__main__":
    main()
