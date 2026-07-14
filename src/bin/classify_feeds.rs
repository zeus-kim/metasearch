//! Feed classification CLI tool
//!
//! Classifies all feeds in registry with language and category.
//! Usage: cargo run --release --bin classify_feeds

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
struct Feed {
    #[serde(default)]
    lang: String,
    url: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    country: String,
    #[serde(rename = "type", default)]
    feed_type: String,
    #[serde(default)]
    tier: u8,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let input = args.get(1).map(|s| s.as_str()).unwrap_or("static/feeds_registry.jsonl");
    let output = args.get(2).map(|s| s.as_str()).unwrap_or("static/feeds_registry_classified.jsonl");

    println!("Feed Classification Tool");
    println!("========================");
    println!("Input:  {}", input);
    println!("Output: {}", output);

    let url_patterns = load_url_patterns();
    let domain_rules = load_domain_rules();
    let lang_patterns = load_lang_patterns();

    let file = File::open(input).expect("Failed to open input file");
    let reader = BufReader::new(file);

    let out_file = File::create(output).expect("Failed to create output file");
    let mut writer = BufWriter::new(out_file);

    let mut total = 0;
    let mut classified_lang = 0;
    let mut classified_cat = 0;
    let mut lang_stats: HashMap<String, usize> = HashMap::new();
    let mut cat_stats: HashMap<String, usize> = HashMap::new();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };

        if line.trim().is_empty() {
            continue;
        }

        let mut feed: Feed = match serde_json::from_str(&line) {
            Ok(f) => f,
            Err(_) => continue,
        };

        total += 1;

        // Classify language - always check if we can find a more specific language
        if let Some(lang) = detect_language(&feed.url, &lang_patterns) {
            // Override if: empty, unknown, or current is "en" but URL indicates different
            if feed.lang.is_empty() || feed.lang == "unknown" || (feed.lang == "en" && lang != "en") {
                feed.lang = lang;
                classified_lang += 1;
            }
        }
        // Default to "en" if still empty
        if feed.lang.is_empty() {
            feed.lang = "en".to_string();
        }

        // Classify category - always try to find one
        if let Some(cat) = classify_category(&feed.url, &url_patterns, &domain_rules) {
            // Override if: empty, generic, or video
            if feed.category.is_empty() || feed.category == "other" || feed.category == "news" {
                feed.category = cat;
                classified_cat += 1;
            }
        }
        // Default to "news" if still empty (but not for video)
        if feed.category.is_empty() && !feed.url.contains("youtube.com") {
            feed.category = "news".to_string();
        }

        // Update stats
        *lang_stats.entry(feed.lang.clone()).or_insert(0) += 1;
        *cat_stats.entry(feed.category.clone()).or_insert(0) += 1;

        // Write classified feed
        let json = serde_json::to_string(&feed).unwrap();
        writeln!(writer, "{}", json).unwrap();

        if total % 50000 == 0 {
            println!("Processed {} feeds...", total);
        }
    }

    writer.flush().unwrap();

    println!("\n=== Results ===");
    println!("Total feeds: {}", total);
    println!("Classified language: {}", classified_lang);
    println!("Classified category: {}", classified_cat);

    println!("\n=== Language Distribution (top 20) ===");
    let mut lang_vec: Vec<_> = lang_stats.iter().collect();
    lang_vec.sort_by(|a, b| b.1.cmp(a.1));
    for (lang, count) in lang_vec.iter().take(20) {
        println!("  {}: {}", lang, count);
    }

    println!("\n=== Category Distribution (top 20) ===");
    let mut cat_vec: Vec<_> = cat_stats.iter().collect();
    cat_vec.sort_by(|a, b| b.1.cmp(a.1));
    for (cat, count) in cat_vec.iter().take(20) {
        println!("  {}: {}", cat, count);
    }

    println!("\nDone! Output written to: {}", output);
}

/// Extract domain from URL
fn extract_domain(url: &str) -> String {
    let url = url.to_lowercase();
    // Remove protocol
    let without_proto = url.split("://").last().unwrap_or(&url);
    // Get domain part (before first /)
    let domain = without_proto.split('/').next().unwrap_or(without_proto);
    // Remove port if present
    domain.split(':').next().unwrap_or(domain).to_string()
}

fn detect_language(url: &str, patterns: &[(Regex, &str)]) -> Option<String> {
    let url_lower = url.to_lowercase();
    let domain = extract_domain(url);

    // Check URL patterns first (domain-specific news sites)
    for (regex, lang) in patterns {
        if regex.is_match(&url_lower) {
            return Some(lang.to_string());
        }
    }

    // Check TLD patterns on domain only
    let tld_langs = [
        (".co.kr", "ko"), (".or.kr", "ko"), (".go.kr", "ko"),
        (".co.jp", "ja"), (".ne.jp", "ja"), (".or.jp", "ja"),
        (".com.cn", "zh"), (".com.tw", "zh"), (".com.hk", "zh"),
        (".com.br", "pt"),
    ];

    // Check compound TLDs first
    for (tld, lang) in tld_langs {
        if domain.ends_with(tld) {
            return Some(lang.to_string());
        }
    }

    // Check simple TLDs
    let simple_tld_langs = [
        (".kr", "ko"),
        (".jp", "ja"),
        (".cn", "zh"), (".tw", "zh"), (".hk", "zh"),
        (".de", "de"), (".at", "de"),
        (".fr", "fr"),
        (".es", "es"), (".mx", "es"), (".ar", "es"),
        (".pt", "pt"), (".br", "pt"),
        (".it", "it"),
        (".ru", "ru"),
        (".nl", "nl"),
        (".pl", "pl"),
        (".se", "sv"),
        (".no", "no"),
        (".dk", "da"),
        (".fi", "fi"),
        (".cz", "cs"),
        (".hu", "hu"),
        (".ro", "ro"),
        (".ua", "uk"),
        (".bg", "bg"),
        (".hr", "hr"),
        (".sk", "sk"),
        (".si", "sl"),
        (".rs", "sr"),
        (".gr", "el"),
        (".tr", "tr"),
        (".il", "he"),
        (".ir", "fa"),
        (".sa", "ar"), (".ae", "ar"), (".eg", "ar"), (".qa", "ar"),
        (".in", "hi"),
        (".pk", "ur"),
        (".bd", "bn"),
        (".th", "th"),
        (".vn", "vi"),
        (".id", "id"),
        (".my", "ms"),
        (".ph", "tl"),
    ];

    for (tld, lang) in simple_tld_langs {
        if domain.ends_with(tld) {
            return Some(lang.to_string());
        }
    }

    // Check for language codes in URL
    let lang_codes = [
        ("/ko/", "ko"), ("/kr/", "ko"), ("hl=ko", "ko"), ("lang=ko", "ko"),
        ("/ja/", "ja"), ("/jp/", "ja"), ("hl=ja", "ja"), ("lang=ja", "ja"),
        ("/zh/", "zh"), ("/cn/", "zh"), ("hl=zh", "zh"), ("lang=zh", "zh"),
        ("/de/", "de"), ("hl=de", "de"), ("lang=de", "de"),
        ("/fr/", "fr"), ("hl=fr", "fr"), ("lang=fr", "fr"),
        ("/es/", "es"), ("hl=es", "es"), ("lang=es", "es"),
        ("/pt/", "pt"), ("hl=pt", "pt"), ("lang=pt", "pt"),
        ("/it/", "it"), ("hl=it", "it"), ("lang=it", "it"),
        ("/ru/", "ru"), ("hl=ru", "ru"), ("lang=ru", "ru"),
        ("/ar/", "ar"), ("hl=ar", "ar"), ("lang=ar", "ar"),
        ("/hi/", "hi"), ("hl=hi", "hi"), ("lang=hi", "hi"),
        ("/vi/", "vi"), ("hl=vi", "vi"), ("lang=vi", "vi"),
        ("/th/", "th"), ("hl=th", "th"), ("lang=th", "th"),
        ("/id/", "id"), ("hl=id", "id"), ("lang=id", "id"),
        ("/tr/", "tr"), ("hl=tr", "tr"), ("lang=tr", "tr"),
        ("/nl/", "nl"), ("hl=nl", "nl"), ("lang=nl", "nl"),
        ("/pl/", "pl"), ("hl=pl", "pl"), ("lang=pl", "pl"),
        ("/sv/", "sv"), ("hl=sv", "sv"), ("lang=sv", "sv"),
        ("/uk/", "uk"), ("hl=uk", "uk"), ("lang=uk", "uk"),
        ("/el/", "el"), ("hl=el", "el"), ("lang=el", "el"),
        ("/he/", "he"), ("hl=he", "he"), ("lang=he", "he"), ("hl=iw", "he"),
        ("/fa/", "fa"), ("hl=fa", "fa"), ("lang=fa", "fa"),
    ];

    for (pattern, lang) in lang_codes {
        if url_lower.contains(pattern) {
            return Some(lang.to_string());
        }
    }

    None
}

fn classify_category(url: &str, url_patterns: &[(Regex, &str)], domain_rules: &HashMap<&str, &str>) -> Option<String> {
    let url_lower = url.to_lowercase();

    // Check domain rules first (most specific)
    for (domain, category) in domain_rules {
        if url_lower.contains(domain) {
            return Some(category.to_string());
        }
    }

    // Check URL patterns
    for (regex, category) in url_patterns {
        if regex.is_match(&url_lower) {
            return Some(category.to_string());
        }
    }

    None
}

fn load_url_patterns() -> Vec<(Regex, &'static str)> {
    vec![
        // Sports
        (Regex::new(r"sports?\.").unwrap(), "sports"),
        (Regex::new(r"/sports?/").unwrap(), "sports"),
        (Regex::new(r"/sport/").unwrap(), "sports"),
        (Regex::new(r"/esports?/").unwrap(), "sports"),
        (Regex::new(r"topic/SPORTS").unwrap(), "sports"),
        (Regex::new(r"/football/").unwrap(), "sports"),
        (Regex::new(r"/soccer/").unwrap(), "sports"),
        (Regex::new(r"/baseball/").unwrap(), "sports"),
        (Regex::new(r"/basketball/").unwrap(), "sports"),
        (Regex::new(r"/tennis/").unwrap(), "sports"),
        (Regex::new(r"/golf/").unwrap(), "sports"),
        (Regex::new(r"/nba/").unwrap(), "sports"),
        (Regex::new(r"/nfl/").unwrap(), "sports"),
        (Regex::new(r"/mlb/").unwrap(), "sports"),

        // Tech
        (Regex::new(r"tech\.").unwrap(), "tech"),
        (Regex::new(r"/tech/").unwrap(), "tech"),
        (Regex::new(r"/technology/").unwrap(), "tech"),
        (Regex::new(r"/it/").unwrap(), "tech"),
        (Regex::new(r"/digital/").unwrap(), "tech"),
        (Regex::new(r"/gadget/").unwrap(), "tech"),
        (Regex::new(r"/ai/").unwrap(), "tech"),
        (Regex::new(r"topic/TECHNOLOGY").unwrap(), "tech"),
        (Regex::new(r"/science/").unwrap(), "science"),
        (Regex::new(r"topic/SCIENCE").unwrap(), "science"),

        // Business/Economy
        (Regex::new(r"/business/").unwrap(), "business"),
        (Regex::new(r"/economy/").unwrap(), "business"),
        (Regex::new(r"/finance/").unwrap(), "finance"),
        (Regex::new(r"/money/").unwrap(), "finance"),
        (Regex::new(r"/stock/").unwrap(), "finance"),
        (Regex::new(r"/market/").unwrap(), "finance"),
        (Regex::new(r"topic/BUSINESS").unwrap(), "business"),

        // Politics
        (Regex::new(r"/politic").unwrap(), "politics"),
        (Regex::new(r"/government/").unwrap(), "politics"),
        (Regex::new(r"/election/").unwrap(), "politics"),
        (Regex::new(r"topic/NATION").unwrap(), "politics"),

        // World
        (Regex::new(r"/world/").unwrap(), "world"),
        (Regex::new(r"/international/").unwrap(), "world"),
        (Regex::new(r"/global/").unwrap(), "world"),
        (Regex::new(r"/foreign/").unwrap(), "world"),
        (Regex::new(r"topic/WORLD").unwrap(), "world"),

        // Entertainment
        (Regex::new(r"/entertain").unwrap(), "entertainment"),
        (Regex::new(r"/celeb").unwrap(), "entertainment"),
        (Regex::new(r"/star/").unwrap(), "entertainment"),
        (Regex::new(r"/drama/").unwrap(), "entertainment"),
        (Regex::new(r"/movie/").unwrap(), "entertainment"),
        (Regex::new(r"/music/").unwrap(), "entertainment"),
        (Regex::new(r"topic/ENTERTAINMENT").unwrap(), "entertainment"),

        // Health
        (Regex::new(r"/health/").unwrap(), "health"),
        (Regex::new(r"/medical/").unwrap(), "health"),
        (Regex::new(r"/wellness/").unwrap(), "health"),
        (Regex::new(r"topic/HEALTH").unwrap(), "health"),

        // Culture
        (Regex::new(r"/culture/").unwrap(), "culture"),
        (Regex::new(r"/art/").unwrap(), "culture"),
        (Regex::new(r"/museum/").unwrap(), "culture"),
        (Regex::new(r"/book/").unwrap(), "culture"),

        // Lifestyle
        (Regex::new(r"/life/").unwrap(), "lifestyle"),
        (Regex::new(r"/lifestyle/").unwrap(), "lifestyle"),
        (Regex::new(r"/living/").unwrap(), "lifestyle"),
        (Regex::new(r"/food/").unwrap(), "lifestyle"),
        (Regex::new(r"/travel/").unwrap(), "lifestyle"),
        (Regex::new(r"/auto/").unwrap(), "lifestyle"),

        // Opinion
        (Regex::new(r"/opinion/").unwrap(), "opinion"),
        (Regex::new(r"/editorial/").unwrap(), "opinion"),
        (Regex::new(r"/column/").unwrap(), "opinion"),

        // YouTube
        (Regex::new(r"youtube\.com/feeds").unwrap(), "video"),
    ]
}

fn load_domain_rules() -> HashMap<&'static str, &'static str> {
    let mut rules = HashMap::new();

    // Sports domains
    for domain in ["espn.com", "goal.com", "bleacherreport.com", "sports.yahoo.com",
                   "sports.chosun.com", "sports.donga.com", "sports.khan.co.kr",
                   "osen.co.kr", "xportsnews.com", "stoo.com", "sportstoto.co.kr"] {
        rules.insert(domain, "sports");
    }

    // Tech domains
    for domain in ["techcrunch.com", "theverge.com", "arstechnica.com", "wired.com",
                   "engadget.com", "gizmodo.com", "techradar.com", "cnet.com",
                   "zdnet.co.kr", "bloter.net", "itworld.co.kr", "etnews.com"] {
        rules.insert(domain, "tech");
    }

    // Finance domains
    for domain in ["bloomberg.com", "wsj.com", "ft.com", "reuters.com",
                   "marketwatch.com", "cnbc.com", "mk.co.kr", "hankyung.com",
                   "sedaily.com", "edaily.co.kr", "fnnews.com"] {
        rules.insert(domain, "finance");
    }

    // Entertainment domains
    for domain in ["variety.com", "tmz.com", "ew.com", "billboard.com",
                   "dispatch.co.kr", "tenasia.co.kr", "tvreport.co.kr"] {
        rules.insert(domain, "entertainment");
    }

    // Science domains
    for domain in ["nature.com", "sciencemag.org", "sciencedaily.com", "phys.org",
                   "sciencetimes.co.kr", "dongascience.com"] {
        rules.insert(domain, "science");
    }

    // Health domains
    for domain in ["webmd.com", "healthline.com", "medscape.com",
                   "health.chosun.com", "hidoc.co.kr", "kormedi.com"] {
        rules.insert(domain, "health");
    }

    rules
}

fn load_lang_patterns() -> Vec<(Regex, &'static str)> {
    vec![
        // Korean patterns
        (Regex::new(r"chosun\.com|donga\.com|khan\.co\.kr|hani\.co\.kr|joongang\.co\.kr").unwrap(), "ko"),
        (Regex::new(r"yonhapnews\.co\.kr|yna\.co\.kr|ytn\.co\.kr|mbc\.co\.kr|kbs\.co\.kr|sbs\.co\.kr").unwrap(), "ko"),
        (Regex::new(r"mk\.co\.kr|hankyung\.com|sedaily\.com|mt\.co\.kr").unwrap(), "ko"),
        (Regex::new(r"naver\.com|daum\.net|kakao\.com").unwrap(), "ko"),

        // Japanese patterns
        (Regex::new(r"nhk\.or\.jp|asahi\.com|yomiuri\.co\.jp|mainichi\.jp|nikkei\.com|sankei\.com").unwrap(), "ja"),
        (Regex::new(r"yahoo\.co\.jp|livedoor\.jp|hatena\.ne\.jp").unwrap(), "ja"),

        // Chinese patterns
        (Regex::new(r"xinhuanet\.com|people\.com\.cn|chinanews\.com|sohu\.com|sina\.com").unwrap(), "zh"),
        (Regex::new(r"ltn\.com\.tw|udn\.com|chinatimes\.com|cna\.com\.tw").unwrap(), "zh"),
        (Regex::new(r"scmp\.com|hk01\.com|rthk\.hk").unwrap(), "zh"),

        // German patterns
        (Regex::new(r"spiegel\.de|zeit\.de|faz\.net|welt\.de|sueddeutsche\.de|tagesschau\.de").unwrap(), "de"),

        // French patterns
        (Regex::new(r"lemonde\.fr|lefigaro\.fr|liberation\.fr|france24\.com|rfi\.fr").unwrap(), "fr"),

        // Spanish patterns
        (Regex::new(r"elpais\.com|elmundo\.es|abc\.es|rtve\.es|lavanguardia\.com").unwrap(), "es"),

        // Portuguese patterns
        (Regex::new(r"globo\.com|folha\.uol\.com\.br|estadao\.com\.br|publico\.pt").unwrap(), "pt"),

        // Italian patterns
        (Regex::new(r"repubblica\.it|corriere\.it|lastampa\.it|ansa\.it").unwrap(), "it"),

        // Russian patterns
        (Regex::new(r"ria\.ru|tass\.ru|lenta\.ru|rbc\.ru|kommersant\.ru").unwrap(), "ru"),

        // Arabic patterns
        (Regex::new(r"aljazeera\.net|alarabiya\.net|youm7\.com|alahram\.org").unwrap(), "ar"),
    ]
}
