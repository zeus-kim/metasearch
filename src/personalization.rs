//! Local, privacy-preserving personalization.
//!
//! Learns which engines you reach for most often (via explicit `!bang`s) and
//! gently boosts their ranking weight. The only thing stored is a small
//! `engine -> count` map, persisted crate-side to a JSON file on disk. Query
//! text is NEVER recorded — this keeps the no-query-logging promise while still
//! adapting to your habits entirely locally (nothing leaves the machine).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

/// Tracks per-engine usage counts and persists them to disk.
pub struct Personalization {
    path: PathBuf,
    counts: Mutex<HashMap<String, u64>>,
}

impl Personalization {
    /// Load existing counts from `path` (or start empty if absent/corrupt).
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let counts = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<HashMap<String, u64>>(&s).ok())
            .unwrap_or_default();
        Personalization {
            path,
            counts: Mutex::new(counts),
        }
    }

    /// Record explicit usage of the given engines (e.g. from `!bang`s) and
    /// persist the updated counts. No-op for an empty slice.
    pub fn record(&self, engines: &[String]) {
        if engines.is_empty() {
            return;
        }
        let snapshot = {
            let Ok(mut map) = self.counts.lock() else {
                return;
            };
            for e in engines {
                *map.entry(e.clone()).or_insert(0) += 1;
            }
            map.clone()
        };
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        if let Ok(json) = serde_json::to_string(&snapshot) {
            let _ = std::fs::write(&self.path, json);
        }
    }

    /// Current usage counts snapshot.
    pub fn counts(&self) -> HashMap<String, u64> {
        self.counts.lock().map(|m| m.clone()).unwrap_or_default()
    }

    /// Apply a gentle, bounded boost to engine weights based on usage. Mutates
    /// `weights` in place; engines never used are left untouched.
    pub fn apply_boost(&self, weights: &mut HashMap<String, f64>) {
        let counts = self.counts();
        for (engine, count) in counts {
            let factor = boost_factor(count);
            let w = weights.entry(engine).or_insert(1.0);
            *w *= factor;
        }
    }
}

/// Bounded logarithmic boost factor for a usage `count`. Returns `1.0` for an
/// unused engine and saturates around `1.5` so personalization nudges ranking
/// without ever overwhelming the base positional score.
pub fn boost_factor(count: u64) -> f64 {
    if count == 0 {
        return 1.0;
    }
    let boost = 0.15 * (1.0 + count as f64).ln();
    1.0 + boost.min(0.5)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boost_grows_then_saturates() {
        assert_eq!(boost_factor(0), 1.0);
        assert!(boost_factor(1) > 1.0);
        assert!(boost_factor(10) > boost_factor(1));
        assert!(boost_factor(1_000_000) <= 1.5 + 1e-9);
    }

    #[test]
    fn record_persists_and_boosts() {
        let dir = std::env::temp_dir().join(format!("ms-pers-{}", nanos()));
        let path = dir.join("usage.json");
        let p = Personalization::load(&path);
        p.record(&["github".into(), "github".into(), "wikipedia".into()]);

        // Reload from disk: counts survived.
        let p2 = Personalization::load(&path);
        let counts = p2.counts();
        assert_eq!(counts.get("github"), Some(&2));
        assert_eq!(counts.get("wikipedia"), Some(&1));

        let mut weights: HashMap<String, f64> =
            [("github".to_string(), 1.0), ("brave".to_string(), 1.0)]
                .into_iter()
                .collect();
        p2.apply_boost(&mut weights);
        assert!(weights["github"] > 1.0); // boosted (used)
        assert_eq!(weights["brave"], 1.0); // untouched (never used)

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn nanos() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }
}
