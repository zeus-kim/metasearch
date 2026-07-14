//! SQLite-based article storage with full-text search.
//!
//! Provides persistent storage for RSS articles with:
//! - FTS5 full-text search
//! - Automatic deduplication by URL hash
//! - Configurable retention (auto-delete old articles)
//! - Trending word extraction

use super::RssItem;
use std::path::Path;
use std::sync::Mutex;

/// Article storage with SQLite backend
pub struct ArticleStore {
    conn: Mutex<rusqlite::Connection>,
    retention_days: u32,
}

impl ArticleStore {
    /// Open or create article database
    pub fn open(path: impl AsRef<Path>, retention_days: u32) -> Result<Self, rusqlite::Error> {
        let conn = rusqlite::Connection::open(path)?;

        // Enable WAL mode for better concurrency
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;

        // Create schema
        conn.execute_batch(r#"
            CREATE TABLE IF NOT EXISTS articles (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                url_hash TEXT UNIQUE NOT NULL,
                url TEXT NOT NULL,
                title TEXT NOT NULL,
                description TEXT,
                source TEXT,
                category TEXT,
                language TEXT,
                published_at INTEGER,
                indexed_at INTEGER NOT NULL,
                thumbnail TEXT,
                feed_type TEXT DEFAULT 'news',
                country TEXT DEFAULT '',
                normalized_category TEXT DEFAULT '',
                tier INTEGER DEFAULT 2
            );

            CREATE INDEX IF NOT EXISTS idx_articles_lang ON articles(language);
            CREATE INDEX IF NOT EXISTS idx_articles_cat ON articles(category);
            CREATE INDEX IF NOT EXISTS idx_articles_published ON articles(published_at DESC);
            CREATE INDEX IF NOT EXISTS idx_articles_indexed ON articles(indexed_at DESC);

            -- FTS5 full-text search index (space-tokenized, good for Latin/Cyrillic)
            CREATE VIRTUAL TABLE IF NOT EXISTS articles_fts USING fts5(
                title, description, source,
                content='articles',
                content_rowid='id'
            );

            -- FTS5 trigram index (universal, works for all languages including CJK)
            CREATE VIRTUAL TABLE IF NOT EXISTS articles_fts_trigram USING fts5(
                title, description,
                content='articles',
                content_rowid='id',
                tokenize='trigram'
            );

            -- Triggers to keep FTS in sync
            CREATE TRIGGER IF NOT EXISTS articles_ai AFTER INSERT ON articles BEGIN
                INSERT INTO articles_fts(rowid, title, description, source)
                VALUES (new.id, new.title, new.description, new.source);
                INSERT INTO articles_fts_trigram(rowid, title, description)
                VALUES (new.id, new.title, new.description);
            END;

            CREATE TRIGGER IF NOT EXISTS articles_ad AFTER DELETE ON articles BEGIN
                INSERT INTO articles_fts(articles_fts, rowid, title, description, source)
                VALUES ('delete', old.id, old.title, old.description, old.source);
                INSERT INTO articles_fts_trigram(articles_fts_trigram, rowid, title, description)
                VALUES ('delete', old.id, old.title, old.description);
            END;

            -- Trending words table
            CREATE TABLE IF NOT EXISTS trending_words (
                word TEXT PRIMARY KEY,
                count INTEGER DEFAULT 1,
                last_seen INTEGER NOT NULL
            );

            -- Feed quality tracking table
            CREATE TABLE IF NOT EXISTS feed_quality (
                url_hash TEXT PRIMARY KEY,
                url TEXT NOT NULL,
                success_count INTEGER DEFAULT 0,
                fail_count INTEGER DEFAULT 0,
                last_success_at INTEGER,
                last_fail_at INTEGER,
                quality_score REAL DEFAULT 50.0,
                status TEXT DEFAULT 'active',
                created_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_feed_quality_status ON feed_quality(status);
            CREATE INDEX IF NOT EXISTS idx_feed_quality_score ON feed_quality(quality_score DESC);

            -- Domain extraction stats (content extraction success/failure)
            CREATE TABLE IF NOT EXISTS domain_extraction_stats (
                domain TEXT PRIMARY KEY,
                success_count INTEGER DEFAULT 0,
                fail_count INTEGER DEFAULT 0,
                last_attempt_at INTEGER,
                extractable INTEGER DEFAULT 1
            );
            CREATE INDEX IF NOT EXISTS idx_domain_extractable ON domain_extraction_stats(extractable);
        "#)?;

        // Migration: add columns that may be missing from older DBs
        // Ignore errors since columns may already exist
        let _ = conn.execute("ALTER TABLE articles ADD COLUMN feed_type TEXT DEFAULT 'news'", []);
        let _ = conn.execute("ALTER TABLE articles ADD COLUMN country TEXT DEFAULT ''", []);
        let _ = conn.execute("ALTER TABLE articles ADD COLUMN normalized_category TEXT DEFAULT ''", []);
        let _ = conn.execute("ALTER TABLE articles ADD COLUMN tier INTEGER DEFAULT 2", []);

        Ok(Self {
            conn: Mutex::new(conn),
            retention_days,
        })
    }

    /// Open in-memory database (for testing)
    pub fn open_memory(retention_days: u32) -> Result<Self, rusqlite::Error> {
        Self::open(":memory:", retention_days)
    }

    /// Insert articles, skipping duplicates
    pub fn insert_articles(&self, items: &[RssItem]) -> Result<usize, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let mut inserted = 0;
        let mut stmt = conn.prepare_cached(
            "INSERT OR IGNORE INTO articles
             (url_hash, url, title, description, source, category, language, published_at, indexed_at, thumbnail, feed_type, country, normalized_category, tier)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"
        )?;

        for item in items {
            // Validate timestamp: must be between 2020 and now+1day
            let valid_published = item.published.filter(|&ts| {
                ts >= 1577836800 && ts <= now + 86400  // 2020-01-01 to now+1day
            });

            let url_hash = hash_url(&item.url);
            let result = stmt.execute(rusqlite::params![
                url_hash,
                item.url,
                item.title,
                item.description,
                item.source,
                item.category,
                item.language,
                valid_published,
                now,
                item.thumbnail,
                item.feed_type,
                item.country,
                item.normalized_category,
                item.tier,
            ]);
            if result.map(|n| n > 0).unwrap_or(false) {
                inserted += 1;
                // Update trending words for new articles
                self.update_trending_words(&item.title, now, &conn)?;
            }
        }

        Ok(inserted)
    }

    /// Search articles by query (LIKE for CJK, FTS5 for Latin)
    pub fn search(&self, query: &str, lang: Option<&str>, limit: usize) -> Result<Vec<RssItem>, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();

        // Use trigram FTS for non-Latin scripts (FTS5 can't tokenize them properly)
        let is_non_latin = query.chars().any(|c| {
            ('\u{AC00}'..='\u{D7AF}').contains(&c) || // Korean
            ('\u{4E00}'..='\u{9FFF}').contains(&c) || // CJK
            ('\u{3040}'..='\u{30FF}').contains(&c) || // Japanese
            ('\u{0900}'..='\u{097F}').contains(&c) || // Devanagari (Hindi, Marathi)
            ('\u{0980}'..='\u{09FF}').contains(&c) || // Bengali
            ('\u{0C00}'..='\u{0C7F}').contains(&c) || // Telugu
            ('\u{0B80}'..='\u{0BFF}').contains(&c) || // Tamil
            ('\u{0D00}'..='\u{0D7F}').contains(&c) || // Malayalam
            ('\u{0C80}'..='\u{0CFF}').contains(&c) || // Kannada
            ('\u{0A80}'..='\u{0AFF}').contains(&c) || // Gujarati
            ('\u{0A00}'..='\u{0A7F}').contains(&c) || // Punjabi
            ('\u{0E00}'..='\u{0E7F}').contains(&c) || // Thai
            ('\u{0600}'..='\u{06FF}').contains(&c) || // Arabic
            ('\u{0590}'..='\u{05FF}').contains(&c) || // Hebrew
            ('\u{0400}'..='\u{04FF}').contains(&c) || // Cyrillic
            ('\u{0370}'..='\u{03FF}').contains(&c)    // Greek
        });

        if is_non_latin {
            // CJK: use trigram FTS5 with BM25 + time decay + tier boost
            // Exclude YouTube (video content, not news articles)
            let sql = if lang.is_some() {
                "SELECT a.url, a.title, a.description, a.source, a.category, a.language, a.published_at, a.thumbnail, a.feed_type, a.country, a.normalized_category, a.tier
                 FROM articles a
                 JOIN articles_fts_trigram f ON a.id = f.rowid
                 WHERE articles_fts_trigram MATCH ?1 AND a.language = ?2 AND a.source NOT LIKE '%youtube%'
                 ORDER BY (CASE WHEN a.tier = 1 THEN 10.0 ELSE 1.0 END) * (bm25(articles_fts_trigram) * -1.0) * (1.0 + 1.0 / (1.0 + (strftime('%s','now') - a.published_at) / 604800.0)) DESC
                 LIMIT ?3"
            } else {
                "SELECT a.url, a.title, a.description, a.source, a.category, a.language, a.published_at, a.thumbnail, a.feed_type, a.country, a.normalized_category, a.tier
                 FROM articles a
                 JOIN articles_fts_trigram f ON a.id = f.rowid
                 WHERE articles_fts_trigram MATCH ?1 AND a.source NOT LIKE '%youtube%'
                 ORDER BY (CASE WHEN a.tier = 1 THEN 10.0 ELSE 1.0 END) * (bm25(articles_fts_trigram) * -1.0) * (1.0 + 1.0 / (1.0 + (strftime('%s','now') - a.published_at) / 604800.0)) DESC
                 LIMIT ?2"
            };

            let mut stmt = conn.prepare(sql)?;
            let rows = if let Some(l) = lang {
                stmt.query_map(rusqlite::params![query, l, limit], row_to_item)?
            } else {
                stmt.query_map(rusqlite::params![query, limit], row_to_item)?
            };
            return rows.filter_map(|r| r.ok()).collect::<Vec<_>>().pipe(Ok);
        }

        // FTS5 for Latin languages - BM25 + time decay + tier boost ranking
        // bm25() returns negative values (more negative = better match)
        // freshness decay: 1/(1 + days_old/7) gives recent articles higher weight
        // tier 1 sources get 10x boost, YouTube excluded
        let sql = if lang.is_some() {
            "SELECT a.url, a.title, a.description, a.source, a.category, a.language, a.published_at, a.thumbnail, a.feed_type, a.country, a.normalized_category, a.tier
             FROM articles a
             JOIN articles_fts f ON a.id = f.rowid
             WHERE articles_fts MATCH ?1 AND a.language = ?2 AND a.source NOT LIKE '%youtube%'
             ORDER BY (CASE WHEN a.tier = 1 THEN 10.0 ELSE 1.0 END) * (bm25(articles_fts) * -1.0) * (1.0 + 1.0 / (1.0 + (strftime('%s','now') - a.published_at) / 604800.0)) DESC
             LIMIT ?3"
        } else {
            "SELECT a.url, a.title, a.description, a.source, a.category, a.language, a.published_at, a.thumbnail, a.feed_type, a.country, a.normalized_category, a.tier
             FROM articles a
             JOIN articles_fts f ON a.id = f.rowid
             WHERE articles_fts MATCH ?1 AND a.source NOT LIKE '%youtube%'
             ORDER BY (CASE WHEN a.tier = 1 THEN 10.0 ELSE 1.0 END) * (bm25(articles_fts) * -1.0) * (1.0 + 1.0 / (1.0 + (strftime('%s','now') - a.published_at) / 604800.0)) DESC
             LIMIT ?2"
        };

        let mut stmt = conn.prepare(sql)?;
        let fts_query = fts5_escape(query);

        let rows = if let Some(l) = lang {
            stmt.query_map(rusqlite::params![fts_query, l, limit], row_to_item)?
        } else {
            stmt.query_map(rusqlite::params![fts_query, limit], row_to_item)?
        };

        rows.filter_map(|r| r.ok()).collect::<Vec<_>>().pipe(Ok)
    }

    /// Get recent articles (tier 1 sources prioritized)
    /// country: optional ISO 3166-1 alpha-2 code (e.g., "US", "KR", "JP")
    pub fn recent(&self, lang: Option<&str>, category: Option<&str>, limit: usize) -> Result<Vec<RssItem>, rusqlite::Error> {
        self.recent_with_country(lang, category, None, limit)
    }

    /// Get recent articles with optional country filter
    pub fn recent_with_country(&self, lang: Option<&str>, category: Option<&str>, country: Option<&str>, limit: usize) -> Result<Vec<RssItem>, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        // Order by tier (1 first) then freshness with time decay
        // tier 1 gets 10x boost (strong preference for major outlets), freshness decays over 7 days
        const RANK: &str = "(CASE WHEN tier = 1 THEN 10.0 ELSE 1.0 END) * (1.0 + 1.0 / (1.0 + (strftime('%s','now') - published_at) / 604800.0))";

        // For CJK languages, fetch more to filter by title script
        let fetch_limit = if matches!(lang, Some("ja") | Some("zh") | Some("ko")) {
            limit * 5  // Fetch 5x to account for mislabeled articles
        } else {
            limit
        };

        // Category filter with alias mappings for various DB formats
        let cat_filter = |c: &str| {
            let patterns: Vec<String> = match c {
                "ai" => vec![
                    "normalized_category LIKE '%:ai'",
                    "normalized_category LIKE '%artificial-intelligence%'",
                    "normalized_category LIKE '%machine-learning%'",
                    "category = 'ai'",
                ].into_iter().map(String::from).collect(),
                "art" | "culture" => vec![
                    "normalized_category LIKE '%:culture'",
                    "normalized_category LIKE '%:arts'",
                    "category = 'culture'",
                    "category = 'art'",
                ].into_iter().map(String::from).collect(),
                "climate" => vec![
                    "normalized_category LIKE '%:environment'",
                    "normalized_category LIKE '%climate%'",
                    "category = 'climate'",
                    "category = 'environment'",
                ].into_iter().map(String::from).collect(),
                "economy" | "business" => vec![
                    "normalized_category LIKE '%:business'",
                    "normalized_category LIKE '%:economie'",
                    "category = 'business'",
                    "category = 'economy'",
                ].into_iter().map(String::from).collect(),
                _ => vec![
                    format!("normalized_category LIKE '%:{c}'"),
                    format!("category = '{c}'"),
                ],
            };
            format!("({})", patterns.join(" OR "))
        };

        // Build WHERE clause dynamically
        let mut conditions: Vec<String> = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        let mut param_idx = 1;

        if let Some(l) = lang {
            conditions.push(format!("language = ?{}", param_idx));
            params.push(Box::new(l.to_string()));
            param_idx += 1;
        }
        if let Some(c) = category {
            conditions.push(cat_filter(c));
        }
        if let Some(co) = country {
            // Normalize country code to uppercase for matching
            conditions.push(format!("UPPER(country) = ?{}", param_idx));
            params.push(Box::new(co.to_uppercase()));
            param_idx += 1;
        }

        // Exclude YouTube from news results (video content, not news articles)
        conditions.push("source NOT LIKE '%youtube%'".to_string());

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };

        let sql = format!(
            "SELECT url, title, description, source, category, language, published_at, thumbnail, feed_type, country, normalized_category, tier
             FROM articles {} ORDER BY {} DESC LIMIT ?{}",
            where_clause, RANK, param_idx
        );
        params.push(Box::new(fetch_limit as i64));

        let mut stmt = conn.prepare(&sql)?;
        let params_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let rows = stmt.query_map(params_refs.as_slice(), row_to_item)?;
        let items: Vec<RssItem> = rows.filter_map(|r| r.ok()).collect();

        // For CJK languages, filter by actual title script
        let filtered = if let Some(l) = lang {
            match l {
                "ja" => items.into_iter()
                    .filter(|item| item.title.chars().any(|c|
                        ('\u{3040}'..='\u{309F}').contains(&c) || // Hiragana
                        ('\u{30A0}'..='\u{30FF}').contains(&c) || // Katakana
                        ('\u{4E00}'..='\u{9FFF}').contains(&c)    // CJK
                    ))
                    .take(limit)
                    .collect(),
                "zh" => items.into_iter()
                    .filter(|item| item.title.chars().any(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c)))
                    .take(limit)
                    .collect(),
                "ko" => items.into_iter()
                    .filter(|item| item.title.chars().any(|c| ('\u{AC00}'..='\u{D7AF}').contains(&c)))
                    .take(limit)
                    .collect(),
                _ => items.into_iter().take(limit).collect(),
            }
        } else {
            items.into_iter().take(limit).collect()
        };

        Ok(filtered)
    }

    /// Get trending words
    pub fn trending(&self, limit: usize) -> Result<Vec<(String, i64)>, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let cutoff = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64 - 86400) // Last 24 hours
            .unwrap_or(0);

        let mut stmt = conn.prepare(
            "SELECT word, count FROM trending_words
             WHERE last_seen > ?1 AND length(word) > 2
             ORDER BY count DESC LIMIT ?2"
        )?;

        let rows = stmt.query_map(rusqlite::params![cutoff, limit], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;

        rows.filter_map(|r| r.ok()).collect::<Vec<_>>().pipe(Ok)
    }

    /// Delete articles older than retention period
    pub fn cleanup(&self) -> Result<usize, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let cutoff = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64 - (self.retention_days as i64 * 86400))
            .unwrap_or(0);

        // Delete old articles and those with invalid timestamps
        let deleted = conn.execute(
            "DELETE FROM articles WHERE indexed_at < ?1 OR published_at IS NULL OR published_at < 1577836800",
            rusqlite::params![cutoff]
        )?;

        // Clean old trending words
        conn.execute(
            "DELETE FROM trending_words WHERE last_seen < ?1",
            rusqlite::params![cutoff]
        )?;

        // Embeddings reference articles with ON DELETE CASCADE, but rusqlite
        // connections don't enable foreign_keys by default — drop orphans.
        conn.execute(
            "DELETE FROM article_embeddings WHERE article_id NOT IN (SELECT id FROM articles)",
            [],
        )?;

        // Optimize database
        conn.execute_batch("PRAGMA optimize;")?;

        Ok(deleted)
    }

    /// Bytes of live data in the database (page count minus freelist), i.e.
    /// what the file would shrink to after a VACUUM.
    pub fn disk_usage_bytes(&self) -> Result<u64, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        let page_count: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
        let freelist: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
        Ok((page_count - freelist).max(0) as u64 * page_size.max(0) as u64)
    }

    /// Evict oldest articles until live data fits under `max_disk_mb` MiB,
    /// then VACUUM to actually return the space to the filesystem. No-op when
    /// `max_disk_mb` is 0 (unlimited). Returns the number of evicted articles.
    pub fn enforce_disk_cap(&self, max_disk_mb: u64) -> Result<usize, rusqlite::Error> {
        if max_disk_mb == 0 {
            return Ok(0);
        }
        let cap_bytes = max_disk_mb.saturating_mul(1024 * 1024);
        let mut evicted = 0usize;
        while self.disk_usage_bytes()? > cap_bytes {
            let conn = self.conn.lock().unwrap();
            let deleted = conn.execute(
                "DELETE FROM articles WHERE id IN
                     (SELECT id FROM articles ORDER BY indexed_at ASC LIMIT 500)",
                [],
            )?;
            conn.execute(
                "DELETE FROM article_embeddings WHERE article_id NOT IN (SELECT id FROM articles)",
                [],
            )?;
            drop(conn);
            if deleted == 0 {
                break; // nothing left to evict; cap smaller than fixed overhead
            }
            evicted += deleted;
        }
        if evicted > 0 {
            let conn = self.conn.lock().unwrap();
            conn.execute_batch("VACUUM;")?;
        }
        Ok(evicted)
    }

    /// Get statistics
    pub fn stats(&self) -> Result<StoreStats, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();

        let total_articles: i64 = conn.query_row(
            "SELECT COUNT(*) FROM articles", [], |r| r.get(0)
        )?;

        let languages: Vec<(String, i64)> = {
            let mut stmt = conn.prepare(
                "SELECT language, COUNT(*) FROM articles GROUP BY language ORDER BY COUNT(*) DESC"
            )?;
            let rows: Vec<_> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .filter_map(|r| r.ok())
                .collect();
            rows
        };

        let categories: Vec<(String, i64)> = {
            let mut stmt = conn.prepare(
                "SELECT category, COUNT(*) FROM articles GROUP BY category ORDER BY COUNT(*) DESC"
            )?;
            let rows: Vec<_> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .filter_map(|r| r.ok())
                .collect();
            rows
        };

        Ok(StoreStats {
            total_articles: total_articles as usize,
            languages,
            categories,
        })
    }

    /// Record feed fetch success
    pub fn record_feed_success(&self, url: &str, item_count: usize) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let url_hash = hash_url(url);

        conn.execute(
            "INSERT INTO feed_quality (url_hash, url, success_count, last_success_at, created_at)
             VALUES (?1, ?2, 1, ?3, ?3)
             ON CONFLICT(url_hash) DO UPDATE SET
                success_count = success_count + 1,
                last_success_at = ?3,
                quality_score = CAST(success_count + 1 AS REAL) / (success_count + fail_count + 1) * 100,
                status = CASE
                    WHEN CAST(success_count + 1 AS REAL) / (success_count + fail_count + 1) >= 0.5 THEN 'active'
                    WHEN CAST(success_count + 1 AS REAL) / (success_count + fail_count + 1) >= 0.2 THEN 'degraded'
                    ELSE 'disabled'
                END",
            rusqlite::params![url_hash, url, now]
        )?;
        Ok(())
    }

    /// Record feed fetch failure
    pub fn record_feed_failure(&self, url: &str, _error: &str) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let url_hash = hash_url(url);

        conn.execute(
            "INSERT INTO feed_quality (url_hash, url, fail_count, last_fail_at, created_at)
             VALUES (?1, ?2, 1, ?3, ?3)
             ON CONFLICT(url_hash) DO UPDATE SET
                fail_count = fail_count + 1,
                last_fail_at = ?3,
                quality_score = CAST(success_count AS REAL) / (success_count + fail_count + 1) * 100,
                status = CASE
                    WHEN CAST(success_count AS REAL) / (success_count + fail_count + 1) >= 0.5 THEN 'active'
                    WHEN CAST(success_count AS REAL) / (success_count + fail_count + 1) >= 0.2 THEN 'degraded'
                    ELSE 'disabled'
                END",
            rusqlite::params![url_hash, url, now]
        )?;
        Ok(())
    }

    /// Check if feed is active (not disabled due to low quality)
    pub fn is_feed_active(&self, url: &str) -> bool {
        let conn = self.conn.lock().unwrap();
        let url_hash = hash_url(url);

        let status: Result<String, _> = conn.query_row(
            "SELECT status FROM feed_quality WHERE url_hash = ?1",
            rusqlite::params![url_hash],
            |row| row.get(0)
        );

        match status {
            Ok(s) => s != "disabled",
            Err(_) => true, // New feed, assume active
        }
    }

    /// Check if feed should be polled this cycle (degraded feeds poll less frequently)
    pub fn should_poll_feed(&self, url: &str, cycle: u64) -> bool {
        let conn = self.conn.lock().unwrap();
        let url_hash = hash_url(url);

        let result: Result<(String, i64, i64), _> = conn.query_row(
            "SELECT status, success_count, fail_count FROM feed_quality WHERE url_hash = ?1",
            rusqlite::params![url_hash],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        );

        match result {
            Ok((status, success, fail)) => {
                match status.as_str() {
                    "disabled" => false,
                    "degraded" => cycle % 5 == 0, // Poll every 5th cycle
                    _ => {
                        // For active feeds with many failures, also slow down
                        if fail > 10 && success < fail {
                            cycle % 3 == 0
                        } else {
                            true
                        }
                    }
                }
            }
            Err(_) => true, // New feed, always poll
        }
    }

    /// Get feed quality statistics
    pub fn feed_quality_stats(&self) -> Result<FeedQualityStats, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();

        let total: i64 = conn.query_row(
            "SELECT COUNT(*) FROM feed_quality", [], |r| r.get(0)
        )?;

        let active: i64 = conn.query_row(
            "SELECT COUNT(*) FROM feed_quality WHERE status = 'active'", [], |r| r.get(0)
        )?;

        let degraded: i64 = conn.query_row(
            "SELECT COUNT(*) FROM feed_quality WHERE status = 'degraded'", [], |r| r.get(0)
        )?;

        let disabled: i64 = conn.query_row(
            "SELECT COUNT(*) FROM feed_quality WHERE status = 'disabled'", [], |r| r.get(0)
        )?;

        let avg_quality: f64 = conn.query_row(
            "SELECT COALESCE(AVG(quality_score), 0) FROM feed_quality", [], |r| r.get(0)
        )?;

        Ok(FeedQualityStats {
            total: total as usize,
            active: active as usize,
            degraded: degraded as usize,
            disabled: disabled as usize,
            avg_quality,
        })
    }

    /// Update trending words from title
    fn update_trending_words(&self, title: &str, now: i64, conn: &rusqlite::Connection) -> Result<(), rusqlite::Error> {
        let words = extract_words(title);
        let mut stmt = conn.prepare_cached(
            "INSERT INTO trending_words (word, count, last_seen) VALUES (?1, 1, ?2)
             ON CONFLICT(word) DO UPDATE SET count = count + 1, last_seen = ?2"
        )?;

        for word in words {
            stmt.execute(rusqlite::params![word, now])?;
        }
        Ok(())
    }

    /// Record domain extraction success
    pub fn record_extraction_success(&self, url: &str) {
        if let Some(domain) = extract_domain(url) {
            let conn = match self.conn.lock() {
                Ok(c) => c,
                Err(_) => return,
            };
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);

            let _ = conn.execute(
                "INSERT INTO domain_extraction_stats (domain, success_count, last_attempt_at)
                 VALUES (?1, 1, ?2)
                 ON CONFLICT(domain) DO UPDATE SET
                    success_count = success_count + 1,
                    last_attempt_at = ?2,
                    extractable = CASE
                        WHEN CAST(success_count + 1 AS REAL) / (success_count + fail_count + 1) >= 0.3 THEN 1
                        ELSE 0
                    END",
                rusqlite::params![domain, now]
            );
        }
    }

    /// Record domain extraction failure
    pub fn record_extraction_failure(&self, url: &str) {
        if let Some(domain) = extract_domain(url) {
            let conn = match self.conn.lock() {
                Ok(c) => c,
                Err(_) => return,
            };
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);

            let _ = conn.execute(
                "INSERT INTO domain_extraction_stats (domain, fail_count, last_attempt_at, extractable)
                 VALUES (?1, 1, ?2, 1)
                 ON CONFLICT(domain) DO UPDATE SET
                    fail_count = fail_count + 1,
                    last_attempt_at = ?2,
                    extractable = CASE
                        WHEN CAST(success_count AS REAL) / (success_count + fail_count + 1) >= 0.3 THEN 1
                        ELSE 0
                    END",
                rusqlite::params![domain, now]
            );
        }
    }

    /// Check if domain is extractable (success rate >= 30%)
    pub fn is_domain_extractable(&self, url: &str) -> bool {
        let domain = match extract_domain(url) {
            Some(d) => d,
            None => return true, // Unknown domain, assume extractable
        };

        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(_) => return true,
        };

        let result: Result<i64, _> = conn.query_row(
            "SELECT extractable FROM domain_extraction_stats WHERE domain = ?1",
            rusqlite::params![domain],
            |row| row.get(0)
        );

        match result {
            Ok(extractable) => extractable == 1,
            Err(_) => true, // No data yet, assume extractable
        }
    }

    /// Get list of non-extractable domains
    pub fn get_non_extractable_domains(&self) -> Vec<String> {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(_) => return vec![],
        };

        let mut stmt = match conn.prepare(
            "SELECT domain FROM domain_extraction_stats WHERE extractable = 0"
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };

        stmt.query_map([], |row| row.get(0))
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    }
}

/// Extract domain from URL
fn extract_domain(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_lowercase()))
}

/// Store statistics
#[derive(Debug, Clone)]
pub struct StoreStats {
    pub total_articles: usize,
    pub languages: Vec<(String, i64)>,
    pub categories: Vec<(String, i64)>,
}

/// Feed quality statistics
#[derive(Debug, Clone)]
pub struct FeedQualityStats {
    pub total: usize,
    pub active: usize,
    pub degraded: usize,
    pub disabled: usize,
    pub avg_quality: f64,
}

/// Hash URL for deduplication
fn hash_url(url: &str) -> String {
    use sha2::{Sha256, Digest};
    let mut hasher = Sha256::new();
    hasher.update(url.as_bytes());
    format!("{:x}", hasher.finalize())[..16].to_string()
}

/// Escape query for FTS5
fn fts5_escape(query: &str) -> String {
    // Simple escaping - wrap in quotes for phrase search if contains special chars
    if query.contains(|c: char| !c.is_alphanumeric() && !c.is_whitespace()) {
        format!("\"{}\"", query.replace('"', "\"\""))
    } else {
        query.to_string()
    }
}

/// Extract words for trending
fn extract_words(text: &str) -> Vec<String> {
    // Common stop words to filter out
    const STOP_WORDS: &[&str] = &[
        "the", "a", "an", "is", "are", "was", "were", "be", "been", "being",
        "have", "has", "had", "do", "does", "did", "will", "would", "could", "should",
        "may", "might", "must", "shall", "can", "need", "dare", "ought", "used",
        "to", "of", "in", "for", "on", "with", "at", "by", "from", "as", "into",
        "through", "during", "before", "after", "above", "below", "between", "under",
        "and", "but", "or", "nor", "so", "yet", "both", "either", "neither",
        "not", "only", "own", "same", "than", "too", "very", "just",
        "이", "그", "저", "것", "수", "등", "및", "더", "또", "년", "월", "일",
        "은", "는", "이", "가", "을", "를", "의", "에", "와", "과", "로", "으로",
    ];

    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 2 && w.len() < 30)
        .map(|w| w.to_lowercase())
        .filter(|w| !STOP_WORDS.contains(&w.as_str()))
        .filter(|w| !w.chars().all(|c| c.is_numeric()))
        .collect()
}

/// Convert row to RssItem
fn row_to_item(row: &rusqlite::Row) -> Result<RssItem, rusqlite::Error> {
    Ok(RssItem {
        url: row.get(0)?,
        title: row.get(1)?,
        description: row.get(2)?,
        source: row.get(3)?,
        category: row.get(4)?,
        language: row.get(5)?,
        published: row.get(6)?,
        thumbnail: row.get(7)?,
        feed_type: row.get(8).ok(),
        country: row.get(9).ok(),
        normalized_category: row.get(10).ok(),
        tier: row.get::<_, Option<u8>>(11).ok().flatten().unwrap_or(2),
    })
}

/// Pipe trait for ergonomic chaining
trait Pipe: Sized {
    fn pipe<F, R>(self, f: F) -> R where F: FnOnce(Self) -> R {
        f(self)
    }
}
impl<T> Pipe for T {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_url() {
        let h1 = hash_url("https://example.com/article1");
        let h2 = hash_url("https://example.com/article2");
        assert_ne!(h1, h2);
        assert_eq!(h1.len(), 16);
    }

    #[test]
    fn test_extract_words() {
        let words = extract_words("Breaking: Apple announces new iPhone 15");
        assert!(words.contains(&"apple".to_string()));
        assert!(words.contains(&"iphone".to_string()));
        assert!(!words.contains(&"the".to_string()));
    }
}
