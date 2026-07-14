//! Trending entity detection and tracking.
//!
//! Ported from orgos-core internal/trending/trending.go

use std::collections::HashMap;
use regex::Regex;

/// Entity types for classification
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntityType {
    Person,
    Company,
    Product,
    Location,
    Event,
    Technology,
    Unknown,
}

impl EntityType {
    pub fn as_str(&self) -> &'static str {
        match self {
            EntityType::Person => "person",
            EntityType::Company => "company",
            EntityType::Product => "product",
            EntityType::Location => "location",
            EntityType::Event => "event",
            EntityType::Technology => "technology",
            EntityType::Unknown => "unknown",
        }
    }
}

/// Extracted entity
#[derive(Debug, Clone)]
pub struct Entity {
    pub text: String,
    pub entity_type: EntityType,
    pub score: f64,
}

/// Trending item with score
#[derive(Debug, Clone)]
pub struct TrendingItem {
    pub entity: String,
    pub entity_type: EntityType,
    pub current_count: u32,
    pub previous_count: u32,
    pub score: f64,
    pub samples: Vec<String>,
}

/// Pattern-based entity extractor (fast, no LLM needed)
/// Supports all major world scripts via Unicode ranges
pub struct EntityExtractor {
    /// Multi-script patterns for entity extraction
    patterns: Vec<(&'static str, Regex)>,
    /// Hashtags
    hashtag_pattern: Regex,
    /// Common stopwords to filter (all languages)
    stopwords: std::collections::HashSet<String>,
}

impl EntityExtractor {
    pub fn new() -> Self {
        let stopwords: std::collections::HashSet<String> = [
            // Korean
            "오늘", "내일", "어제", "이번", "지난", "다음", "올해", "작년", "내년",
            "종합", "속보", "단독", "긴급", "뉴스", "기사", "보도", "발표", "현황",
            "대한민국", "한국", "미국", "일본", "중국", "서울", "부산",
            // Japanese
            "ニュース", "速報", "最新", "今日", "明日", "昨日", "日本", "東京",
            // Chinese
            "新闻", "今天", "明天", "昨天", "中国", "北京", "上海",
            // English
            "The", "This", "That", "What", "When", "Where", "Why", "How",
            "News", "Update", "Breaking", "Report", "Today", "Tomorrow",
            "New", "Year", "Time", "Day", "Week", "Month",
            "January", "February", "March", "April", "May", "June",
            "July", "August", "September", "October", "November", "December",
            // German
            "Der", "Die", "Das", "Und", "Neue", "Zeit", "Jahr", "Tag",
            // French
            "Les", "Des", "Une", "Pour", "Dans", "Avec", "Sur",
            // Spanish
            "Los", "Las", "Del", "Para", "Con", "Por", "Una",
            // Russian
            "Что", "Это", "Как", "Для", "При", "Все",
            // Arabic common
            "في", "من", "على", "إلى", "عن", "مع",
            // Hindi common
            "और", "के", "में", "से", "को", "है",
        ].iter().map(|s| s.to_string()).collect();

        // Build patterns for all major scripts
        let patterns: Vec<(&'static str, Regex)> = vec![
            // East Asian
            ("korean", Regex::new(r"[가-힣]{2,8}").unwrap()),
            ("japanese", Regex::new(r"[一-鿿]{2,8}|[ぁ-ゟ]{2,10}|[゠-ヿ]{2,10}").unwrap()),
            ("chinese", Regex::new(r"[一-鿿]{2,8}").unwrap()),

            // South Asian
            ("devanagari", Regex::new(r"[ऀ-ॿ]{2,15}").unwrap()),     // Hindi, Sanskrit, Marathi
            ("bengali", Regex::new(r"[ঀ-৿]{2,15}").unwrap()),        // Bengali, Assamese
            ("gurmukhi", Regex::new(r"[਀-੿]{2,15}").unwrap()),       // Punjabi
            ("gujarati", Regex::new(r"[઀-૿]{2,15}").unwrap()),       // Gujarati
            ("oriya", Regex::new(r"[଀-୿]{2,15}").unwrap()),          // Odia
            ("tamil", Regex::new(r"[஀-௿]{2,15}").unwrap()),          // Tamil
            ("telugu", Regex::new(r"[ఀ-౿]{2,15}").unwrap()),         // Telugu
            ("kannada", Regex::new(r"[ಀ-೿]{2,15}").unwrap()),        // Kannada
            ("malayalam", Regex::new(r"[ഀ-ൿ]{2,15}").unwrap()),      // Malayalam
            ("sinhala", Regex::new(r"[඀-෿]{2,15}").unwrap()),        // Sinhala

            // Southeast Asian
            ("thai", Regex::new(r"[ก-๛]{2,20}").unwrap()),           // Thai
            ("lao", Regex::new(r"[ກ-ໝ]{2,20}").unwrap()),            // Lao
            ("myanmar", Regex::new(r"[က-ၙ]{2,15}").unwrap()),        // Myanmar/Burmese
            ("khmer", Regex::new(r"[ក-៹]{2,15}").unwrap()),          // Khmer/Cambodian
            ("vietnamese", Regex::new(r"\b[A-ZÀ-Ỹ][a-zà-ỹ]{2,15}\b").unwrap()), // Vietnamese (Latin + diacritics)

            // Middle Eastern
            ("arabic", Regex::new(r"[؀-ۿ]{2,20}").unwrap()),         // Arabic, Persian, Urdu
            ("hebrew", Regex::new(r"[֐-׿]{2,15}").unwrap()),         // Hebrew

            // European
            ("cyrillic", Regex::new(r"[Ѐ-ӿ]{2,15}").unwrap()),       // Russian, Ukrainian, Bulgarian, etc.
            ("greek", Regex::new(r"[Ͱ-Ͽ]{2,15}").unwrap()),          // Greek
            ("armenian", Regex::new(r"[Ա-֏]{2,15}").unwrap()),       // Armenian
            ("georgian", Regex::new(r"[Ⴀ-ჿ]{2,15}").unwrap()),       // Georgian

            // African
            ("ethiopic", Regex::new(r"[ሀ-᎙]{2,15}").unwrap()),       // Amharic, Tigrinya, etc.

            // Latin-based (capitalized words for European languages)
            ("latin", Regex::new(r"\b[A-ZÀ-ÖØ-Þ][a-zà-öø-ÿ]{2,20}\b").unwrap()),
        ];

        Self {
            patterns,
            hashtag_pattern: Regex::new(r"#[\w-￿]+").unwrap(),
            stopwords,
        }
    }

    /// Extract entities from text using all script patterns
    pub fn extract(&self, text: &str) -> Vec<Entity> {
        let mut entities = Vec::new();
        let mut seen = std::collections::HashSet::new();

        // Extract using all patterns
        for (_name, pattern) in &self.patterns {
            for cap in pattern.find_iter(text) {
                let s = cap.as_str().to_string();
                if s.len() >= 2 && !self.stopwords.contains(&s) && !seen.contains(&s) {
                    seen.insert(s.clone());
                    entities.push(Entity {
                        text: s,
                        entity_type: EntityType::Unknown,
                        score: 1.0,
                    });
                }
            }
        }

        // Extract hashtags
        for cap in self.hashtag_pattern.find_iter(text) {
            let s = cap.as_str().to_string();
            if !seen.contains(&s) {
                seen.insert(s.clone());
                entities.push(Entity {
                    text: s,
                    entity_type: EntityType::Unknown,
                    score: 0.8,
                });
            }
        }

        entities
    }
}

impl Default for EntityExtractor {
    fn default() -> Self {
        Self::new()
    }
}

/// Trending calculator
#[allow(dead_code)]
pub struct TrendingCalculator {
    /// Entity counts for current window
    current_counts: HashMap<String, u32>,
    /// Entity counts for previous window
    previous_counts: HashMap<String, u32>,
    /// Sample documents per entity
    samples: HashMap<String, Vec<String>>,
    /// Window size in seconds
    window_secs: i64,
}

impl TrendingCalculator {
    pub fn new(window_hours: u32) -> Self {
        Self {
            current_counts: HashMap::new(),
            previous_counts: HashMap::new(),
            samples: HashMap::new(),
            window_secs: window_hours as i64 * 3600,
        }
    }

    /// Add entity count
    pub fn add(&mut self, entity: &str, count: u32, sample: Option<String>) {
        *self.current_counts.entry(entity.to_string()).or_insert(0) += count;
        if let Some(s) = sample {
            let samples = self.samples.entry(entity.to_string()).or_insert_with(Vec::new);
            if samples.len() < 3 {
                samples.push(s);
            }
        }
    }

    /// Calculate trending items
    pub fn calculate(&self, min_count: u32, min_growth: f64) -> Vec<TrendingItem> {
        let mut trending = Vec::new();

        for (entity, &current) in &self.current_counts {
            if current < min_count {
                continue;
            }

            let previous = self.previous_counts.get(entity).copied().unwrap_or(0);

            // Check growth rate
            let growth = if previous == 0 {
                current as f64
            } else {
                current as f64 / previous as f64
            };

            if growth < min_growth {
                continue;
            }

            // Calculate score: (current - previous) * log2(current)
            let diff = current.saturating_sub(previous) as f64;
            let score = diff * (current as f64).log2();

            trending.push(TrendingItem {
                entity: entity.clone(),
                entity_type: EntityType::Unknown,
                current_count: current,
                previous_count: previous,
                score,
                samples: self.samples.get(entity).cloned().unwrap_or_default(),
            });
        }

        // Sort by score descending
        trending.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

        trending
    }

    /// Get top trending
    pub fn top(&self, limit: usize) -> Vec<TrendingItem> {
        let mut items = self.calculate(3, 1.5);
        items.truncate(limit);
        items
    }
}

impl Default for TrendingCalculator {
    fn default() -> Self {
        Self::new(6) // 6 hour window
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_korean_extraction() {
        let extractor = EntityExtractor::new();
        let entities = extractor.extract("삼성전자가 새로운 스마트폰을 발표했다");
        let texts: Vec<_> = entities.iter().map(|e| e.text.as_str()).collect();
        assert!(texts.contains(&"삼성전자"));
        assert!(texts.contains(&"스마트폰"));
    }

    #[test]
    fn test_japanese_extraction() {
        let extractor = EntityExtractor::new();
        let entities = extractor.extract("東京オリンピックが開催される");
        let texts: Vec<_> = entities.iter().map(|e| e.text.as_str()).collect();
        assert!(texts.iter().any(|t| t.contains("東京") || t.contains("オリンピック")));
    }

    #[test]
    fn test_arabic_extraction() {
        let extractor = EntityExtractor::new();
        let entities = extractor.extract("الرئيس يلتقي بوزير الخارجية");
        assert!(!entities.is_empty());
    }

    #[test]
    fn test_hindi_extraction() {
        let extractor = EntityExtractor::new();
        let entities = extractor.extract("प्रधानमंत्री ने नई योजना की घोषणा की");
        assert!(!entities.is_empty());
    }
}
