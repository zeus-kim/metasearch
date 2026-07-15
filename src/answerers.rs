//! Local instant-answer widgets, à la the standard "answerers".
//!
//! These run entirely offline (no upstream request, no query logging) and
//! populate the response `answers` field:
//! * arithmetic calculator (`2 * (3 + 4)`, `sqrt(2)`, `sin(pi/2)`)
//! * unit conversion (`10 km to miles`, `100 c to f`, `5 kg in lb`)
//! * date / time (`time`, `utc`, `date`)
//! * random helpers (`flip a coin`, `roll 2d6`, `random 1-100`)
//! * `uuid` / `guid`
//! * hashing (`sha256 hello`, `sha512 foo`)

#![allow(dead_code)] // Some functions reserved for future features

use std::time::Duration;

use serde_json::Value;
use sha2::{Digest, Sha256, Sha512};

use crate::engines::USER_AGENT;
use crate::types::Answer;

const ENGINE: &str = "answer";

/// Run every answerer against the query, returning any instant answers.
pub fn answer(query: &str) -> Vec<Answer> {
    let q = query.trim();
    if q.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    if let Some(a) = calculator(q) {
        out.push(a);
    }
    if let Some(a) = unit_convert(q) {
        out.push(a);
    }
    if let Some(a) = datetime(q) {
        out.push(a);
    }
    if let Some(a) = random(q) {
        out.push(a);
    }
    if let Some(a) = uuid(q) {
        out.push(a);
    }
    if let Some(a) = hashing(q) {
        out.push(a);
    }
    out
}

// ----------------------------------------------------------- online answerers

/// Network-backed instant answers: live currency conversion and dictionary
/// definitions. Each only fires when the query *clearly* matches its trigger
/// pattern, so ordinary searches never cause an upstream request. Any
/// network/parse failure degrades silently to no answer (offline-safe).
///
/// Privacy: like every other engine call, the matched term is sent to the
/// upstream (Frankfurter / dictionaryapi.dev) but is never logged locally.
pub async fn answer_online(
    query: &str,
    client: &reqwest::Client,
    timeout: Duration,
) -> Vec<Answer> {
    let q = query.trim();
    if q.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    if let Some(req) = currency_request(q) {
        if let Some(a) = fetch_currency(client, &req, timeout).await {
            out.push(a);
        }
    } else if let Some(place) = weather_request(q) {
        if let Some(a) = fetch_weather(client, &place, timeout).await {
            out.push(a);
        }
    } else if let Some(req) = translate_request(q) {
        if let Some(a) = fetch_translation(client, &req, timeout).await {
            out.push(a);
        }
    } else if let Some(term) = define_request(q) {
        if let Some(a) = fetch_definition(client, &term, timeout).await {
            out.push(a);
        }
    } else if let Some(req) = crypto_request(q) {
        if let Some(a) = fetch_crypto(client, &req, timeout).await {
            out.push(a);
        }
    } else if let Some(req) = stock_request(q) {
        if let Some(a) = fetch_stock(client, &req, timeout).await {
            out.push(a);
        }
    } else if let Some(req) = filing_request(q) {
        if let Some(a) = fetch_filing(client, &req, timeout).await {
            out.push(a);
        }
    }
    out
}

// ------------------------------------------------------------------- stock

/// A parsed stock price request.
#[derive(Debug, Clone)]
pub struct StockRequest {
    pub symbol: String,
    pub name: String,
}

// ----------------------------------------------------------- fuzzy matching

/// Lightweight fuzzy matching helper for handling typos, spacing variations, and partial matches.
/// Returns a similarity score from 0.0 to 1.0.
fn fuzzy_score(query: &str, target: &str) -> f64 {
    let q = query.to_lowercase();
    let t = target.to_lowercase();

    // Exact match
    if q == t {
        return 1.0;
    }

    // Contains (for partial matches like "삼성" -> "삼성전자")
    if t.contains(&q) {
        return 0.9;
    }

    // Remove spaces and compare (handles "삼성전자주가" vs "삼성 전자")
    let q_nospace: String = q.chars().filter(|c| !c.is_whitespace()).collect();
    let t_nospace: String = t.chars().filter(|c| !c.is_whitespace()).collect();
    if q_nospace == t_nospace || t_nospace.contains(&q_nospace) {
        return 0.85;
    }

    // Edit distance based scoring (for typos)
    let dist = levenshtein(&q_nospace, &t_nospace);
    let max_len = q_nospace.chars().count().max(t_nospace.chars().count());
    if max_len == 0 {
        return 0.0;
    }
    let similarity = 1.0 - (dist as f64 / max_len as f64);

    // Only accept if similarity is high enough (typo tolerance)
    if similarity >= 0.7 {
        similarity * 0.8 // Scale down slightly since it's not exact
    } else {
        0.0
    }
}

/// Simple Levenshtein distance implementation (no external deps).
fn levenshtein(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let m = a_chars.len();
    let n = b_chars.len();

    if m == 0 { return n; }
    if n == 0 { return m; }

    // Use two rows instead of full matrix for memory efficiency
    let mut prev: Vec<usize> = (0..=n).collect();
    let mut curr: Vec<usize> = vec![0; n + 1];

    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = if a_chars[i - 1] == b_chars[j - 1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1)             // deletion
                .min(curr[j - 1] + 1)           // insertion
                .min(prev[j - 1] + cost);       // substitution
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[n]
}

/// Find the best matching entry from a list using fuzzy matching.
/// Returns (symbol, display_name, score) if a match is found with score >= threshold.
fn fuzzy_find_company<'a>(
    query: &str,
    companies: &'a [(&[&str], &str, &str)],
    threshold: f64,
) -> Option<(&'a str, &'a str, f64)> {
    let mut best: Option<(&str, &str, f64)> = None;

    for (keywords, symbol, display) in companies {
        for &kw in *keywords {
            let score = fuzzy_score(query, kw);
            if score >= threshold {
                if best.is_none() || score > best.unwrap().2 {
                    best = Some((symbol, display, score));
                }
            }
        }
    }
    best
}

/// Find the best matching city using fuzzy matching.
fn fuzzy_find_city<'a>(query: &str, cities: &'a [(&str, &str)], threshold: f64) -> Option<&'a str> {
    let mut best: Option<(&'a str, f64)> = None;

    for &(keyword, city) in cities {
        let score = fuzzy_score(query, keyword);
        if score >= threshold {
            if best.is_none() || score > best.unwrap().1 {
                best = Some((city, score));
            }
        }
    }
    best.map(|(city, _)| city)
}

/// Common Korean keyboard typos: maps mistyped jamo combinations to correct ones.
/// E.g., "삼성전ㅈ" (forgot to complete 자) -> try matching "삼성전자"
fn normalize_korean_typos(s: &str) -> String {
    // Handle common incomplete jamo at the end
    let mut result = s.to_string();

    // Common typo patterns: trailing incomplete consonants
    let typo_fixes = [
        ("ㅈ", "자"), ("ㄱ", "가"), ("ㄴ", "나"), ("ㄷ", "다"),
        ("ㄹ", "라"), ("ㅁ", "마"), ("ㅂ", "바"), ("ㅅ", "사"),
        ("ㅇ", "아"), ("ㅎ", "하"), ("ㅊ", "차"), ("ㅋ", "카"),
        ("ㅌ", "타"), ("ㅍ", "파"),
    ];

    for (typo, fix) in typo_fixes {
        if result.ends_with(typo) {
            result = format!("{}{}", &result[..result.len() - typo.len()], fix);
            break;
        }
    }
    result
}

/// Map Korean company names to Yahoo Finance tickers.
fn korean_stock_ticker(name: &str) -> Option<(&'static str, &'static str)> {
    match name {
        "삼성전자" | "삼성" => Some(("005930.KS", "삼성전자")),
        "sk하이닉스" | "하이닉스" => Some(("000660.KS", "SK하이닉스")),
        "네이버" | "naver" => Some(("035420.KS", "네이버")),
        "카카오" | "kakao" => Some(("035720.KS", "카카오")),
        "현대차" | "현대자동차" => Some(("005380.KS", "현대자동차")),
        "기아" | "기아차" => Some(("000270.KS", "기아")),
        "lg전자" => Some(("066570.KS", "LG전자")),
        "셀트리온" => Some(("068270.KS", "셀트리온")),
        "삼성바이오" | "삼성바이오로직스" => Some(("207940.KS", "삼성바이오로직스")),
        "포스코" => Some(("005490.KS", "POSCO홀딩스")),
        "삼성sdi" | "삼성에스디아이" => Some(("006400.KS", "삼성SDI")),
        "lg화학" => Some(("051910.KS", "LG화학")),
        "현대모비스" => Some(("012330.KS", "현대모비스")),
        "lg에너지솔루션" | "lg에너지" => Some(("373220.KS", "LG에너지솔루션")),
        "kb금융" => Some(("105560.KS", "KB금융")),
        "신한지주" | "신한금융" => Some(("055550.KS", "신한지주")),
        _ => None,
    }
}

/// Map US company names to tickers.
fn us_stock_ticker(name: &str) -> Option<(&'static str, &'static str)> {
    match name {
        "apple" | "애플" => Some(("AAPL", "Apple")),
        "microsoft" | "마이크로소프트" | "ms" => Some(("MSFT", "Microsoft")),
        "google" | "구글" | "alphabet" => Some(("GOOGL", "Alphabet")),
        "amazon" | "아마존" => Some(("AMZN", "Amazon")),
        "tesla" | "테슬라" => Some(("TSLA", "Tesla")),
        "nvidia" | "엔비디아" => Some(("NVDA", "NVIDIA")),
        "meta" | "메타" | "facebook" | "페이스북" => Some(("META", "Meta")),
        "netflix" | "넷플릭스" => Some(("NFLX", "Netflix")),
        "amd" => Some(("AMD", "AMD")),
        "intel" | "인텔" => Some(("INTC", "Intel")),
        _ => None,
    }
}

/// Detect stock price query - requires explicit intent keywords like "주가", "stock", "가격".
/// Returns StockRequest with symbol="" if company not in cache (will search via Yahoo API).
pub fn stock_request(q: &str) -> Option<StockRequest> {
    let lower = q.trim().to_ascii_lowercase();

    // Must have explicit stock intent keyword
    let stock_keywords = ["주가", "주식", "stock", "시세", "stock price"];
    let has_stock_intent = stock_keywords.iter().any(|kw| lower.contains(kw));

    if !has_stock_intent {
        return None;
    }

    // Extract company name by removing stock keywords
    let mut company_name = lower.clone();
    for kw in &stock_keywords {
        company_name = company_name.replace(kw, "");
    }
    let company_name = company_name.trim().to_string();

    if company_name.is_empty() {
        return None;
    }

    // Check Korean companies (cached)
    for (keywords, symbol, display) in KOREAN_COMPANIES {
        for kw in *keywords {
            if lower.contains(kw) {
                return Some(StockRequest {
                    symbol: symbol.to_string(),
                    name: display.to_string(),
                });
            }
        }
    }

    // Check US companies (cached)
    for (keywords, symbol, display) in US_COMPANIES {
        for kw in *keywords {
            if lower.contains(kw) {
                return Some(StockRequest {
                    symbol: symbol.to_string(),
                    name: display.to_string(),
                });
            }
        }
    }

    // Check for direct ticker (e.g., "AAPL stock", "NVDA 주가")
    let skip_words = ["stock", "price", "share", "shares"];
    for word in lower.split_whitespace() {
        if skip_words.contains(&word) {
            continue;
        }
        let upper = word.to_ascii_uppercase();
        if upper.len() >= 2 && upper.len() <= 5 && upper.chars().all(|c| c.is_ascii_alphabetic()) {
            return Some(StockRequest {
                symbol: upper.clone(),
                name: upper,
            });
        }
    }

    // Not in cache - return with empty symbol to trigger Yahoo search
    Some(StockRequest {
        symbol: String::new(),
        name: company_name,
    })
}

const KOREAN_COMPANIES: &[(&[&str], &str, &str)] = &[
    (&["삼성전자", "삼성 전자"], "005930.KS", "삼성전자"),
    (&["sk하이닉스", "하이닉스"], "000660.KS", "SK하이닉스"),
    (&["네이버"], "035420.KS", "네이버"),
    (&["카카오"], "035720.KS", "카카오"),
    (&["현대차", "현대자동차"], "005380.KS", "현대자동차"),
    (&["기아"], "000270.KS", "기아"),
    (&["lg전자", "엘지전자"], "066570.KS", "LG전자"),
    (&["셀트리온"], "068270.KS", "셀트리온"),
    (&["삼성바이오"], "207940.KS", "삼성바이오로직스"),
    (&["포스코"], "005490.KS", "POSCO홀딩스"),
    (&["삼성sdi"], "006400.KS", "삼성SDI"),
    (&["lg화학", "엘지화학"], "051910.KS", "LG화학"),
    (&["현대모비스"], "012330.KS", "현대모비스"),
    (&["lg에너지솔루션"], "373220.KS", "LG에너지솔루션"),
    (&["kb금융"], "105560.KS", "KB금융"),
    (&["신한지주", "신한금융"], "055550.KS", "신한지주"),
];

const US_COMPANIES: &[(&[&str], &str, &str)] = &[
    (&["apple", "애플"], "AAPL", "Apple"),
    (&["microsoft", "마이크로소프트"], "MSFT", "Microsoft"),
    (&["google", "구글", "alphabet"], "GOOGL", "Alphabet"),
    (&["amazon", "아마존"], "AMZN", "Amazon"),
    (&["tesla", "테슬라"], "TSLA", "Tesla"),
    (&["nvidia", "엔비디아"], "NVDA", "NVIDIA"),
    (&["meta", "메타", "facebook", "페이스북"], "META", "Meta"),
    (&["netflix", "넷플릭스"], "NFLX", "Netflix"),
    (&["amd"], "AMD", "AMD"),
    (&["intel", "인텔"], "INTC", "Intel"),
];

fn is_known_ticker(ticker: &str) -> bool {
    matches!(ticker,
        "AAPL" | "MSFT" | "GOOGL" | "GOOG" | "AMZN" | "TSLA" | "NVDA" | "META" | "NFLX" |
        "AMD" | "INTC" | "IBM" | "ORCL" | "CRM" | "ADBE" | "PYPL" | "SQ" | "SHOP" |
        "UBER" | "LYFT" | "ABNB" | "COIN" | "HOOD" | "PLTR" | "SNOW" | "NET" |
        "JPM" | "BAC" | "WFC" | "GS" | "MS" | "V" | "MA" |
        "DIS" | "CMCSA" | "T" | "VZ" |
        "JNJ" | "PFE" | "UNH" | "MRK" | "LLY" |
        "XOM" | "CVX" | "BP" |
        "WMT" | "COST" | "HD" | "NKE" | "SBUX" | "MCD" |
        "BA" | "CAT" | "GE" | "LMT"
    )
}

async fn fetch_stock(
    client: &reqwest::Client,
    req: &StockRequest,
    timeout: Duration,
) -> Option<Answer> {
    let symbol = if req.symbol.is_empty() {
        // Search for symbol via Yahoo Finance API
        search_yahoo_symbol(client, &req.name, timeout).await?
    } else {
        req.symbol.clone()
    };

    let url = format!(
        "https://query1.finance.yahoo.com/v8/finance/chart/{}?interval=1d&range=1d",
        symbol
    );

    let body: Value = client
        .get(&url)
        .header("User-Agent", USER_AGENT)
        .timeout(timeout)
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;

    let req_with_symbol = StockRequest {
        symbol: symbol.clone(),
        name: if req.name.is_empty() || req.name == req.symbol {
            get_company_name_from_response(&body).unwrap_or(symbol.clone())
        } else {
            req.name.clone()
        },
    };
    parse_stock(&body, &req_with_symbol)
}

/// Search Yahoo Finance for a stock symbol by company name.
async fn search_yahoo_symbol(
    client: &reqwest::Client,
    query: &str,
    timeout: Duration,
) -> Option<String> {
    let body: Value = client
        .get("https://query1.finance.yahoo.com/v1/finance/search")
        .header("User-Agent", USER_AGENT)
        .query(&[("q", query), ("quotesCount", "1"), ("newsCount", "0")])
        .timeout(timeout)
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;

    body.get("quotes")?
        .get(0)?
        .get("symbol")?
        .as_str()
        .map(|s| s.to_string())
}

fn get_company_name_from_response(body: &Value) -> Option<String> {
    body.get("chart")?
        .get("result")?
        .get(0)?
        .get("meta")?
        .get("shortName")
        .or_else(|| body.get("chart")?.get("result")?.get(0)?.get("meta")?.get("longName"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

pub fn parse_stock(body: &Value, req: &StockRequest) -> Option<Answer> {
    let chart = body.get("chart")?.get("result")?.get(0)?;
    let meta = chart.get("meta")?;

    let price = meta.get("regularMarketPrice")?.as_f64()?;
    let prev_close = meta.get("previousClose").and_then(|v| v.as_f64()).unwrap_or(price);
    let currency = meta.get("currency").and_then(|v| v.as_str()).unwrap_or("USD");

    let change = price - prev_close;
    let change_pct = if prev_close > 0.0 { (change / prev_close) * 100.0 } else { 0.0 };

    let arrow = if change >= 0.0 { "▲" } else { "▼" };
    let sign = if change >= 0.0 { "+" } else { "" };

    let answer = format!(
        "{} ({}) · {} {:.2} {} {}{:.2} ({}{:.2}%)",
        req.name, req.symbol, currency, price, arrow, sign, change, sign, change_pct
    );

    let url = format!("https://finance.yahoo.com/quote/{}", req.symbol);
    Some(Answer::new(answer, ENGINE).with_url(&url))
}

// ------------------------------------------------------------------ crypto

#[derive(Debug, Clone)]
pub struct CryptoRequest {
    pub id: String,
    pub name: String,
}

/// Detect cryptocurrency query.
pub fn crypto_request(q: &str) -> Option<CryptoRequest> {
    let lower = q.trim().to_lowercase();

    // Must have crypto intent keyword
    let crypto_keywords = ["시세", "가격", "price", "코인", "coin", "crypto"];
    let has_crypto_intent = crypto_keywords.iter().any(|kw| lower.contains(kw));

    if !has_crypto_intent {
        return None;
    }

    // Check known cryptocurrencies
    for &(keywords, id, name) in CRYPTO_LIST {
        for &kw in keywords {
            if lower.contains(kw) {
                return Some(CryptoRequest {
                    id: id.to_string(),
                    name: name.to_string(),
                });
            }
        }
    }

    None
}

const CRYPTO_LIST: &[(&[&str], &str, &str)] = &[
    (&["비트코인", "bitcoin", "btc"], "bitcoin", "Bitcoin"),
    (&["이더리움", "ethereum", "eth", "이더"], "ethereum", "Ethereum"),
    (&["리플", "ripple", "xrp"], "ripple", "XRP"),
    (&["솔라나", "solana", "sol"], "solana", "Solana"),
    (&["에이다", "카르다노", "cardano", "ada"], "cardano", "Cardano"),
    (&["도지코인", "도지", "dogecoin", "doge"], "dogecoin", "Dogecoin"),
    (&["폴리곤", "매틱", "polygon", "matic"], "matic-network", "Polygon"),
    (&["아발란체", "avalanche", "avax"], "avalanche-2", "Avalanche"),
    (&["체인링크", "chainlink", "link"], "chainlink", "Chainlink"),
    (&["폴카닷", "polkadot", "dot"], "polkadot", "Polkadot"),
    (&["시바이누", "shiba", "shib"], "shiba-inu", "Shiba Inu"),
    (&["라이트코인", "litecoin", "ltc"], "litecoin", "Litecoin"),
    (&["유니스왑", "uniswap", "uni"], "uniswap", "Uniswap"),
    (&["스텔라", "stellar", "xlm"], "stellar", "Stellar"),
    (&["트론", "tron", "trx"], "tron", "TRON"),
];

async fn fetch_crypto(
    client: &reqwest::Client,
    req: &CryptoRequest,
    timeout: Duration,
) -> Option<Answer> {
    let body: Value = client
        .get("https://api.coingecko.com/api/v3/simple/price")
        .header("User-Agent", USER_AGENT)
        .query(&[
            ("ids", req.id.as_str()),
            ("vs_currencies", "usd,krw"),
            ("include_24hr_change", "true"),
        ])
        .timeout(timeout)
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;

    parse_crypto(&body, req)
}

fn parse_crypto(body: &Value, req: &CryptoRequest) -> Option<Answer> {
    let data = body.get(&req.id)?;
    let usd = data.get("usd")?.as_f64()?;
    let krw = data.get("krw").and_then(|v| v.as_f64());
    let change = data.get("usd_24h_change").and_then(|v| v.as_f64()).unwrap_or(0.0);

    let arrow = if change >= 0.0 { "▲" } else { "▼" };
    let sign = if change >= 0.0 { "+" } else { "" };

    let krw_str = krw.map(|k| format!(" (₩{:.0})", k)).unwrap_or_default();
    let answer = format!(
        "{} · ${:.2}{} {} {}{:.2}%",
        req.name, usd, krw_str, arrow, sign, change
    );

    let url = format!("https://www.coingecko.com/en/coins/{}", req.id);
    Some(Answer::new(answer, ENGINE).with_url(&url))
}

// ------------------------------------------------------------------ weather

/// Detect a weather query and return the place name. Pure & offline.
/// Now uses keyword detection - if "날씨", "weather", "기온" etc. found, extract city or use default.
pub fn weather_request(q: &str) -> Option<String> {
    let lower = q.trim().to_ascii_lowercase();

    // Check for weather keywords (including common typos). English keywords
    // must match whole words — "weatherproof" is not a weather query.
    let has_weather_keyword = lower.contains("날씨")
        || lower.contains("날시")  // typo
        || lower.contains("기온")
        || lower.contains("온도")
        || lower
            .split_whitespace()
            .any(|w| matches!(w, "weather" | "forecast" | "temperature"));

    if !has_weather_keyword {
        return None;
    }

    // Extract place name from query - let geocoding API handle any language
    let place = extract_place_from_query(&lower);

    if !place.is_empty() && place.len() <= 64 {
        return Some(place);
    }

    // No place in the query — don't guess one; let normal search handle it.
    None
}

fn extract_place_from_query(q: &str) -> String {
    // Korean keywords attach without spaces ("서울날씨") — substring removal.
    let mut cleaned = q.replace('?', " ");
    for k in ["날씨", "날시", "기온", "온도"] {
        cleaned = cleaned.replace(k, " ");
    }
    // English keywords and connectives are removed as whole words only —
    // a substring replace of "in" would eat into place names like "berlin".
    cleaned
        .split_whitespace()
        .filter(|w| {
            !matches!(
                *w,
                "weather" | "forecast" | "temperature" | "in" | "for" | "at" | "the" | "today"
                    | "now" | "tomorrow"
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub async fn fetch_weather(client: &reqwest::Client, place: &str, timeout: Duration) -> Option<Answer> {
    // Use wttr.in API - supports Korean city names directly
    // URL encode the place name manually
    let encoded: String = place.chars().map(|c| {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '~' {
            c.to_string()
        } else {
            c.encode_utf8(&mut [0u8; 4]).bytes().map(|b| format!("%{:02X}", b)).collect()
        }
    }).collect();
    let url = format!("https://wttr.in/{}?format=j1&lang=ko", encoded);
    let wx: Value = client
        .get(&url)
        .header("User-Agent", USER_AGENT)
        .timeout(timeout)
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    parse_wttr(&wx, place)
}

fn parse_wttr(body: &Value, query_place: &str) -> Option<Answer> {
    let current = &body["current_condition"][0];
    let area = &body["nearest_area"][0];

    // Use query place name instead of API's area name (wttr.in returns weird names like "Chongdong" for Seoul)
    let country = area["country"][0]["value"].as_str().unwrap_or("");
    let location = if country.is_empty() { query_place.to_string() } else { format!("{}, {}", query_place, country) };

    let temp_c = current["temp_C"].as_str()?;
    let feels_c = current["FeelsLikeC"].as_str().unwrap_or(temp_c);
    let humidity = current["humidity"].as_str().unwrap_or("?");
    let wind_kmph = current["windspeedKmph"].as_str().unwrap_or("?");
    let desc_ko = current["lang_ko"][0]["value"].as_str()
        .or_else(|| current["weatherDesc"][0]["value"].as_str())
        .unwrap_or("");

    // Get weather icon from code
    let code: i64 = current["weatherCode"].as_str()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let icon = wttr_icon(code);

    let mut text = format!(
        "{} {} {}°C ({}°C) · {}% · {}km/h",
        location, icon, temp_c, feels_c, humidity, wind_kmph
    );

    // Add forecast
    if let Some(weather) = body["weather"].as_array() {
        if let Some(today) = weather.get(0) {
            let min = today["mintempC"].as_str().unwrap_or("?");
            let max = today["maxtempC"].as_str().unwrap_or("?");
            text.push_str(&format!(" · {}°~{}°", min, max));
        }
        if let Some(tomorrow) = weather.get(1) {
            let min = tomorrow["mintempC"].as_str().unwrap_or("?");
            let max = tomorrow["maxtempC"].as_str().unwrap_or("?");
            text.push_str(&format!(" → {}°~{}°", min, max));
        }
    }

    // Add Korean description if available
    if !desc_ko.is_empty() {
        text.push_str(&format!(" ({})", desc_ko));
    }

    Some(Answer::new(text, "wttr.in").with_url("https://wttr.in/"))
}

fn wttr_icon(code: i64) -> &'static str {
    match code {
        113 => "☀️",           // Clear
        116 => "🌤",            // Partly cloudy
        119 | 122 => "☁️",     // Cloudy/Overcast
        143 | 248 | 260 => "🌫", // Fog/Mist
        176 | 263 | 266 | 293 | 296 | 299 | 302 | 305 | 308 | 311 | 314 | 353 | 356 | 359 => "🌧", // Rain
        179 | 182 | 185 | 227 | 230 | 281 | 284 | 317 | 320 | 323 | 326 | 329 | 332 | 335 | 338 | 350 | 362 | 365 | 368 | 371 | 374 | 377 => "🌨", // Snow/Sleet
        200 | 386 | 389 | 392 | 395 => "⛈", // Thunder
        _ => "🌡",
    }
}

/// Extract `(lat, lon, "City, Country")` from an open-meteo geocoding reply. Pure.
pub fn parse_geocode(body: &Value) -> Option<(f64, f64, String)> {
    let first = body["results"].as_array()?.first()?;
    let lat = first["latitude"].as_f64()?;
    let lon = first["longitude"].as_f64()?;
    let name = first["name"].as_str().unwrap_or("").to_string();
    let country = first["country"].as_str().unwrap_or("");
    let label = if country.is_empty() {
        name
    } else {
        format!("{name}, {country}")
    };
    Some((lat, lon, label))
}

/// Build a weather answer with current conditions and forecast.
pub fn parse_weather(body: &Value, place: &str) -> Option<Answer> {
    let current = &body["current"];
    let temp = current["temperature_2m"].as_f64()?;
    let humidity = current["relative_humidity_2m"].as_f64().unwrap_or(0.0);
    let feels_like = current["apparent_temperature"].as_f64().unwrap_or(temp);
    let wind = current["wind_speed_10m"].as_f64().unwrap_or(0.0);
    let code = current["weather_code"].as_i64().unwrap_or(-1);

    let daily = &body["daily"];
    let today_max = daily["temperature_2m_max"].as_array().and_then(|a| a.get(0)).and_then(|v| v.as_f64());
    let today_min = daily["temperature_2m_min"].as_array().and_then(|a| a.get(0)).and_then(|v| v.as_f64());
    let tomorrow_code = daily["weather_code"].as_array().and_then(|a| a.get(1)).and_then(|v| v.as_i64());
    let tomorrow_max = daily["temperature_2m_max"].as_array().and_then(|a| a.get(1)).and_then(|v| v.as_f64());
    let tomorrow_min = daily["temperature_2m_min"].as_array().and_then(|a| a.get(1)).and_then(|v| v.as_f64());

    let icon = weather_icon(code);
    let mut text = format!(
        "{} {} {}°C ({}°C) · {}% · {}km/h",
        place, icon, fmt_num(temp), fmt_num(feels_like), fmt_num(humidity), fmt_num(wind)
    );

    if let (Some(max), Some(min)) = (today_max, today_min) {
        text.push_str(&format!(" · {}°~{}°", fmt_num(min), fmt_num(max)));
    }

    if let (Some(code), Some(max), Some(min)) = (tomorrow_code, tomorrow_max, tomorrow_min) {
        let icon = weather_icon(code);
        text.push_str(&format!(" → {} {}°~{}°", icon, fmt_num(min), fmt_num(max)));
    }

    Some(Answer::new(text, ENGINE).with_url("https://open-meteo.com/"))
}

fn weather_icon(code: i64) -> &'static str {
    match code {
        0 => "☀️",
        1..=2 => "🌤",
        3 => "☁️",
        45 | 48 => "🌫",
        51..=67 => "🌧",
        71..=77 => "🌨",
        80..=82 => "🌦",
        85 | 86 => "🌨",
        95..=99 => "⛈",
        _ => "🌡",
    }
}

// ------------------------------------------------------------- translation

/// A parsed `translate <text> to <lang>` request.
#[derive(Debug, Clone, PartialEq)]
pub struct TranslateRequest {
    pub text: String,
    /// Two-letter target language code.
    pub target: String,
}

/// Map a language name/code to a two-letter code recognized by the backend.
fn lang_code(name: &str) -> Option<&'static str> {
    Some(match name.trim() {
        "en" | "english" => "en",
        "es" | "spanish" | "espanol" | "español" => "es",
        "fr" | "french" | "francais" | "français" => "fr",
        "de" | "german" | "deutsch" => "de",
        "it" | "italian" | "italiano" => "it",
        "pt" | "portuguese" | "portugues" | "português" => "pt",
        "nl" | "dutch" => "nl",
        "ru" | "russian" => "ru",
        "ja" | "japanese" => "ja",
        "zh" | "chinese" | "mandarin" => "zh",
        "ko" | "korean" => "ko",
        "ar" | "arabic" => "ar",
        "hi" | "hindi" => "hi",
        _ => return None,
    })
}

/// Detect a `translate X to <lang>` query. Pure & offline.
pub fn translate_request(q: &str) -> Option<TranslateRequest> {
    let lower = q.trim();
    let rest = lower.strip_prefix("translate ").or_else(|| {
        lower
            .strip_prefix("Translate ")
            .or_else(|| lower.strip_prefix("TRANSLATE "))
    })?;
    // Split on the last " to " so the text may itself contain "to".
    let idx = rest.to_ascii_lowercase().rfind(" to ")?;
    let text = rest[..idx].trim().trim_matches('"').trim();
    let target_raw = rest[idx + 4..]
        .trim()
        .trim_end_matches('?')
        .to_ascii_lowercase();
    if text.is_empty() {
        return None;
    }
    let target = lang_code(&target_raw)?;
    Some(TranslateRequest {
        text: text.to_string(),
        target: target.to_string(),
    })
}

async fn fetch_translation(
    client: &reqwest::Client,
    req: &TranslateRequest,
    timeout: Duration,
) -> Option<Answer> {
    // MyMemory translation API (keyless, rate-limited). Auto-detects source.
    let langpair = format!("autodetect|{}", req.target);
    let body: Value = client
        .get("https://api.mymemory.translated.net/get")
        .header("User-Agent", USER_AGENT)
        .query(&[("q", req.text.as_str()), ("langpair", langpair.as_str())])
        .timeout(timeout)
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    parse_translation(&body, req)
}

/// Build a translation answer from a MyMemory reply. Pure.
pub fn parse_translation(body: &Value, req: &TranslateRequest) -> Option<Answer> {
    let translated = body["responseData"]["translatedText"].as_str()?;
    if translated.trim().is_empty() {
        return None;
    }
    let text = format!(
        "{} → [{}] {}",
        req.text,
        req.target.to_uppercase(),
        translated
    );
    Some(Answer::new(text, ENGINE).with_url("https://mymemory.translated.net/"))
}

// --------------------------------------------------------- currency conversion

/// A parsed `<amount> <FROM> to <TO>` currency-conversion request.
#[derive(Debug, Clone, PartialEq)]
pub struct CurrencyRequest {
    pub amount: f64,
    pub from: String,
    pub to: String,
}

/// ISO-4217 codes the FX backend (Frankfurter / ECB) supports. Used to gate
/// detection so we don't fire on phrases like "cat to dog".
const CURRENCIES: &[&str] = &[
    "AUD", "BGN", "BRL", "CAD", "CHF", "CNY", "CZK", "DKK", "EUR", "GBP", "HKD", "HUF", "IDR",
    "ILS", "INR", "ISK", "JPY", "KRW", "MXN", "MYR", "NOK", "NZD", "PHP", "PLN", "RON", "SEK",
    "SGD", "THB", "TRY", "USD", "ZAR",
];

fn known_currency(code: &str) -> Option<String> {
    let up = code.to_ascii_uppercase();
    CURRENCIES.contains(&up.as_str()).then_some(up)
}

/// Detect a currency-conversion query. Pure & offline.
///
/// Accepts e.g. `100 usd to eur`, `convert 50 gbp to jpy`, `5 usd in eur`,
/// `usd to eur` (amount defaults to 1).
pub fn currency_request(q: &str) -> Option<CurrencyRequest> {
    let lower = q.to_ascii_lowercase();
    let mut toks: Vec<&str> = lower.split_whitespace().collect();
    if toks.first() == Some(&"convert") {
        toks.remove(0);
    }
    // Locate the separator.
    let sep = toks
        .iter()
        .position(|&t| t == "to" || t == "in" || t == "into")?;
    if sep == 0 || sep + 1 >= toks.len() {
        return None;
    }
    let to = known_currency(toks[sep + 1])?;
    // Left side: either [amount, FROM] or [FROM] (or "100usd" stuck together).
    let left = &toks[..sep];
    let (amount, from) = match left {
        [a, c] => (a.parse::<f64>().ok()?, known_currency(c)?),
        [c] => {
            // Try a stuck-together "100usd" first, else bare "usd" (amount 1).
            if let Some(split) = c.find(|ch: char| ch.is_ascii_alphabetic()) {
                if split > 0 {
                    if let (Ok(a), Some(cur)) =
                        (c[..split].parse::<f64>(), known_currency(&c[split..]))
                    {
                        (a, cur)
                    } else {
                        (1.0, known_currency(c)?)
                    }
                } else {
                    (1.0, known_currency(c)?)
                }
            } else {
                (1.0, known_currency(c)?)
            }
        }
        _ => return None,
    };
    if !amount.is_finite() {
        return None;
    }
    Some(CurrencyRequest { amount, from, to })
}

async fn fetch_currency(
    client: &reqwest::Client,
    req: &CurrencyRequest,
    timeout: Duration,
) -> Option<Answer> {
    let resp = client
        .get("https://api.frankfurter.dev/v1/latest")
        .header("User-Agent", USER_AGENT)
        .query(&[("base", req.from.as_str()), ("symbols", req.to.as_str())])
        .timeout(timeout)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: Value = resp.json().await.ok()?;
    parse_currency(&body, req)
}

/// Build the conversion answer from a Frankfurter `/latest` response. Pure.
pub fn parse_currency(body: &Value, req: &CurrencyRequest) -> Option<Answer> {
    let rate = body["rates"][&req.to].as_f64()?;
    let converted = req.amount * rate;
    let date = body["date"].as_str().unwrap_or("");
    let mut text = format!(
        "{} {} = {} {}",
        fmt_num(req.amount),
        req.from,
        fmt_money(converted),
        req.to
    );
    if !date.is_empty() {
        text.push_str(&format!(
            "  (1 {} = {} {}, {})",
            req.from,
            fmt_num(rate),
            req.to,
            date
        ));
    }
    Some(Answer::new(text, ENGINE).with_url("https://www.frankfurter.dev/"))
}

// --------------------------------------------------------------- dictionary

/// Detect a "define X" style query and return the term to look up. Pure.
pub fn define_request(q: &str) -> Option<String> {
    let lower = q.to_ascii_lowercase();
    let lower = lower.trim();
    let term = if let Some(rest) = lower.strip_prefix("define ") {
        rest
    } else if let Some(rest) = lower.strip_prefix("definition of ") {
        rest
    } else if let Some(rest) = lower.strip_prefix("meaning of ") {
        rest
    } else if let Some(rest) = lower.strip_prefix("what does ") {
        rest.strip_suffix(" mean")
            .or_else(|| rest.strip_suffix(" mean?"))?
    } else if let Some(rest) = lower.strip_prefix("what is the definition of ") {
        rest
    } else {
        return None;
    };
    let term = term.trim().trim_end_matches('?').trim();
    // Dictionary lookups are single words; reject empty or multi-word phrases.
    if term.is_empty() || term.split_whitespace().count() != 1 {
        return None;
    }
    Some(term.to_string())
}

async fn fetch_definition(
    client: &reqwest::Client,
    term: &str,
    timeout: Duration,
) -> Option<Answer> {
    let url = format!(
        "https://api.dictionaryapi.dev/api/v2/entries/en/{}",
        urlencode(term)
    );
    let resp = client
        .get(&url)
        .header("User-Agent", USER_AGENT)
        .timeout(timeout)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: Value = resp.json().await.ok()?;
    parse_definition(&body, term)
}

/// Build a definition answer from a dictionaryapi.dev response. Pure.
pub fn parse_definition(body: &Value, term: &str) -> Option<Answer> {
    let entry = body.as_array()?.first()?;
    let word = entry["word"].as_str().unwrap_or(term);
    let phonetic = entry["phonetic"].as_str().unwrap_or("");
    let meanings = entry["meanings"].as_array()?;
    let mut parts = Vec::new();
    for meaning in meanings.iter().take(2) {
        let pos = meaning["partOfSpeech"].as_str().unwrap_or("");
        let def = meaning["definitions"][0]["definition"]
            .as_str()
            .unwrap_or("");
        if def.is_empty() {
            continue;
        }
        if pos.is_empty() {
            parts.push(def.to_string());
        } else {
            parts.push(format!("({pos}) {def}"));
        }
    }
    if parts.is_empty() {
        return None;
    }
    let header = if phonetic.is_empty() {
        word.to_string()
    } else {
        format!("{word} {phonetic}")
    };
    let text = format!("{header} — {}", parts.join("  ·  "));
    let url = entry["sourceUrls"][0]
        .as_str()
        .map(String::from)
        .unwrap_or_else(|| format!("https://en.wiktionary.org/wiki/{}", urlencode(word)));
    Some(Answer::new(text, ENGINE).with_url(url))
}

/// Percent-encode a term for use in a URL path segment.
fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// Format a monetary amount with two decimal places.
fn fmt_money(v: f64) -> String {
    format!("{v:.2}")
}

// ---------------------------------------------------------------- calculator

/// Evaluate an arithmetic expression. Pure & testable.
pub fn evaluate(expr: &str) -> Option<f64> {
    let tokens = tokenize(expr)?;
    let mut p = Parser { tokens, pos: 0 };
    let v = p.expr()?;
    if p.pos == p.tokens.len() {
        Some(v)
    } else {
        None
    }
}

fn calculator(q: &str) -> Option<Answer> {
    // Only treat as a calculation when it actually looks like math: must contain
    // a digit and (an operator or a known function/constant) to avoid hijacking
    // ordinary queries.
    let lower = q.to_ascii_lowercase();
    let has_op = lower.contains(['+', '*', '/', '%', '^', '(']) || lower.contains('-');
    let has_fn = ["sqrt", "sin", "cos", "tan", "log", "ln", "abs", "exp", "pi"]
        .iter()
        .any(|f| lower.contains(f));
    let has_digit = lower.chars().any(|c| c.is_ascii_digit());
    if !(has_digit && (has_op || has_fn)) {
        return None;
    }
    let value = evaluate(&lower)?;
    if !value.is_finite() {
        return None;
    }
    Some(Answer::new(
        format!("{} = {}", q.trim(), fmt_num(value)),
        ENGINE,
    ))
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Caret,
    LParen,
    RParen,
    Ident(String),
}

fn tokenize(input: &str) -> Option<Vec<Tok>> {
    let mut toks = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' => i += 1,
            '+' => {
                toks.push(Tok::Plus);
                i += 1;
            }
            '-' => {
                toks.push(Tok::Minus);
                i += 1;
            }
            '*' => {
                toks.push(Tok::Star);
                i += 1;
            }
            '/' => {
                toks.push(Tok::Slash);
                i += 1;
            }
            '%' => {
                toks.push(Tok::Percent);
                i += 1;
            }
            '^' => {
                toks.push(Tok::Caret);
                i += 1;
            }
            '(' => {
                toks.push(Tok::LParen);
                i += 1;
            }
            ')' => {
                toks.push(Tok::RParen);
                i += 1;
            }
            _ if c.is_ascii_digit() || c == '.' => {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                    i += 1;
                }
                let s: String = chars[start..i].iter().collect();
                toks.push(Tok::Num(s.parse().ok()?));
            }
            _ if c.is_ascii_alphabetic() => {
                let start = i;
                while i < chars.len() && chars[i].is_ascii_alphabetic() {
                    i += 1;
                }
                let s: String = chars[start..i].iter().collect();
                toks.push(Tok::Ident(s));
            }
            _ => return None,
        }
    }
    Some(toks)
}

struct Parser {
    tokens: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.pos)
    }

    fn expr(&mut self) -> Option<f64> {
        let mut v = self.term()?;
        while let Some(op) = self.peek() {
            match op {
                Tok::Plus => {
                    self.pos += 1;
                    v += self.term()?;
                }
                Tok::Minus => {
                    self.pos += 1;
                    v -= self.term()?;
                }
                _ => break,
            }
        }
        Some(v)
    }

    fn term(&mut self) -> Option<f64> {
        let mut v = self.power()?;
        while let Some(op) = self.peek() {
            match op {
                Tok::Star => {
                    self.pos += 1;
                    v *= self.power()?;
                }
                Tok::Slash => {
                    self.pos += 1;
                    v /= self.power()?;
                }
                Tok::Percent => {
                    self.pos += 1;
                    v %= self.power()?;
                }
                _ => break,
            }
        }
        Some(v)
    }

    fn power(&mut self) -> Option<f64> {
        let base = self.unary()?;
        if let Some(Tok::Caret) = self.peek() {
            self.pos += 1;
            let exp = self.power()?; // right-associative
            Some(base.powf(exp))
        } else {
            Some(base)
        }
    }

    fn unary(&mut self) -> Option<f64> {
        match self.peek() {
            Some(Tok::Minus) => {
                self.pos += 1;
                Some(-self.unary()?)
            }
            Some(Tok::Plus) => {
                self.pos += 1;
                self.unary()
            }
            _ => self.primary(),
        }
    }

    fn primary(&mut self) -> Option<f64> {
        match self.peek()?.clone() {
            Tok::Num(n) => {
                self.pos += 1;
                Some(n)
            }
            Tok::LParen => {
                self.pos += 1;
                let v = self.expr()?;
                if self.peek() == Some(&Tok::RParen) {
                    self.pos += 1;
                    Some(v)
                } else {
                    None
                }
            }
            Tok::Ident(name) => {
                self.pos += 1;
                match name.as_str() {
                    "pi" => Some(std::f64::consts::PI),
                    "e" => Some(std::f64::consts::E),
                    "tau" => Some(std::f64::consts::TAU),
                    fname => {
                        // function call: name '(' expr ')'
                        if self.peek() != Some(&Tok::LParen) {
                            return None;
                        }
                        self.pos += 1;
                        let arg = self.expr()?;
                        if self.peek() != Some(&Tok::RParen) {
                            return None;
                        }
                        self.pos += 1;
                        apply_fn(fname, arg)
                    }
                }
            }
            _ => None,
        }
    }
}

fn apply_fn(name: &str, x: f64) -> Option<f64> {
    Some(match name {
        "sqrt" => x.sqrt(),
        "sin" => x.sin(),
        "cos" => x.cos(),
        "tan" => x.tan(),
        "ln" => x.ln(),
        "log" => x.log10(),
        "abs" => x.abs(),
        "exp" => x.exp(),
        "floor" => x.floor(),
        "ceil" => x.ceil(),
        "round" => x.round(),
        _ => return None,
    })
}

// ----------------------------------------------------------- unit conversion

enum Dim {
    Linear(f64), // factor to base unit
    Temp(&'static str),
}

fn unit_factor(u: &str) -> Option<Dim> {
    use Dim::*;
    Some(match u {
        // length (base: metre)
        "m" | "meter" | "meters" | "metre" | "metres" => Linear(1.0),
        "km" | "kilometer" | "kilometers" | "kilometre" | "kilometres" => Linear(1000.0),
        "cm" | "centimeter" | "centimeters" => Linear(0.01),
        "mm" | "millimeter" | "millimeters" => Linear(0.001),
        "mi" | "mile" | "miles" => Linear(1609.344),
        "yd" | "yard" | "yards" => Linear(0.9144),
        "ft" | "foot" | "feet" => Linear(0.3048),
        "in" | "inch" | "inches" => Linear(0.0254),
        "nmi" => Linear(1852.0),
        // mass (base: gram)
        "g" | "gram" | "grams" => Linear(1.0),
        "kg" | "kilogram" | "kilograms" => Linear(1000.0),
        "mg" | "milligram" | "milligrams" => Linear(0.001),
        "lb" | "lbs" | "pound" | "pounds" => Linear(453.59237),
        "oz" | "ounce" | "ounces" => Linear(28.349523125),
        "st" | "stone" | "stones" => Linear(6350.29318),
        "t" | "tonne" | "tonnes" | "ton" => Linear(1_000_000.0),
        // temperature (special)
        "c" | "celsius" => Temp("c"),
        "f" | "fahrenheit" => Temp("f"),
        "k" | "kelvin" => Temp("k"),
        _ => return None,
    })
}

fn to_celsius(v: f64, unit: &str) -> f64 {
    match unit {
        "f" => (v - 32.0) * 5.0 / 9.0,
        "k" => v - 273.15,
        _ => v,
    }
}

fn from_celsius(c: f64, unit: &str) -> f64 {
    match unit {
        "f" => c * 9.0 / 5.0 + 32.0,
        "k" => c + 273.15,
        _ => c,
    }
}

fn unit_convert(q: &str) -> Option<Answer> {
    let lower = q.to_ascii_lowercase();
    let toks: Vec<&str> = lower.split_whitespace().collect();
    // Find the "to" / "in" separator.
    let sep = toks
        .iter()
        .position(|&t| t == "to" || t == "in" || t == "into")?;
    if sep == 0 || sep + 1 >= toks.len() {
        return None;
    }
    let to_unit = toks[sep + 1];
    // Left side: number then unit (possibly "10km" stuck together).
    let left: Vec<&str> = toks[..sep].to_vec();
    let (value, from_unit) = parse_value_unit(&left)?;

    let from = unit_factor(from_unit)?;
    let to = unit_factor(to_unit)?;
    let result = match (from, to) {
        (Dim::Linear(a), Dim::Linear(b)) => value * a / b,
        (Dim::Temp(a), Dim::Temp(b)) => from_celsius(to_celsius(value, a), b),
        _ => return None,
    };
    Some(Answer::new(
        format!(
            "{} {} = {} {}",
            fmt_num(value),
            from_unit,
            fmt_num(result),
            to_unit
        ),
        ENGINE,
    ))
}

fn parse_value_unit(tokens: &[&str]) -> Option<(f64, &'static str)> {
    // Either ["10", "km"] or ["10km"].
    if tokens.len() >= 2 {
        if let Ok(v) = tokens[0].parse::<f64>() {
            let u = canonical_unit(tokens[1])?;
            return Some((v, u));
        }
    }
    if tokens.len() == 1 {
        let t = tokens[0];
        let split = t.find(|c: char| c.is_ascii_alphabetic())?;
        let v: f64 = t[..split].parse().ok()?;
        let u = canonical_unit(&t[split..])?;
        return Some((v, u));
    }
    None
}

/// Resolve a unit token to its canonical &'static str key (validates it exists).
fn canonical_unit(u: &str) -> Option<&'static str> {
    const UNITS: &[&str] = &[
        "m",
        "meter",
        "meters",
        "metre",
        "metres",
        "km",
        "kilometer",
        "kilometers",
        "kilometre",
        "kilometres",
        "cm",
        "centimeter",
        "centimeters",
        "mm",
        "millimeter",
        "millimeters",
        "mi",
        "mile",
        "miles",
        "yd",
        "yard",
        "yards",
        "ft",
        "foot",
        "feet",
        "in",
        "inch",
        "inches",
        "nmi",
        "g",
        "gram",
        "grams",
        "kg",
        "kilogram",
        "kilograms",
        "mg",
        "milligram",
        "milligrams",
        "lb",
        "lbs",
        "pound",
        "pounds",
        "oz",
        "ounce",
        "ounces",
        "st",
        "stone",
        "stones",
        "t",
        "tonne",
        "tonnes",
        "ton",
        "c",
        "celsius",
        "f",
        "fahrenheit",
        "k",
        "kelvin",
    ];
    UNITS.iter().find(|&&k| k == u).copied()
}

// --------------------------------------------------------------- date / time

fn datetime(q: &str) -> Option<Answer> {
    let lower = q.to_ascii_lowercase();
    let lower = lower.trim();
    let matches = matches!(
        lower,
        "time"
            | "date"
            | "now"
            | "datetime"
            | "utc"
            | "utc time"
            | "time utc"
            | "current time"
            | "current date"
            | "what time is it"
            | "what is the time"
            | "what is the date"
            | "unix time"
            | "timestamp"
            | "epoch"
    );
    if !matches {
        return None;
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    if lower == "unix time" || lower == "timestamp" || lower == "epoch" {
        return Some(Answer::new(format!("Unix time: {secs}"), ENGINE));
    }
    let (y, mo, d, h, mi, s) = unix_to_utc(secs);
    Some(Answer::new(
        format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02} UTC"),
        ENGINE,
    ))
}

/// Convert a Unix timestamp (seconds) into a UTC civil datetime.
/// Uses Howard Hinnant's days-from-civil inverse algorithm.
pub fn unix_to_utc(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m as u32, d as u32, h as u32, mi as u32, s as u32)
}

// ------------------------------------------------------------------- random

fn random(q: &str) -> Option<Answer> {
    use rand::Rng;
    let lower = q.to_ascii_lowercase();
    let mut rng = rand::thread_rng();

    if lower.contains("coin") && (lower.contains("flip") || lower.contains("toss")) {
        let side = if rng.gen::<bool>() { "Heads" } else { "Tails" };
        return Some(Answer::new(format!("Coin flip: {side}"), ENGINE));
    }
    // Dice: "roll 2d6", "roll a dice", "roll d20"
    if lower.contains("roll") || lower.contains("dice") || lower.contains("die") {
        let (count, sides) = parse_dice(&lower).unwrap_or((1, 6));
        let total: u64 = (0..count).map(|_| rng.gen_range(1..=sides)).sum();
        return Some(Answer::new(
            format!("Rolled {count}d{sides}: {total}"),
            ENGINE,
        ));
    }
    if lower.starts_with("random") || lower.contains("random number") {
        let (lo, hi) = parse_range(&lower).unwrap_or((1, 100));
        let n = rng.gen_range(lo..=hi);
        return Some(Answer::new(format!("Random number: {n}"), ENGINE));
    }
    None
}

fn parse_dice(s: &str) -> Option<(u64, u64)> {
    // Find a token of the form NdM or dM.
    for tok in s.split_whitespace() {
        if let Some((a, b)) = tok.split_once('d') {
            let count = if a.is_empty() { 1 } else { a.parse().ok()? };
            let sides: u64 = b.parse().ok()?;
            if (1..=1000).contains(&count) && sides >= 2 {
                return Some((count, sides));
            }
        }
    }
    None
}

fn parse_range(s: &str) -> Option<(i64, i64)> {
    // "random 1-100" or "random between 1 and 100"
    let digits: Vec<i64> = s
        .replace('-', " ")
        .split_whitespace()
        .filter_map(|t| t.parse::<i64>().ok())
        .collect();
    if digits.len() >= 2 {
        let (a, b) = (digits[0], digits[1]);
        return Some((a.min(b), a.max(b)));
    }
    None
}

// --------------------------------------------------------------------- uuid

fn uuid(q: &str) -> Option<Answer> {
    let lower = q.trim().to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "uuid" | "uuidv4" | "uuid v4" | "guid" | "random uuid" | "generate uuid"
    ) {
        return Some(Answer::new(format!("UUID: {}", uuid_v4()), ENGINE));
    }
    None
}

/// Generate a random (v4) UUID string.
pub fn uuid_v4() -> String {
    use rand::Rng;
    let mut b = [0u8; 16];
    rand::thread_rng().fill(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

// -------------------------------------------------------------------- hash

fn hashing(q: &str) -> Option<Answer> {
    let (algo, rest) = q.split_once(char::is_whitespace)?;
    let rest = rest.trim();
    if rest.is_empty() {
        return None;
    }
    let digest = match algo.to_ascii_lowercase().as_str() {
        "sha256" => {
            let mut h = Sha256::new();
            h.update(rest.as_bytes());
            hex(&h.finalize())
        }
        "sha512" => {
            let mut h = Sha512::new();
            h.update(rest.as_bytes());
            hex(&h.finalize())
        }
        _ => return None,
    };
    Some(Answer::new(
        format!("{}: {digest}", algo.to_ascii_uppercase()),
        ENGINE,
    ))
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

// ------------------------------------------------------------------ helpers

/// Format a float without noisy trailing zeros.
fn fmt_num(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e15 {
        return format!("{}", v as i64);
    }
    let s = format!("{v:.6}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    s.to_string()
}

// ---------------------------------------------------------- SEC EDGAR filings

/// A parsed SEC filing request.
#[derive(Debug, Clone)]
pub struct FilingRequest {
    pub company: String,
    pub cik: Option<String>,
}

/// Detect SEC filing query. Triggers on "SEC filing", "공시", "10-K", "10-Q" etc.
pub fn filing_request(q: &str) -> Option<FilingRequest> {
    let lower = q.trim().to_lowercase();

    // Check for filing intent keywords
    let filing_keywords = [
        "sec filing", "sec filings", "10-k", "10-q", "8-k",
        "공시", "사업보고서", "분기보고서", "annual report",
    ];
    let has_filing_intent = filing_keywords.iter().any(|kw| lower.contains(kw));

    if !has_filing_intent {
        return None;
    }

    // Extract company name by removing keywords
    let mut company = lower.clone();
    for kw in &filing_keywords {
        company = company.replace(kw, "");
    }
    let company = company.trim().to_string();

    if company.is_empty() || company.len() < 2 {
        return None;
    }

    // Check if it's a known company with CIK
    let cik = SEC_COMPANIES.iter()
        .find(|(names, _)| names.iter().any(|n| company.contains(&n.to_lowercase())))
        .map(|(_, cik)| cik.to_string());

    Some(FilingRequest { company, cik })
}

/// Major companies and their SEC CIK numbers.
const SEC_COMPANIES: &[(&[&str], &str)] = &[
    (&["apple", "애플"], "0000320193"),
    (&["microsoft", "마이크로소프트", "ms"], "0000789019"),
    (&["google", "alphabet", "구글", "알파벳"], "0001652044"),
    (&["amazon", "아마존"], "0001018724"),
    (&["meta", "facebook", "메타", "페이스북"], "0001326801"),
    (&["tesla", "테슬라"], "0001318605"),
    (&["nvidia", "엔비디아"], "0001045810"),
    (&["netflix", "넷플릭스"], "0001065280"),
    (&["intel", "인텔"], "0000050863"),
    (&["amd"], "0000002488"),
    (&["walmart", "월마트"], "0000104169"),
    (&["coca-cola", "코카콜라"], "0000021344"),
    (&["disney", "디즈니"], "0001744489"),
    (&["nike", "나이키"], "0000320187"),
    (&["starbucks", "스타벅스"], "0000829224"),
];

async fn fetch_filing(
    client: &reqwest::Client,
    req: &FilingRequest,
    timeout: Duration,
) -> Option<Answer> {
    let cik = req.cik.as_ref()?;

    // SEC EDGAR submissions API (no key required)
    let url = format!("https://data.sec.gov/submissions/CIK{}.json", cik);
    let body: Value = client
        .get(&url)
        .header("User-Agent", "Orgos Search contact@orgos.cc")
        .timeout(timeout)
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;

    parse_filing(&body, req)
}

fn parse_filing(body: &Value, req: &FilingRequest) -> Option<Answer> {
    let name = body.get("name")?.as_str()?;
    let filings = body.get("filings")?.get("recent")?;
    let forms = filings.get("form")?.as_array()?;
    let dates = filings.get("filingDate")?.as_array()?;
    let accessions = filings.get("accessionNumber")?.as_array()?;
    let descriptions = filings.get("primaryDocument")?.as_array()?;

    let mut answer = format!("📋 {} SEC Filings\n\n", name);
    let mut count = 0;

    for i in 0..forms.len().min(5) {
        let form = forms.get(i)?.as_str()?;
        let date = dates.get(i)?.as_str()?;
        let accession = accessions.get(i)?.as_str()?;
        let _doc = descriptions.get(i).and_then(|d| d.as_str()).unwrap_or("");

        answer.push_str(&format!("• {} ({}) - {}\n", form, date, accession.replace("-", "")));
        count += 1;
    }

    if count == 0 {
        return None;
    }

    let cik = req.cik.as_ref()?;
    let url = format!("https://www.sec.gov/cgi-bin/browse-edgar?action=getcompany&CIK={}&type=&dateb=&owner=include&count=40", cik);

    Some(Answer::new(answer.trim(), ENGINE).with_url(&url))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calculator_basic() {
        assert_eq!(evaluate("2 + 3 * 4"), Some(14.0));
        assert_eq!(evaluate("(2 + 3) * 4"), Some(20.0));
        assert_eq!(evaluate("2 ^ 10"), Some(1024.0));
        assert_eq!(evaluate("10 % 3"), Some(1.0));
        assert_eq!(evaluate("-5 + 2"), Some(-3.0));
    }

    #[test]
    fn calculator_functions() {
        assert_eq!(evaluate("sqrt(16)"), Some(4.0));
        assert!((evaluate("sin(pi/2)").unwrap() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn calculator_answer_only_for_math() {
        assert!(calculator("how to bake bread").is_none());
        assert!(calculator("12 * 12").is_some());
        assert!(calculator("2024").is_none()); // bare number, not a calc
    }

    #[test]
    fn unit_length() {
        let a = unit_convert("10 km to miles").unwrap();
        assert!(a.answer.contains("6.21"));
    }

    #[test]
    fn unit_temperature() {
        let a = unit_convert("100 c to f").unwrap();
        assert!(a.answer.contains("212"));
    }

    #[test]
    fn unit_stuck_together() {
        let a = unit_convert("5kg in lb").unwrap();
        assert!(a.answer.contains("11.0"));
    }

    #[test]
    fn datetime_epoch_is_correct() {
        // 2001-09-09 01:46:40 UTC
        assert_eq!(unix_to_utc(1_000_000_000), (2001, 9, 9, 1, 46, 40));
        assert_eq!(unix_to_utc(0), (1970, 1, 1, 0, 0, 0));
    }

    #[test]
    fn uuid_shape_is_v4() {
        let u = uuid_v4();
        assert_eq!(u.len(), 36);
        assert_eq!(u.as_bytes()[14], b'4'); // version nibble
    }

    #[test]
    fn hash_known_vector() {
        let a = hashing("sha256 abc").unwrap();
        assert!(a
            .answer
            .contains("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"));
    }

    #[test]
    fn dice_and_range_parse() {
        assert_eq!(parse_dice("roll 2d6"), Some((2, 6)));
        assert_eq!(parse_range("random 1-100"), Some((1, 100)));
        assert_eq!(parse_range("random between 5 and 2"), Some((2, 5)));
    }

    #[test]
    fn currency_request_parses() {
        let r = currency_request("100 usd to eur").unwrap();
        assert_eq!(r.amount, 100.0);
        assert_eq!(r.from, "USD");
        assert_eq!(r.to, "EUR");
        let r = currency_request("convert 50 gbp into jpy").unwrap();
        assert_eq!(
            (r.amount, r.from.as_str(), r.to.as_str()),
            (50.0, "GBP", "JPY")
        );
        // bare codes default to amount 1
        assert_eq!(currency_request("usd to eur").unwrap().amount, 1.0);
        // stuck together
        assert_eq!(currency_request("5usd in eur").unwrap().amount, 5.0);
    }

    #[test]
    fn currency_request_rejects_non_currency() {
        // "cat"/"dog" are 3 letters but not currencies.
        assert!(currency_request("cat to dog").is_none());
        assert!(currency_request("how to cook eggs").is_none());
        assert!(currency_request("usd").is_none());
    }

    #[test]
    fn currency_parse_fixture() {
        let body: Value =
            serde_json::from_str(include_str!("../tests/fixtures/frankfurter.json")).unwrap();
        let req = CurrencyRequest {
            amount: 100.0,
            from: "USD".into(),
            to: "EUR".into(),
        };
        let a = parse_currency(&body, &req).unwrap();
        assert!(a.answer.contains("100 USD"));
        assert!(a.answer.contains("85.88 EUR"));
        assert!(a.answer.contains("2026-05-29"));
    }

    #[test]
    fn define_request_parses() {
        assert_eq!(
            define_request("define ostensible").as_deref(),
            Some("ostensible")
        );
        assert_eq!(
            define_request("definition of serendipity").as_deref(),
            Some("serendipity")
        );
        assert_eq!(
            define_request("what does ephemeral mean").as_deref(),
            Some("ephemeral")
        );
        assert_eq!(
            define_request("meaning of zeitgeist?").as_deref(),
            Some("zeitgeist")
        );
        // Multi-word phrases and non-define queries are rejected.
        assert!(define_request("define the universe and everything").is_none());
        assert!(define_request("best pizza in town").is_none());
    }

    #[test]
    fn weather_request_parses() {
        assert_eq!(
            weather_request("weather in London").as_deref(),
            Some("london")
        );
        assert_eq!(weather_request("london weather").as_deref(), Some("london"));
        assert_eq!(
            weather_request("forecast for Paris").as_deref(),
            Some("paris")
        );
        assert_eq!(
            weather_request("temperature in tokyo").as_deref(),
            Some("tokyo")
        );
        assert!(weather_request("how to weatherproof a deck").is_none());
        assert!(weather_request("weather").is_none());
    }

    #[test]
    fn weather_parse_fixtures() {
        let geo: Value =
            serde_json::from_str(include_str!("../tests/fixtures/open_meteo_geocode.json"))
                .unwrap();
        let (lat, lon, label) = parse_geocode(&geo).unwrap();
        assert!((lat - 51.50853).abs() < 1e-4);
        assert!((lon + 0.12574).abs() < 1e-4);
        assert_eq!(label, "London, United Kingdom");

        let wx: Value =
            serde_json::from_str(include_str!("../tests/fixtures/open_meteo_weather.json"))
                .unwrap();
        let a = parse_weather(&wx, &label).unwrap();
        assert!(a.answer.contains("London, United Kingdom"));
        assert!(a.answer.contains("14.2"));
        assert!(a.answer.contains("🌧"), "weather_code 61 renders the rain icon");
        assert!(a.answer.contains("18.5"));
    }

    #[test]
    fn translate_request_parses() {
        let r = translate_request("translate hello world to french").unwrap();
        assert_eq!(r.text, "hello world");
        assert_eq!(r.target, "fr");
        let r = translate_request("translate \"good morning\" to es").unwrap();
        assert_eq!(r.text, "good morning");
        assert_eq!(r.target, "es");
        assert!(translate_request("translate this to klingon").is_none());
        assert!(translate_request("best translation services").is_none());
    }

    #[test]
    fn translation_parse_fixture() {
        let body: Value =
            serde_json::from_str(include_str!("../tests/fixtures/mymemory.json")).unwrap();
        let req = TranslateRequest {
            text: "hello world".into(),
            target: "fr".into(),
        };
        let a = parse_translation(&body, &req).unwrap();
        assert!(a.answer.contains("Bonjour le monde"));
        assert!(a.answer.contains("FR"));
    }

    #[test]
    fn definition_parse_fixture() {
        let body: Value =
            serde_json::from_str(include_str!("../tests/fixtures/dictionary.json")).unwrap();
        let a = parse_definition(&body, "ostensible").unwrap();
        assert!(a.answer.contains("ostensible"));
        assert!(a.answer.contains("adjective"));
        assert!(a.answer.contains("Apparent"));
        assert_eq!(
            a.url.as_deref(),
            Some("https://en.wiktionary.org/wiki/ostensible")
        );
    }
}
