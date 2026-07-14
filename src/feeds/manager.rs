//! Feed lifecycle manager - handles feed states, quality scoring, and polling tiers.
//!
//! Ported from orgos-core internal/feeds/manager.go

use rusqlite::{Connection, params};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Feed status in the lifecycle
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedStatus {
    Candidate,   // Newly discovered, not yet validated
    Active,      // Validated and being polled
    Stale,       // No new content for a while
    Failing,     // Repeated fetch failures
    Quarantined, // Temporarily disabled
    Dead,        // Permanently failed
    Blocked,     // Manually blocked
}

impl FeedStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            FeedStatus::Candidate => "candidate",
            FeedStatus::Active => "active",
            FeedStatus::Stale => "stale",
            FeedStatus::Failing => "failing",
            FeedStatus::Quarantined => "quarantined",
            FeedStatus::Dead => "dead",
            FeedStatus::Blocked => "blocked",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "active" => FeedStatus::Active,
            "stale" => FeedStatus::Stale,
            "failing" => FeedStatus::Failing,
            "quarantined" => FeedStatus::Quarantined,
            "dead" => FeedStatus::Dead,
            "blocked" => FeedStatus::Blocked,
            _ => FeedStatus::Candidate,
        }
    }
}

/// Polling tier determines fetch frequency
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollTier {
    Hot,        // 15 minutes - high quality active feeds
    Warm,       // 2 hours
    Cold,       // 6 hours
    Quarantine, // 1 day - problematic feeds
}

impl PollTier {
    pub fn interval_secs(&self) -> u64 {
        match self {
            PollTier::Hot => 15 * 60,
            PollTier::Warm => 2 * 60 * 60,
            PollTier::Cold => 6 * 60 * 60,
            PollTier::Quarantine => 24 * 60 * 60,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            PollTier::Hot => "hot",
            PollTier::Warm => "warm",
            PollTier::Cold => "cold",
            PollTier::Quarantine => "quarantine",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "hot" => PollTier::Hot,
            "warm" => PollTier::Warm,
            "cold" => PollTier::Cold,
            _ => PollTier::Quarantine,
        }
    }
}

/// Quality score components
#[derive(Debug, Clone, Default)]
pub struct QualityScore {
    pub availability: f64,  // 0-100: uptime percentage
    pub freshness: f64,     // 0-100: how often new content
    pub content: f64,       // 0-100: content quality
    pub trust: f64,         // 0-100: domain trust
}

impl QualityScore {
    pub fn total(&self) -> f64 {
        (self.availability * 0.3 + self.freshness * 0.3 + self.content * 0.25 + self.trust * 0.15)
            .clamp(0.0, 100.0)
    }
}

/// Managed feed entry
#[derive(Debug, Clone)]
pub struct ManagedFeed {
    pub id: i64,
    pub url: String,
    pub canonical_url: String,
    pub domain: String,
    pub title: Option<String>,
    pub language: String,
    pub country: String,
    pub category: String,
    pub status: FeedStatus,
    pub poll_tier: PollTier,
    pub quality_score: f64,
    pub last_polled_at: Option<i64>,
    pub last_success_at: Option<i64>,
    pub consecutive_failures: u32,
    pub total_items: u32,
    pub created_at: i64,
}

/// Feed manager handles feed lifecycle
pub struct FeedManager {
    db: Arc<Mutex<Connection>>,
}

impl FeedManager {
    /// Create new feed manager with SQLite database
    pub fn new(db_path: &str) -> Result<Self, rusqlite::Error> {
        let conn = Connection::open(db_path)?;

        conn.execute_batch(r#"
            CREATE TABLE IF NOT EXISTS managed_feeds (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                url TEXT UNIQUE NOT NULL,
                canonical_url TEXT NOT NULL,
                domain TEXT NOT NULL,
                title TEXT,
                language TEXT DEFAULT 'unknown',
                country TEXT DEFAULT 'unknown',
                category TEXT DEFAULT 'news',
                status TEXT DEFAULT 'candidate',
                poll_tier TEXT DEFAULT 'cold',
                quality_score REAL DEFAULT 50.0,
                last_polled_at INTEGER,
                last_success_at INTEGER,
                consecutive_failures INTEGER DEFAULT 0,
                total_items INTEGER DEFAULT 0,
                created_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_mf_status ON managed_feeds(status);
            CREATE INDEX IF NOT EXISTS idx_mf_lang ON managed_feeds(language);
            CREATE INDEX IF NOT EXISTS idx_mf_tier ON managed_feeds(poll_tier);
            CREATE INDEX IF NOT EXISTS idx_mf_domain ON managed_feeds(domain);

            CREATE TABLE IF NOT EXISTS documents (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                url TEXT UNIQUE NOT NULL,
                feed_id INTEGER,
                title TEXT NOT NULL,
                summary TEXT,
                content TEXT,
                image_url TEXT,
                language TEXT,
                country TEXT,
                category TEXT,
                published_at INTEGER,
                indexed_at INTEGER NOT NULL,
                FOREIGN KEY (feed_id) REFERENCES managed_feeds(id)
            );

            CREATE INDEX IF NOT EXISTS idx_doc_feed ON documents(feed_id);
            CREATE INDEX IF NOT EXISTS idx_doc_lang ON documents(language);
            CREATE INDEX IF NOT EXISTS idx_doc_pub ON documents(published_at);

            CREATE VIRTUAL TABLE IF NOT EXISTS documents_fts USING fts5(
                title, summary, content,
                tokenize='trigram'
            );

            CREATE TABLE IF NOT EXISTS document_entities (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                document_id INTEGER NOT NULL,
                entity_text TEXT NOT NULL,
                entity_type TEXT NOT NULL,
                score REAL DEFAULT 1.0,
                FOREIGN KEY (document_id) REFERENCES documents(id)
            );

            CREATE INDEX IF NOT EXISTS idx_de_doc ON document_entities(document_id);
            CREATE INDEX IF NOT EXISTS idx_de_entity ON document_entities(entity_text);
        "#)?;

        Ok(Self {
            db: Arc::new(Mutex::new(conn)),
        })
    }

    /// Normalize feed URL
    pub fn normalize_url(url: &str) -> String {
        let mut normalized = url.trim().to_string();

        // Remove trailing slashes
        while normalized.ends_with('/') {
            normalized.pop();
        }

        // Remove common tracking params
        if let Some(pos) = normalized.find('?') {
            let base = &normalized[..pos];
            let query = &normalized[pos + 1..];
            let clean_params: Vec<&str> = query
                .split('&')
                .filter(|p| {
                    !p.starts_with("utm_") &&
                    !p.starts_with("ref=") &&
                    !p.starts_with("source=")
                })
                .collect();
            if clean_params.is_empty() {
                normalized = base.to_string();
            } else {
                normalized = format!("{}?{}", base, clean_params.join("&"));
            }
        }

        normalized
    }

    /// Extract domain from URL
    pub fn extract_domain(url: &str) -> String {
        url.split("://")
            .nth(1)
            .and_then(|s| s.split('/').next())
            .unwrap_or("")
            .to_string()
    }

    /// Add new feed candidate
    pub fn add_feed(&self, url: &str, language: &str, category: &str) -> Result<i64, rusqlite::Error> {
        let canonical = Self::normalize_url(url);
        let domain = Self::extract_domain(url);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let db = self.db.lock().unwrap();
        db.execute(
            "INSERT OR IGNORE INTO managed_feeds
             (url, canonical_url, domain, language, category, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![url, canonical, domain, language, category, now],
        )?;

        Ok(db.last_insert_rowid())
    }

    /// Get feeds due for polling
    pub fn get_due_feeds(&self, limit: usize) -> Vec<ManagedFeed> {
        let db = self.db.lock().unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let mut stmt = match db.prepare(
            "SELECT id, url, canonical_url, domain, title, language, country, category,
                    status, poll_tier, quality_score, last_polled_at, last_success_at,
                    consecutive_failures, total_items, created_at
             FROM managed_feeds
             WHERE status IN ('active', 'candidate')
             AND (last_polled_at IS NULL OR
                  last_polled_at < ?1 - CASE poll_tier
                      WHEN 'hot' THEN 900
                      WHEN 'warm' THEN 7200
                      WHEN 'cold' THEN 21600
                      ELSE 86400
                  END)
             ORDER BY quality_score DESC, last_polled_at ASC
             LIMIT ?2"
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };

        stmt.query_map(params![now, limit as i64], |row| {
            Ok(ManagedFeed {
                id: row.get(0)?,
                url: row.get(1)?,
                canonical_url: row.get(2)?,
                domain: row.get(3)?,
                title: row.get(4)?,
                language: row.get(5)?,
                country: row.get(6)?,
                category: row.get(7)?,
                status: FeedStatus::from_str(&row.get::<_, String>(8)?),
                poll_tier: PollTier::from_str(&row.get::<_, String>(9)?),
                quality_score: row.get(10)?,
                last_polled_at: row.get(11)?,
                last_success_at: row.get(12)?,
                consecutive_failures: row.get(13)?,
                total_items: row.get(14)?,
                created_at: row.get(15)?,
            })
        })
        .ok()
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    /// Update feed after successful poll
    pub fn mark_success(&self, feed_id: i64, items_added: u32) -> Result<(), rusqlite::Error> {
        let db = self.db.lock().unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        db.execute(
            "UPDATE managed_feeds SET
             status = 'active',
             last_polled_at = ?1,
             last_success_at = ?1,
             consecutive_failures = 0,
             total_items = total_items + ?2,
             poll_tier = CASE
                 WHEN quality_score >= 80 THEN 'hot'
                 WHEN quality_score >= 50 THEN 'warm'
                 ELSE 'cold'
             END
             WHERE id = ?3",
            params![now, items_added, feed_id],
        )?;
        Ok(())
    }

    /// Update feed after failed poll
    pub fn mark_failure(&self, feed_id: i64) -> Result<(), rusqlite::Error> {
        let db = self.db.lock().unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        db.execute(
            "UPDATE managed_feeds SET
             last_polled_at = ?1,
             consecutive_failures = consecutive_failures + 1,
             status = CASE
                 WHEN consecutive_failures >= 10 THEN 'dead'
                 WHEN consecutive_failures >= 5 THEN 'quarantined'
                 WHEN consecutive_failures >= 3 THEN 'failing'
                 ELSE status
             END,
             poll_tier = CASE
                 WHEN consecutive_failures >= 5 THEN 'quarantine'
                 ELSE poll_tier
             END
             WHERE id = ?2",
            params![now, feed_id],
        )?;
        Ok(())
    }

    /// Update quality score
    pub fn update_quality(&self, feed_id: i64, score: &QualityScore) -> Result<(), rusqlite::Error> {
        let db = self.db.lock().unwrap();
        db.execute(
            "UPDATE managed_feeds SET quality_score = ?1 WHERE id = ?2",
            params![score.total(), feed_id],
        )?;
        Ok(())
    }

    /// Get feeds by language
    pub fn get_by_language(&self, lang: &str, limit: usize) -> Vec<ManagedFeed> {
        let db = self.db.lock().unwrap();
        let mut stmt = match db.prepare(
            "SELECT id, url, canonical_url, domain, title, language, country, category,
                    status, poll_tier, quality_score, last_polled_at, last_success_at,
                    consecutive_failures, total_items, created_at
             FROM managed_feeds
             WHERE language = ?1 AND status = 'active'
             ORDER BY quality_score DESC
             LIMIT ?2"
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };

        stmt.query_map(params![lang, limit as i64], |row| {
            Ok(ManagedFeed {
                id: row.get(0)?,
                url: row.get(1)?,
                canonical_url: row.get(2)?,
                domain: row.get(3)?,
                title: row.get(4)?,
                language: row.get(5)?,
                country: row.get(6)?,
                category: row.get(7)?,
                status: FeedStatus::from_str(&row.get::<_, String>(8)?),
                poll_tier: PollTier::from_str(&row.get::<_, String>(9)?),
                quality_score: row.get(10)?,
                last_polled_at: row.get(11)?,
                last_success_at: row.get(12)?,
                consecutive_failures: row.get(13)?,
                total_items: row.get(14)?,
                created_at: row.get(15)?,
            })
        })
        .ok()
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    /// Import feeds from registry
    pub fn import_registry(&self, registry_path: &str) -> Result<usize, Box<dyn std::error::Error>> {
        let data = std::fs::read_to_string(registry_path)?;
        let mut count = 0;

        for line in data.lines() {
            if let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) {
                let url = entry["url"].as_str().unwrap_or("");
                let lang = entry["lang"].as_str().unwrap_or("unknown");
                if !url.is_empty() {
                    if self.add_feed(url, lang, "news").is_ok() {
                        count += 1;
                    }
                }
            }
        }

        Ok(count)
    }

    /// Get statistics
    pub fn stats(&self) -> FeedManagerStats {
        let db = self.db.lock().unwrap();

        let total: i64 = db.query_row(
            "SELECT COUNT(*) FROM managed_feeds", [], |r| r.get(0)
        ).unwrap_or(0);

        let active: i64 = db.query_row(
            "SELECT COUNT(*) FROM managed_feeds WHERE status = 'active'", [], |r| r.get(0)
        ).unwrap_or(0);

        let documents: i64 = db.query_row(
            "SELECT COUNT(*) FROM documents", [], |r| r.get(0)
        ).unwrap_or(0);

        FeedManagerStats {
            total_feeds: total as usize,
            active_feeds: active as usize,
            total_documents: documents as usize,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FeedManagerStats {
    pub total_feeds: usize,
    pub active_feeds: usize,
    pub total_documents: usize,
}
