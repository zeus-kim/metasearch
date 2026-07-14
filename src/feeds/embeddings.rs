//! Background embedding worker and semantic search
//! Uses Ollama BGE-M3 for multilingual embeddings

use rusqlite::params;
use std::collections::HashMap;
use std::sync::Mutex;

const EMBEDDING_DIM: usize = 1024;
const BATCH_SIZE: usize = 100;
const EMBEDDING_MAX_AGE_DAYS: i64 = 7;

/// Category keywords for embedding-based classification (multilingual)
/// Must match UI categories: tech, ai, world, politics, economy, finance, health, climate, sports, entertainment, art, science
const CATEGORY_KEYWORDS: &[(&str, &str)] = &[
    ("tech", "기술 IT 소프트웨어 하드웨어 앱 스마트폰 technology software hardware app smartphone computer 技術 テクノロジー 科技"),
    ("ai", "인공지능 AI 머신러닝 딥러닝 ChatGPT GPT LLM 생성AI artificial intelligence machine learning deep learning 人工知能"),
    ("world", "국제 세계 외교 해외 글로벌 UN 미국 중국 유럽 international world global foreign diplomacy 国際 世界 國際"),
    ("politics", "정치 국회 대통령 선거 법안 정당 여당 야당 politics government election president congress 政治 選挙"),
    ("economy", "경제 기업 산업 투자 시장 무역 GDP economy business company market investment trade 経済 企業 經濟"),
    ("finance", "주식 증시 금융 은행 코스피 나스닥 비트코인 암호화폐 stock finance banking KOSPI NASDAQ bitcoin crypto 株式 金融"),
    ("health", "건강 의료 병원 질병 치료 백신 코로나 health medical hospital disease treatment vaccine 健康 医療 醫療"),
    ("climate", "기후 환경 날씨 온난화 탄소 재생에너지 태풍 climate environment weather carbon renewable 気候 環境 氣候"),
    ("sports", "스포츠 축구 야구 농구 올림픽 월드컵 NBA MLB sports football soccer baseball basketball Olympics 스포츠 體育"),
    ("entertainment", "연예 방송 드라마 영화 아이돌 K-pop 넷플릭스 entertainment celebrity drama movie Netflix 芸能 エンタメ 娛樂"),
    ("art", "문화 예술 전시 공연 박물관 음악 미술 culture art exhibition museum performance music 文化 芸術 藝術"),
    ("science", "과학 연구 우주 물리 생물 NASA 노벨 science research space physics biology NASA Nobel 科学 宇宙 科學"),
];

/// Embedding store with SQLite backend
pub struct EmbeddingStore {
    conn: Mutex<rusqlite::Connection>,
    ollama_url: String,
    model: String,
}

impl EmbeddingStore {
    /// Open embedding store (uses same DB as articles)
    pub fn open(db_path: &str, ollama_url: &str) -> Result<Self, rusqlite::Error> {
        let conn = rusqlite::Connection::open(db_path)?;

        // Ensure tables exist
        conn.execute_batch(r#"
            CREATE TABLE IF NOT EXISTS article_embeddings (
                article_id INTEGER PRIMARY KEY,
                embedding BLOB NOT NULL,
                model TEXT DEFAULT 'bge-m3',
                created_at INTEGER NOT NULL,
                FOREIGN KEY(article_id) REFERENCES articles(id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_embeddings_created ON article_embeddings(created_at);

            CREATE TABLE IF NOT EXISTS category_embeddings (
                category TEXT PRIMARY KEY,
                embedding BLOB NOT NULL,
                keywords TEXT NOT NULL,
                updated_at INTEGER NOT NULL
            );
        "#)?;

        Ok(Self {
            conn: Mutex::new(conn),
            ollama_url: ollama_url.to_string(),
            model: "bge-m3".to_string(),
        })
    }

    /// Get pending articles that need embedding (only recent articles)
    pub fn get_pending(&self, limit: usize) -> Result<Vec<(i64, String)>, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let cutoff = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0) - (EMBEDDING_MAX_AGE_DAYS * 24 * 60 * 60);

        let mut stmt = conn.prepare(
            "SELECT a.id, a.title || ' ' || COALESCE(a.description, '')
             FROM articles a
             LEFT JOIN article_embeddings e ON a.id = e.article_id
             WHERE e.article_id IS NULL
               AND a.indexed_at > ?
             ORDER BY a.indexed_at DESC
             LIMIT ?"
        )?;

        let rows = stmt.query_map(params![cutoff, limit], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;

        rows.filter_map(|r| r.ok()).collect::<Vec<_>>().pipe(Ok)
    }

    /// Store embedding for an article
    pub fn store_embedding(&self, article_id: i64, embedding: &[f32]) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        // Convert f32 array to bytes
        let bytes: Vec<u8> = embedding.iter()
            .flat_map(|f| f.to_le_bytes())
            .collect();

        conn.execute(
            "INSERT OR REPLACE INTO article_embeddings (article_id, embedding, model, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![article_id, bytes, self.model, now]
        )?;

        Ok(())
    }

    /// Get embedding for an article
    pub fn get_embedding(&self, article_id: i64) -> Result<Option<Vec<f32>>, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let result: Result<Vec<u8>, _> = conn.query_row(
            "SELECT embedding FROM article_embeddings WHERE article_id = ?",
            [article_id],
            |row| row.get(0)
        );

        match result {
            Ok(bytes) => Ok(Some(bytes_to_embedding(&bytes))),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Semantic search: find similar articles by query
    pub async fn search(&self, query: &str, lang: Option<&str>, limit: usize) -> Result<Vec<(i64, f32)>, String> {
        // Get query embedding
        let query_emb = self.get_ollama_embedding(query).await?;

        // Search in DB
        let conn = self.conn.lock().unwrap();

        let sql = if lang.is_some() {
            "SELECT e.article_id, e.embedding, a.language
             FROM article_embeddings e
             JOIN articles a ON e.article_id = a.id
             WHERE a.language = ?
             ORDER BY e.created_at DESC
             LIMIT 5000"
        } else {
            "SELECT e.article_id, e.embedding, a.language
             FROM article_embeddings e
             JOIN articles a ON e.article_id = a.id
             ORDER BY e.created_at DESC
             LIMIT 5000"
        };

        let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;

        let rows: Vec<(i64, Vec<u8>)> = if let Some(l) = lang {
            let iter = stmt.query_map([l], |row| Ok((row.get(0)?, row.get(1)?)))
                .map_err(|e| e.to_string())?;
            iter.filter_map(|r| r.ok()).collect()
        } else {
            let iter = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .map_err(|e| e.to_string())?;
            iter.filter_map(|r| r.ok()).collect()
        };

        // Calculate similarities
        let mut results: Vec<(i64, f32)> = rows.iter()
            .map(|(id, bytes)| {
                let emb = bytes_to_embedding(bytes);
                let sim = cosine_similarity(&query_emb, &emb);
                (*id, sim)
            })
            .collect();

        // Sort by similarity descending
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(limit);

        Ok(results)
    }

    /// Get embedding from Ollama
    pub async fn get_ollama_embedding(&self, text: &str) -> Result<Vec<f32>, String> {
        if text.trim().is_empty() {
            return Err("empty text".to_string());
        }

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| e.to_string())?;

        let url = format!("{}/api/embeddings", self.ollama_url);

        let body = serde_json::json!({
            "model": self.model,
            "prompt": text.chars().take(500).collect::<String>()
        });

        let resp = client.post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("request failed: {}", e))?;

        if !resp.status().is_success() {
            return Err(format!("http {}", resp.status()));
        }

        let bytes = resp.bytes().await.map_err(|e| format!("read body: {}", e))?;
        let json: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|e| format!("parse json: {}", e))?;

        let embedding = json["embedding"]
            .as_array()
            .ok_or("No embedding in response")?
            .iter()
            .map(|v| v.as_f64().unwrap_or(0.0) as f32)
            .collect();

        Ok(embedding)
    }

    /// Process pending embeddings in batches (also classifies category)
    pub async fn process_batch(&self) -> Result<usize, String> {
        let pending = self.get_pending(BATCH_SIZE).map_err(|e| e.to_string())?;

        if pending.is_empty() {
            return Ok(0);
        }

        // Load category embeddings once
        let cat_embeddings = self.get_category_embeddings().map_err(|e| e.to_string())?;

        let mut processed = 0;
        for (article_id, text) in pending {
            // Truncate long text
            let text = if text.len() > 1000 {
                text.chars().take(1000).collect()
            } else {
                text
            };

            match self.get_ollama_embedding(&text).await {
                Ok(emb) => {
                    // Store embedding
                    if let Err(e) = self.store_embedding(article_id, &emb) {
                        eprintln!("[Embedding] Store error: {}", e);
                        continue;
                    }

                    // Classify category using pre-loaded embeddings
                    let category = classify_with_embeddings(&emb, &cat_embeddings);
                    if let Err(e) = self.update_article_category(article_id, &category) {
                        eprintln!("[Embedding] Category update error: {}", e);
                    }

                    processed += 1;
                }
                Err(e) => {
                    eprintln!("[Embedding] Ollama error: {}", e);
                }
            }
        }

        Ok(processed)
    }

    /// Update article category in database
    fn update_article_category(&self, article_id: i64, category: &str) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE articles SET category = ?1 WHERE id = ?2",
            params![category, article_id]
        )?;
        Ok(())
    }

    /// Get embedding statistics (only counts recent articles as pending)
    pub fn stats(&self) -> Result<EmbeddingStats, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let cutoff = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0) - (EMBEDDING_MAX_AGE_DAYS * 24 * 60 * 60);

        let total: i64 = conn.query_row(
            "SELECT COUNT(*) FROM article_embeddings", [], |r| r.get(0)
        )?;

        let pending: i64 = conn.query_row(
            "SELECT COUNT(*) FROM articles a
             LEFT JOIN article_embeddings e ON a.id = e.article_id
             WHERE e.article_id IS NULL AND a.indexed_at > ?",
            [cutoff], |r| r.get(0)
        )?;

        let recent_total: i64 = conn.query_row(
            "SELECT COUNT(*) FROM articles WHERE indexed_at > ?",
            [cutoff], |r| r.get(0)
        )?;

        Ok(EmbeddingStats {
            total: total as usize,
            pending: pending as usize,
            recent_articles: recent_total as usize,
        })
    }

    /// Initialize category embeddings from CATEGORY_KEYWORDS
    pub async fn init_category_embeddings(&self) -> Result<usize, String> {
        let mut initialized = 0;

        for (category, keywords) in CATEGORY_KEYWORDS {
            if self.has_category_embedding(category) {
                continue;
            }

            match self.get_ollama_embedding(keywords).await {
                Ok(emb) => {
                    if let Err(e) = self.store_category_embedding(category, keywords, &emb) {
                        eprintln!("[Embedding] Store category error: {}", e);
                    } else {
                        eprintln!("[Embedding] Initialized category: {}", category);
                        initialized += 1;
                    }
                }
                Err(e) => {
                    eprintln!("[Embedding] Category embedding error for {}: {}", category, e);
                }
            }
        }

        Ok(initialized)
    }

    fn has_category_embedding(&self, category: &str) -> bool {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM category_embeddings WHERE category = ?",
            [category],
            |r| r.get(0)
        ).unwrap_or(0);
        count > 0
    }

    fn store_category_embedding(&self, category: &str, keywords: &str, embedding: &[f32]) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let bytes: Vec<u8> = embedding.iter()
            .flat_map(|f| f.to_le_bytes())
            .collect();

        conn.execute(
            "INSERT OR REPLACE INTO category_embeddings (category, embedding, keywords, updated_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![category, bytes, keywords, now]
        )?;

        Ok(())
    }

    /// Classify text into a category using embedding similarity
    pub async fn classify_by_embedding(&self, text: &str) -> Result<(String, f32), String> {
        let text_emb = self.get_ollama_embedding(text).await?;

        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT category, embedding FROM category_embeddings"
        ).map_err(|e| e.to_string())?;

        let rows: Vec<(String, Vec<u8>)> = stmt.query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?))
        }).map_err(|e| e.to_string())?
        .filter_map(|r| r.ok())
        .collect();

        drop(stmt);
        drop(conn);

        let mut best = ("news".to_string(), 0.0f32);

        for (cat, bytes) in rows {
            let cat_emb = bytes_to_embedding(&bytes);
            let sim = cosine_similarity(&text_emb, &cat_emb);
            if sim > best.1 {
                best = (cat, sim);
            }
        }

        Ok(best)
    }

    /// Batch classify multiple texts
    pub async fn classify_batch(&self, texts: &[&str]) -> Vec<(String, f32)> {
        let mut results = Vec::with_capacity(texts.len());

        for text in texts {
            match self.classify_by_embedding(text).await {
                Ok(r) => results.push(r),
                Err(_) => results.push(("news".to_string(), 0.0)),
            }
        }

        results
    }

    /// Get all category embeddings as a map
    pub fn get_category_embeddings(&self) -> Result<HashMap<String, Vec<f32>>, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT category, embedding FROM category_embeddings"
        )?;

        let mut map = HashMap::new();
        let rows = stmt.query_map([], |row| {
            let cat: String = row.get(0)?;
            let bytes: Vec<u8> = row.get(1)?;
            Ok((cat, bytes))
        })?;

        for row in rows.filter_map(|r| r.ok()) {
            map.insert(row.0, bytes_to_embedding(&row.1));
        }

        Ok(map)
    }
}

#[derive(Debug, Clone)]
pub struct EmbeddingStats {
    pub total: usize,
    pub pending: usize,
    pub recent_articles: usize,
}

/// Convert bytes to f32 embedding
fn bytes_to_embedding(bytes: &[u8]) -> Vec<f32> {
    bytes.chunks(4)
        .map(|chunk| {
            let arr: [u8; 4] = chunk.try_into().unwrap_or([0; 4]);
            f32::from_le_bytes(arr)
        })
        .collect()
}

/// Classify using pre-loaded category embeddings (no async, no DB access)
fn classify_with_embeddings(text_emb: &[f32], cat_embeddings: &HashMap<String, Vec<f32>>) -> String {
    let mut best = ("news".to_string(), 0.0f32);

    for (cat, cat_emb) in cat_embeddings {
        let sim = cosine_similarity(text_emb, cat_emb);
        if sim > best.1 {
            best = (cat.clone(), sim);
        }
    }

    // Only assign if confidence is high enough (>0.5), otherwise default to "news"
    if best.1 > 0.5 {
        best.0
    } else {
        "news".to_string()
    }
}

/// Cosine similarity between two vectors
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }

    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();

    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a * norm_b)
    }
}

/// Pipe trait for ergonomic chaining
trait Pipe: Sized {
    fn pipe<F, R>(self, f: F) -> R where F: FnOnce(Self) -> R {
        f(self)
    }
}
impl<T> Pipe for T {}

/// Background embedding worker
pub struct EmbeddingWorker {
    store: std::sync::Arc<EmbeddingStore>,
}

impl EmbeddingWorker {
    pub fn new(store: std::sync::Arc<EmbeddingStore>) -> Self {
        Self { store }
    }

    /// Run background embedding loop
    pub async fn run(&self) {
        eprintln!("[Embedding] Starting background worker (BGE-M3)");

        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));

        loop {
            interval.tick().await;

            match self.store.process_batch().await {
                Ok(0) => {
                    // No pending, check less frequently
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                }
                Ok(n) => {
                    if let Ok(stats) = self.store.stats() {
                        eprintln!("[Embedding] Processed {} articles, total: {}, pending: {} (recent {}/{})",
                            n, stats.total, stats.pending,
                            stats.recent_articles - stats.pending, stats.recent_articles);
                    }
                }
                Err(e) => {
                    eprintln!("[Embedding] Error: {}", e);
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cosine_similarity() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 0.001);

        let c = vec![0.0, 1.0, 0.0];
        assert!(cosine_similarity(&a, &c).abs() < 0.001);
    }

    #[test]
    fn test_bytes_to_embedding() {
        let original: Vec<f32> = vec![1.0, 2.0, 3.0];
        let bytes: Vec<u8> = original.iter()
            .flat_map(|f| f.to_le_bytes())
            .collect();
        let restored = bytes_to_embedding(&bytes);
        assert_eq!(original, restored);
    }
}
