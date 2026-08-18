//! OpenRouter model-usage cache (written by `sync-openrouter` / `crate::or_sync`,
//! which pulls the Chrome session and queries the dashboard analytics API).

use std::path::PathBuf;

use serde_json::Value;

use crate::config::Config;
use crate::model::{ModelUsage, OpenRouterUsage};

fn cache_path() -> PathBuf {
    crate::config::state_dir().join("openrouter-usage.json")
}

fn models(arr: &Value) -> Vec<ModelUsage> {
    arr.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| {
                    let label = m
                        .get("label")
                        .and_then(|x| x.as_str())
                        .or_else(|| m.get("model").and_then(|x| x.as_str()))
                        .unwrap_or("")
                        .to_string();
                    if label.is_empty() {
                        return None;
                    }
                    let cost = m.get("cost").and_then(|x| x.as_f64()).unwrap_or(0.0);
                    let tokens = as_u64(m.get("tokens"));
                    Some(ModelUsage { label, cost, tokens })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn as_u64(v: Option<&Value>) -> u64 {
    v.and_then(|x| x.as_u64())
        .or_else(|| x_str_u64(v))
        .or_else(|| v.and_then(|x| x.as_f64()).map(|f| f.max(0.0) as u64))
        .unwrap_or(0)
}

fn x_str_u64(v: Option<&Value>) -> Option<u64> {
    v.and_then(|x| x.as_str()).and_then(|s| s.parse::<u64>().ok())
}

/// Read the usage cache written by `crate::or_sync::sync_now`.
pub fn load() -> Option<OpenRouterUsage> {
    let text = std::fs::read_to_string(cache_path()).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let get_total = |key: &str| {
        v.get(key)
            .and_then(|x| x.get("total"))
            .and_then(|x| x.as_f64())
            .unwrap_or(0.0)
    };
    Some(OpenRouterUsage {
        fetched_at: v.get("fetched_at").and_then(|x| x.as_i64()).unwrap_or(0),
        today_total: get_total("today"),
        today_models: v.get("today").and_then(|x| x.get("models")).map(models).unwrap_or_default(),
        month_total: get_total("month"),
        month_models: v.get("month").and_then(|x| x.get("models")).map(models).unwrap_or_default(),
    })
}

/// Is the cache fresh enough to trust (default 20 minutes)?
pub fn is_stale(u: &OpenRouterUsage, now_unix: i64) -> bool {
    now_unix - u.fetched_at > 20 * 60
}

/// Fire-and-forget refresh of the model-usage cache, fully internalized
/// (Chrome cookies + analytics-query via `crate::or_sync`).
pub fn sync_async(cfg: &Config) {
    let cfg = cfg.clone();
    std::thread::spawn(move || {
        let _ = crate::openrouter::sync_now(&cfg);
    });
}
