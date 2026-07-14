//! Pluggable, TTL'd result cache.
//!
//! Three backends, selected by `server.cache_backend`:
//! * `memory` (default) — a bounded in-process `HashMap`. The query text lives
//!   only in RAM (never written to disk/logs), preserving no-query-logging.
//! * `disk` — JSON files under `server.cache_dir`, surviving restarts. Keys are
//!   hashed (sha256) so the raw query never appears in a filename.
//! * `redis` — requires building with `--features redis`; uses a tiny built-in
//!   RESP client (no extra crate). Falls back to a no-op when unreachable.
//!
//! Keyed by `(query, categories, page, lang, safe_search, ...)`. Any backend
//! failure degrades to "cache miss" so search always works.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::search::SearchResponse;

const MAX_ENTRIES: usize = 512;

struct Entry {
    inserted: Instant,
    value: SearchResponse,
}

/// In-memory bounded cache.
struct MemoryCache {
    map: Mutex<HashMap<String, Entry>>,
}

impl MemoryCache {
    fn get(&self, key: &str, ttl: Duration) -> Option<SearchResponse> {
        let mut map = self.map.lock().ok()?;
        if let Some(entry) = map.get(key) {
            if entry.inserted.elapsed() < ttl {
                return Some(entry.value.clone());
            }
            map.remove(key);
        }
        None
    }

    fn put(&self, key: String, value: SearchResponse, ttl: Duration) {
        if let Ok(mut map) = self.map.lock() {
            if map.len() >= MAX_ENTRIES {
                let now = Instant::now();
                map.retain(|_, e| now.duration_since(e.inserted) < ttl);
                if map.len() >= MAX_ENTRIES {
                    map.clear();
                }
            }
            map.insert(
                key,
                Entry {
                    inserted: Instant::now(),
                    value,
                },
            );
        }
    }

    fn clear(&self) {
        if let Ok(mut map) = self.map.lock() {
            map.clear();
        }
    }
}

/// Disk-backed cache: one JSON file per entry under `dir`.
struct DiskCache {
    dir: std::path::PathBuf,
}

impl DiskCache {
    fn new(dir: impl Into<std::path::PathBuf>) -> Self {
        let dir = dir.into();
        let _ = std::fs::create_dir_all(&dir);
        DiskCache { dir }
    }

    fn path_for(&self, key: &str) -> std::path::PathBuf {
        self.dir.join(format!("{}.json", hash_key(key)))
    }

    fn get(&self, key: &str, ttl: Duration) -> Option<SearchResponse> {
        let path = self.path_for(key);
        let raw = std::fs::read_to_string(&path).ok()?;
        let stored: StoredEntry = serde_json::from_str(&raw).ok()?;
        let age = now_unix().saturating_sub(stored.inserted_unix);
        if age > ttl.as_secs() {
            let _ = std::fs::remove_file(&path);
            return None;
        }
        Some(stored.value)
    }

    fn put(&self, key: &str, value: &SearchResponse) {
        let stored = StoredEntry {
            inserted_unix: now_unix(),
            value: value.clone(),
        };
        if let Ok(json) = serde_json::to_string(&stored) {
            let _ = std::fs::write(self.path_for(key), json);
        }
    }

    fn clear(&self) {
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for e in entries.flatten() {
                if e.path().extension().is_some_and(|x| x == "json") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct StoredEntry {
    inserted_unix: u64,
    value: SearchResponse,
}

enum Backend {
    Disabled,
    Memory(MemoryCache),
    Disk(DiskCache),
    #[cfg(feature = "redis")]
    Redis(redis_backend::RedisCache),
}

/// Cache statistics (hits, misses, total queries).
#[derive(Debug, Default)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub total: u64,
}

impl CacheStats {
    /// Calculate hit rate as a percentage (0-100).
    pub fn hit_rate(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            (self.hits as f64 / self.total as f64) * 100.0
        }
    }
}

/// TTL'd result cache with a configurable backend.
pub struct Cache {
    ttl: Duration,
    backend: Backend,
    /// Number of cache hits (atomic for concurrent access).
    hits: AtomicU64,
    /// Number of cache misses (atomic for concurrent access).
    misses: AtomicU64,
}

impl Cache {
    /// Build the in-memory backend (kept for back-compat / tests).
    pub fn new(ttl_secs: u64) -> Self {
        Cache {
            ttl: Duration::from_secs(ttl_secs),
            backend: if ttl_secs == 0 {
                Backend::Disabled
            } else {
                Backend::Memory(MemoryCache {
                    map: Mutex::new(HashMap::new()),
                })
            },
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// Build a cache from server settings, selecting the configured backend.
    pub fn from_settings(s: &crate::config::ServerSettings) -> Self {
        let ttl = s.cache_ttl_secs;
        if ttl == 0 {
            return Cache {
                ttl: Duration::ZERO,
                backend: Backend::Disabled,
                hits: AtomicU64::new(0),
                misses: AtomicU64::new(0),
            };
        }
        let backend = match s.cache_backend.as_str() {
            "disk" => Backend::Disk(DiskCache::new(&s.cache_dir)),
            #[cfg(feature = "redis")]
            "redis" => Backend::Redis(redis_backend::RedisCache::new(&s.redis_url)),
            #[cfg(not(feature = "redis"))]
            "redis" => {
                crate::obs::warn("cache_backend=redis but built without the `redis` feature; falling back to memory");
                Backend::Memory(MemoryCache {
                    map: Mutex::new(HashMap::new()),
                })
            }
            _ => Backend::Memory(MemoryCache {
                map: Mutex::new(HashMap::new()),
            }),
        };
        Cache {
            ttl: Duration::from_secs(ttl),
            backend,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// Whether caching is active.
    pub fn enabled(&self) -> bool {
        !matches!(self.backend, Backend::Disabled) && !self.ttl.is_zero()
    }

    /// Human-readable backend label (for `/stats`).
    pub fn backend_name(&self) -> &'static str {
        match self.backend {
            Backend::Disabled => "disabled",
            Backend::Memory(_) => "memory",
            Backend::Disk(_) => "disk",
            #[cfg(feature = "redis")]
            Backend::Redis(_) => "redis",
        }
    }

    /// Get a cached entry, tracking hit/miss statistics.
    pub fn get(&self, key: &str) -> Option<SearchResponse> {
        if !self.enabled() {
            return None;
        }
        let result = match &self.backend {
            Backend::Disabled => None,
            Backend::Memory(m) => m.get(key, self.ttl),
            Backend::Disk(d) => d.get(key, self.ttl),
            #[cfg(feature = "redis")]
            Backend::Redis(r) => r.get(key),
        };
        if result.is_some() {
            self.hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
        }
        result
    }

    /// Get current cache statistics.
    pub fn stats(&self) -> CacheStats {
        let hits = self.hits.load(Ordering::Relaxed);
        let misses = self.misses.load(Ordering::Relaxed);
        CacheStats {
            hits,
            misses,
            total: hits + misses,
        }
    }

    pub fn put(&self, key: String, value: SearchResponse) {
        if !self.enabled() {
            return;
        }
        match &self.backend {
            Backend::Disabled => {}
            Backend::Memory(m) => m.put(key, value, self.ttl),
            Backend::Disk(d) => d.put(&key, &value),
            #[cfg(feature = "redis")]
            Backend::Redis(r) => r.put(&key, &value, self.ttl),
        }
    }

    /// Drop all cached entries (e.g. after a live settings change).
    pub fn clear(&self) {
        match &self.backend {
            Backend::Disabled => {}
            Backend::Memory(m) => m.clear(),
            Backend::Disk(d) => d.clear(),
            #[cfg(feature = "redis")]
            Backend::Redis(r) => r.clear(),
        }
    }

    /// Approximate number of cached entries (memory backend only; others return 0).
    pub fn len(&self) -> usize {
        match &self.backend {
            Backend::Memory(m) => m.map.lock().map(|g| g.len()).unwrap_or(0),
            _ => 0,
        }
    }
}

/// Hash a cache key so the raw query never appears on disk / in Redis keys.
fn hash_key(key: &str) -> String {
    let mut h = Sha256::new();
    h.update(key.as_bytes());
    let digest = h.finalize();
    let mut s = String::with_capacity(64);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(feature = "redis")]
mod redis_backend {
    //! Minimal synchronous Redis client (RESP over `std::net::TcpStream`).
    //! Implements just the GET / SETEX / FLUSHDB we need — no external crate, so
    //! enabling the `redis` feature never requires fetching new dependencies.

    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    use super::{hash_key, SearchResponse};

    pub struct RedisCache {
        addr: String,
    }

    impl RedisCache {
        pub fn new(url: &str) -> Self {
            // Accept redis://host:port or host:port.
            let stripped = url
                .strip_prefix("redis://")
                .or_else(|| url.strip_prefix("rediss://"))
                .unwrap_or(url);
            let addr = stripped.split('/').next().unwrap_or(stripped);
            let addr = if addr.contains(':') {
                addr.to_string()
            } else {
                format!("{addr}:6379")
            };
            RedisCache { addr }
        }

        fn connect(&self) -> Option<TcpStream> {
            let stream = TcpStream::connect(&self.addr).ok()?;
            stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .ok()?;
            Some(stream)
        }

        fn redis_key(key: &str) -> String {
            format!("metasearch:{}", hash_key(key))
        }

        pub fn get(&self, key: &str) -> Option<SearchResponse> {
            let mut conn = self.connect()?;
            let cmd = encode(&["GET", &Self::redis_key(key)]);
            conn.write_all(&cmd).ok()?;
            let bulk = read_bulk_string(&mut conn)?;
            serde_json::from_slice(&bulk).ok()
        }

        pub fn put(&self, key: &str, value: &SearchResponse, ttl: Duration) {
            let Some(mut conn) = self.connect() else {
                return;
            };
            let Ok(json) = serde_json::to_string(value) else {
                return;
            };
            let secs = ttl.as_secs().max(1).to_string();
            let cmd = encode(&["SETEX", &Self::redis_key(key), &secs, &json]);
            let _ = conn.write_all(&cmd);
            let mut buf = [0u8; 64];
            let _ = conn.read(&mut buf); // consume +OK
        }

        pub fn clear(&self) {
            if let Some(mut conn) = self.connect() {
                let _ = conn.write_all(&encode(&["FLUSHDB"]));
                let mut buf = [0u8; 64];
                let _ = conn.read(&mut buf);
            }
        }
    }

    /// Encode a RESP array of bulk strings.
    fn encode(args: &[&str]) -> Vec<u8> {
        let mut out = format!("*{}\r\n", args.len()).into_bytes();
        for a in args {
            out.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
            out.extend_from_slice(a.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        out
    }

    /// Read a single RESP bulk-string reply, returning its bytes (or None for a
    /// nil reply / error).
    fn read_bulk_string(conn: &mut TcpStream) -> Option<Vec<u8>> {
        let mut all = Vec::new();
        let mut tmp = [0u8; 4096];
        // Read the header line first.
        loop {
            let n = conn.read(&mut tmp).ok()?;
            if n == 0 {
                break;
            }
            all.extend_from_slice(&tmp[..n]);
            // Heuristic: stop once we likely have the whole payload.
            if all.len() >= 2 && all.ends_with(b"\r\n") && all.len() > 8 {
                break;
            }
            if n < tmp.len() {
                break;
            }
        }
        if all.is_empty() || all[0] != b'$' {
            return None;
        }
        let line_end = all.windows(2).position(|w| w == b"\r\n")?;
        let len: i64 = std::str::from_utf8(&all[1..line_end]).ok()?.parse().ok()?;
        if len < 0 {
            return None; // nil
        }
        let start = line_end + 2;
        let end = start + len as usize;
        all.get(start..end).map(|s| s.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServerSettings;
    use crate::search::SearchResponse;

    fn sample() -> SearchResponse {
        let mut r = SearchResponse::empty("hello".into(), 1);
        r.number_of_results = 3;
        r
    }

    #[test]
    fn memory_roundtrip() {
        let c = Cache::new(300);
        assert_eq!(c.backend_name(), "memory");
        c.put("k".into(), sample());
        assert_eq!(c.get("k").unwrap().query, "hello");
    }

    #[test]
    fn disabled_when_ttl_zero() {
        let c = Cache::new(0);
        assert!(!c.enabled());
        c.put("k".into(), sample());
        assert!(c.get("k").is_none());
    }

    #[test]
    fn disk_roundtrip_and_clear() {
        let dir = std::env::temp_dir().join(format!("ms-cache-{}", now_unix_nanos()));
        let s = ServerSettings {
            cache_backend: "disk".into(),
            cache_dir: dir.to_string_lossy().to_string(),
            ..Default::default()
        };
        let c = Cache::from_settings(&s);
        assert_eq!(c.backend_name(), "disk");
        c.put("query-key".into(), sample());
        assert_eq!(c.get("query-key").unwrap().number_of_results, 3);
        // Raw query must not appear in any filename.
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            assert!(!e.file_name().to_string_lossy().contains("query-key"));
        }
        c.clear();
        assert!(c.get("query-key").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn now_unix_nanos() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }
}
