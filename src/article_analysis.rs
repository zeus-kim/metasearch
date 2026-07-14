//! Rule-based article tone analysis without AI dependency.
//!
//! Analyzes text patterns to determine:
//! - Article type (news, analysis, opinion)
//! - Factual vs opinion indicators
//! - Emotional intensity
//! - Source citation quality
//! - Media source bias and reliability

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::OnceLock;

// ============== Media Bias Database ==============

/// Media source bias and reliability info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaSource {
    pub name: String,
    /// -2=far_left, -1=left, 0=center, 1=right, 2=far_right
    pub bias: Option<i8>,
    /// 1=major outlet, 2=established, 3=blog/indie, 4=social
    pub tier: u8,
    #[serde(default)]
    pub note: Option<String>,
}

/// Loaded media bias database
#[derive(Debug, Clone, Deserialize)]
struct MediaBiasDb {
    sources: HashMap<String, CountrySources>,
    default: HashMap<String, DefaultSource>,
}

#[derive(Debug, Clone, Deserialize)]
struct CountrySources {
    #[serde(default)]
    news: Vec<SourceEntry>,
    #[serde(default)]
    tech: Vec<SourceEntry>,
    #[serde(default)]
    youtube: Vec<YoutubeEntry>,
}

#[derive(Debug, Clone, Deserialize)]
struct SourceEntry {
    domain: String,
    name: String,
    bias: i8,
    tier: u8,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct YoutubeEntry {
    channel: String,
    bias: i8,
    tier: u8,
}

#[derive(Debug, Clone, Deserialize)]
struct DefaultSource {
    bias: Option<i8>,
    tier: u8,
}

static MEDIA_BIAS_DB: OnceLock<HashMap<String, MediaSource>> = OnceLock::new();

fn load_media_bias_db() -> HashMap<String, MediaSource> {
    let path = "locales/media_bias.json";
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };

    let db: MediaBiasDb = match serde_json::from_str(&content) {
        Ok(d) => d,
        Err(_) => return HashMap::new(),
    };

    let mut result = HashMap::new();

    for (_country, sources) in db.sources {
        for entry in sources.news {
            result.insert(entry.domain.clone(), MediaSource {
                name: entry.name,
                bias: Some(entry.bias),
                tier: entry.tier,
                note: entry.note,
            });
        }
        for entry in sources.tech {
            result.insert(entry.domain.clone(), MediaSource {
                name: entry.name,
                bias: Some(entry.bias),
                tier: entry.tier,
                note: None,
            });
        }
        for entry in sources.youtube {
            result.insert(format!("youtube:{}", entry.channel), MediaSource {
                name: entry.channel.clone(),
                bias: Some(entry.bias),
                tier: entry.tier,
                note: None,
            });
        }
    }

    result
}

fn get_media_db() -> &'static HashMap<String, MediaSource> {
    MEDIA_BIAS_DB.get_or_init(load_media_bias_db)
}

/// Extract domain from URL
fn extract_domain(url: &str) -> Option<String> {
    let url = url.trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_start_matches("www.");
    let domain = url.split('/').next()?;
    // Get last two parts (e.g., chosun.com from news.chosun.com)
    let parts: Vec<&str> = domain.split('.').collect();
    if parts.len() >= 2 {
        Some(format!("{}.{}", parts[parts.len() - 2], parts[parts.len() - 1]))
    } else {
        Some(domain.to_string())
    }
}

/// Look up media source info by URL
pub fn lookup_source(url: &str) -> Option<MediaSource> {
    let db = get_media_db();
    let domain = extract_domain(url)?;

    // Try exact match first
    if let Some(source) = db.get(&domain) {
        return Some(source.clone());
    }

    // Try with subdomains stripped
    let parts: Vec<&str> = domain.split('.').collect();
    for i in 0..parts.len().saturating_sub(1) {
        let subdomain = parts[i..].join(".");
        if let Some(source) = db.get(&subdomain) {
            return Some(source.clone());
        }
    }

    None
}

/// Look up YouTube channel bias
pub fn lookup_youtube_channel(channel_name: &str) -> Option<MediaSource> {
    let db = get_media_db();
    db.get(&format!("youtube:{}", channel_name)).cloned()
}

/// Get bias label
pub fn bias_label(bias: i8, lang: &str) -> &'static str {
    match (bias, lang) {
        (-2, "ko") => "극좌",
        (-1, "ko") => "진보",
        (0, "ko") => "중도",
        (1, "ko") => "보수",
        (2, "ko") => "극우",
        (-2, _) => "Far Left",
        (-1, _) => "Left",
        (0, _) => "Center",
        (1, _) => "Right",
        (2, _) => "Far Right",
        _ => "Unknown",
    }
}

/// Get tier label
pub fn tier_label(tier: u8, lang: &str) -> &'static str {
    match (tier, lang) {
        (1, "ko") => "주요 언론",
        (2, "ko") => "전문 매체",
        (3, "ko") => "블로그/독립",
        (4, "ko") => "SNS/커뮤니티",
        (1, _) => "Major Outlet",
        (2, _) => "Established",
        (3, _) => "Blog/Indie",
        (4, _) => "Social/Community",
        _ => "Unknown",
    }
}

// ============== Article Analysis ==============

/// Article analysis result based on text patterns
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArticleAnalysis {
    /// Article type detected from URL/content patterns
    pub article_type: ArticleType,
    /// 0-100: higher = more factual (quotes, citations, neutral language)
    pub factual_score: u8,
    /// 0-100: higher = more emotional (adjectives, exclamations)
    pub emotional_score: u8,
    /// 0-100: higher = more opinion-based (1st person, judgments)
    pub opinion_score: u8,
    /// Detailed indicators that contributed to scores
    pub indicators: AnalysisIndicators,
}

/// Detected article type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArticleType {
    /// Breaking/straight news - who, what, when, where
    StraightNews,
    /// Analysis/explainer - facts + interpretation
    Analysis,
    /// Opinion/column/editorial - subjective views
    Opinion,
    /// Interview - primarily quotes
    Interview,
    /// Feature/longform - narrative style
    Feature,
    /// Unknown/mixed
    Unknown,
}

impl ArticleType {
    pub fn label(&self, lang: &str) -> &'static str {
        match (self, lang) {
            (Self::StraightNews, "ko") => "📰 스트레이트",
            (Self::Analysis, "ko") => "🔍 분석",
            (Self::Opinion, "ko") => "💬 오피니언",
            (Self::Interview, "ko") => "🎤 인터뷰",
            (Self::Feature, "ko") => "📝 기획",
            (Self::Unknown, "ko") => "📄 기사",
            (Self::StraightNews, _) => "📰 News",
            (Self::Analysis, _) => "🔍 Analysis",
            (Self::Opinion, _) => "💬 Opinion",
            (Self::Interview, _) => "🎤 Interview",
            (Self::Feature, _) => "📝 Feature",
            (Self::Unknown, _) => "📄 Article",
        }
    }
}

/// Detailed analysis indicators
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AnalysisIndicators {
    /// Percentage of text in quotation marks
    pub quote_ratio: f32,
    /// Count of source citations ("according to", "~에 따르면")
    pub citation_count: u32,
    /// Count of emotional words
    pub emotional_word_count: u32,
    /// Count of 1st person pronouns
    pub first_person_count: u32,
    /// Count of assertive/definitive statements
    pub assertion_count: u32,
    /// Count of hedging/uncertain language
    pub hedge_count: u32,
    /// Exclamation mark count
    pub exclamation_count: u32,
    /// Question mark count (rhetorical questions in opinion)
    pub question_count: u32,
}

/// Analyze article from URL pattern
pub fn detect_type_from_url(url: &str) -> ArticleType {
    let url_lower = url.to_lowercase();

    // Opinion patterns
    if url_lower.contains("/opinion")
        || url_lower.contains("/column")
        || url_lower.contains("/editorial")
        || url_lower.contains("/commentary")
        || url_lower.contains("/viewpoint")
        || url_lower.contains("/사설")
        || url_lower.contains("/칼럼")
        || url_lower.contains("/기고")
    {
        return ArticleType::Opinion;
    }

    // Analysis patterns
    if url_lower.contains("/analysis")
        || url_lower.contains("/insight")
        || url_lower.contains("/explainer")
        || url_lower.contains("/in-depth")
        || url_lower.contains("/해설")
        || url_lower.contains("/분석")
    {
        return ArticleType::Analysis;
    }

    // Interview patterns
    if url_lower.contains("/interview")
        || url_lower.contains("/인터뷰")
        || url_lower.contains("/qa")
        || url_lower.contains("/q-and-a")
    {
        return ArticleType::Interview;
    }

    // Feature patterns
    if url_lower.contains("/feature")
        || url_lower.contains("/longread")
        || url_lower.contains("/기획")
        || url_lower.contains("/르포")
        || url_lower.contains("/특집")
    {
        return ArticleType::Feature;
    }

    // Breaking/straight news patterns
    if url_lower.contains("/breaking")
        || url_lower.contains("/news/")
        || url_lower.contains("/속보")
    {
        return ArticleType::StraightNews;
    }

    ArticleType::Unknown
}

/// Emotional/sensational words (Korean)
const EMOTIONAL_WORDS_KO: &[&str] = &[
    "충격", "경악", "폭로", "논란", "파문", "폭발", "대박", "헉", "깜짝",
    "분노", "격분", "통탄", "비통", "슬픔", "감동", "환희", "열광",
    "최악", "최고", "역대급", "초유", "전무후무", "사상초유",
    "급기야", "결국", "드디어", "마침내", "놀랍게도", "충격적으로",
    "경악스럽게", "믿기지않게", "어처구니없게", "황당하게",
];

/// Emotional/sensational words (English)
const EMOTIONAL_WORDS_EN: &[&str] = &[
    "shocking", "outrage", "scandal", "explosive", "bombshell", "stunning",
    "furious", "devastating", "incredible", "unbelievable", "horrific",
    "amazing", "terrible", "worst", "best", "historic", "unprecedented",
    "finally", "shockingly", "unbelievably", "incredibly", "disturbingly",
];

/// Source citation patterns (Korean)
const CITATION_PATTERNS_KO: &[&str] = &[
    "에 따르면", "라고 말했다", "라고 밝혔다", "라고 전했다",
    "라고 설명했다", "라고 강조했다", "라고 주장했다",
    "보도에 따르면", "발표에 따르면", "조사에 따르면",
    "관계자는", "대변인은", "측은",
];

/// Source citation patterns (English)
const CITATION_PATTERNS_EN: &[&str] = &[
    "according to", "said", "told", "reported", "announced",
    "stated", "explained", "claimed", "argued", "noted",
    "a spokesperson", "officials said", "sources say",
];

/// First person pronouns
const FIRST_PERSON_KO: &[&str] = &["나는", "내가", "저는", "제가", "우리는", "우리가"];
const FIRST_PERSON_EN: &[&str] = &["I ", "I'm", "I've", "my ", "we ", "our "];

/// Assertive/definitive expressions (Korean)
const ASSERTIONS_KO: &[&str] = &[
    "반드시", "틀림없이", "분명히", "확실히", "당연히", "명백히",
    "절대로", "결코", "무조건", "의심할 여지 없이",
    "~해야 한다", "~해서는 안 된다", "~일 수밖에 없다",
];

/// Assertive expressions (English)
const ASSERTIONS_EN: &[&str] = &[
    "must", "definitely", "certainly", "clearly", "obviously", "undoubtedly",
    "absolutely", "never", "always", "should", "have to", "need to",
];

/// Hedging/uncertain language (Korean)
const HEDGES_KO: &[&str] = &[
    "~로 보인다", "~로 보여", "~것으로 보인다", "~듯하다",
    "~할 수 있다", "~일 수 있다", "~할지도", "아마",
    "추정된다", "예상된다", "전망이다", "관측이다",
];

/// Hedging language (English)
const HEDGES_EN: &[&str] = &[
    "may", "might", "could", "possibly", "perhaps", "likely",
    "appears to", "seems to", "is expected", "is believed",
    "reportedly", "allegedly", "according to sources",
];

/// Count quoted text ratio (text inside "" or '')
fn quote_ratio(text: &str) -> f32 {
    let total_chars = text.chars().count();
    if total_chars == 0 {
        return 0.0;
    }

    let mut in_quote = false;
    let mut quote_chars = 0;
    let mut prev_char = ' ';

    for c in text.chars() {
        match c {
            '"' | '\u{201C}' | '\u{201D}' | '\'' | '\u{2018}' | '\u{2019}' | '「' | '」' | '『' | '』' => {
                in_quote = !in_quote;
            }
            _ if in_quote => {
                quote_chars += 1;
            }
            _ => {}
        }
        prev_char = c;
    }
    let _ = prev_char;

    (quote_chars as f32 / total_chars as f32) * 100.0
}

/// Count pattern occurrences in text
fn count_patterns(text: &str, patterns: &[&str]) -> u32 {
    let text_lower = text.to_lowercase();
    patterns.iter()
        .map(|p| text_lower.matches(&p.to_lowercase()).count() as u32)
        .sum()
}

/// Analyze article text and return detailed analysis
pub fn analyze_text(text: &str, url: &str) -> ArticleAnalysis {
    let text_len = text.len();
    if text_len < 50 {
        return ArticleAnalysis {
            article_type: detect_type_from_url(url),
            factual_score: 50,
            emotional_score: 0,
            opinion_score: 50,
            indicators: AnalysisIndicators::default(),
        };
    }

    // Detect language
    let is_korean = text.chars().any(|c| ('\u{AC00}'..='\u{D7AF}').contains(&c));

    // Calculate indicators
    let quote_r = quote_ratio(text);

    let citation_count = if is_korean {
        count_patterns(text, CITATION_PATTERNS_KO)
    } else {
        count_patterns(text, CITATION_PATTERNS_EN)
    };

    let emotional_count = if is_korean {
        count_patterns(text, EMOTIONAL_WORDS_KO)
    } else {
        count_patterns(text, EMOTIONAL_WORDS_EN)
    };

    let first_person_count = if is_korean {
        count_patterns(text, FIRST_PERSON_KO)
    } else {
        count_patterns(text, FIRST_PERSON_EN)
    };

    let assertion_count = if is_korean {
        count_patterns(text, ASSERTIONS_KO)
    } else {
        count_patterns(text, ASSERTIONS_EN)
    };

    let hedge_count = if is_korean {
        count_patterns(text, HEDGES_KO)
    } else {
        count_patterns(text, HEDGES_EN)
    };

    let exclamation_count = text.matches('!').count() as u32;
    let question_count = text.matches('?').count() as u32;

    let indicators = AnalysisIndicators {
        quote_ratio: quote_r,
        citation_count,
        emotional_word_count: emotional_count,
        first_person_count,
        assertion_count,
        hedge_count,
        exclamation_count,
        question_count,
    };

    // Calculate scores
    // Factual score: quotes + citations + hedges (uncertainty = careful reporting)
    let text_factor = (text_len as f32 / 1000.0).min(5.0); // normalize by ~1000 chars
    let factual_raw = (quote_r * 1.5)
        + (citation_count as f32 * 8.0 / text_factor)
        + (hedge_count as f32 * 3.0 / text_factor);
    let factual_score = (factual_raw.min(100.0)) as u8;

    // Emotional score: emotional words + exclamations
    let emotional_raw = (emotional_count as f32 * 10.0 / text_factor)
        + (exclamation_count as f32 * 5.0 / text_factor);
    let emotional_score = (emotional_raw.min(100.0)) as u8;

    // Opinion score: 1st person + assertions - hedges
    let opinion_raw = (first_person_count as f32 * 12.0 / text_factor)
        + (assertion_count as f32 * 8.0 / text_factor)
        + (question_count as f32 * 2.0 / text_factor) // rhetorical questions
        - (hedge_count as f32 * 2.0 / text_factor);
    let opinion_score = (opinion_raw.max(0.0).min(100.0)) as u8;

    // Determine article type
    let url_type = detect_type_from_url(url);
    let article_type = if url_type != ArticleType::Unknown {
        url_type
    } else if quote_r > 30.0 && citation_count > 3 {
        ArticleType::Interview
    } else if opinion_score > 60 || first_person_count > 3 {
        ArticleType::Opinion
    } else if factual_score > 60 && emotional_score < 30 {
        ArticleType::StraightNews
    } else if citation_count > 2 && hedge_count > 2 {
        ArticleType::Analysis
    } else {
        ArticleType::Unknown
    };

    ArticleAnalysis {
        article_type,
        factual_score,
        emotional_score,
        opinion_score,
        indicators,
    }
}

/// Quick analysis returning just the main scores
pub fn quick_analyze(text: &str, url: &str) -> (ArticleType, u8, u8, u8) {
    let analysis = analyze_text(text, url);
    (analysis.article_type, analysis.factual_score, analysis.emotional_score, analysis.opinion_score)
}

/// Get a simple label for the article tone
pub fn tone_label(analysis: &ArticleAnalysis, lang: &str) -> &'static str {
    let f = analysis.factual_score;
    let e = analysis.emotional_score;
    let o = analysis.opinion_score;

    match lang {
        "ko" => {
            if f > 70 && e < 30 { "📊 팩트 중심" }
            else if o > 60 { "💭 의견 중심" }
            else if e > 50 { "🔥 감성적" }
            else if f > 50 { "📰 균형적" }
            else { "📄 일반" }
        }
        _ => {
            if f > 70 && e < 30 { "📊 Factual" }
            else if o > 60 { "💭 Opinion-based" }
            else if e > 50 { "🔥 Emotional" }
            else if f > 50 { "📰 Balanced" }
            else { "📄 General" }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_url_detection() {
        assert_eq!(detect_type_from_url("https://example.com/opinion/article"), ArticleType::Opinion);
        assert_eq!(detect_type_from_url("https://example.com/news/breaking"), ArticleType::StraightNews);
        assert_eq!(detect_type_from_url("https://example.com/analysis/deep-dive"), ArticleType::Analysis);
    }

    #[test]
    fn test_quote_ratio() {
        let text = r#"He said "this is important" and she replied "I agree"."#;
        let ratio = quote_ratio(text);
        assert!(ratio > 30.0 && ratio < 60.0);
    }

    #[test]
    fn test_korean_analysis() {
        let text = "정부 관계자는 \"이번 정책은 반드시 성공할 것\"이라고 밝혔다. 전문가에 따르면 이는 충격적인 결정이다.";
        let analysis = analyze_text(text, "");
        assert!(analysis.indicators.citation_count > 0);
        assert!(analysis.indicators.emotional_word_count > 0);
    }

    #[test]
    fn test_opinion_detection() {
        let text = "I think this is absolutely wrong. We must change this policy. This is definitely the worst decision ever made.";
        let analysis = analyze_text(text, "");
        assert!(analysis.opinion_score > 30);
        assert!(analysis.indicators.first_person_count > 0);
        assert!(analysis.indicators.assertion_count > 0);
    }
}
