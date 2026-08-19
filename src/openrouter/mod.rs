//! OpenRouter: public API (credits + key limit) plus the web-dashboard usage
//! sync (dashboard.rs pulls the Chrome session; usage.rs reads the cache).

mod dashboard;
mod logs;
mod usage;

pub use dashboard::sync_now;
pub use logs::{
    cell_ellipsized, fetch, fit_cell, full_cell, header_cells, log_table_width, row_cells,
    summarize, summary_cells, tps, LOG_COLUMNS, LOG_COL_SEP,
};
pub use usage::{is_stale, load, sync_async};

/// Can the web-dashboard (per-model model-usage) sync run here? macOS/Windows
/// scrape the Chrome session; other platforms have no supported cookie pull and
/// degrade to the public-API (credits / key / windowed spend) view only.
pub fn dashboard_supported() -> bool {
    cfg!(any(target_os = "macos", target_os = "windows"))
}

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::config::Config;
use crate::http;
use crate::model::OpenRouterStatus;

const BASE: &str = "https://openrouter.ai/api/v1";

fn key_path(cfg: &Config) -> PathBuf {
    cfg.secrets_dir.join("openrouter.json")
}

/// Account name in the OS keyring / fallback file (`secrets/openrouter.json`).
const ACCOUNT: &str = "openrouter";

fn parse_key(text: &str) -> Option<String> {
    let v: Value = serde_json::from_str(text).ok()?;
    let k = v
        .get("api_key")
        .or_else(|| v.get("key"))
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim();
    if k.is_empty() {
        None
    } else {
        Some(k.to_string())
    }
}

/// Key sources: `OPENROUTER_API_KEY` env first, then the OS keyring (with the
/// `secrets_dir` fallback file), then the standalone default config dir.
pub fn load_key(cfg: &Config) -> Option<String> {
    if let Ok(k) = std::env::var("OPENROUTER_API_KEY") {
        let k = k.trim().to_string();
        if !k.is_empty() {
            return Some(k);
        }
    }
    // 1) OS keyring, with the `secrets_dir` file as its fallback
    if let Some(k) = crate::secrets::read_json(cfg, ACCOUNT).and_then(|j| parse_key(&j)) {
        return Some(k);
    }
    // 2) standalone default config dir (legacy sharing with the plugin)
    let mut candidates = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        let default = PathBuf::from(home)
            .join(".config")
            .join("usage-bar")
            .join("secrets")
            .join("openrouter.json");
        if default != key_path(cfg) {
            candidates.push(default);
        }
    }
    for p in candidates {
        if let Some(k) = crate::secrets::read_file(&p).and_then(|j| parse_key(&j)) {
            return Some(k);
        }
    }
    None
}

pub fn save_key(cfg: &Config, key: &str) {
    let data = serde_json::json!({ "api_key": key.trim(), "saved_at": now() });
    if let Ok(text) = serde_json::to_string(&data) {
        crate::secrets::write_json(cfg, ACCOUNT, &text);
    }
}

pub fn clear_key(cfg: &Config) {
    crate::secrets::delete(cfg, ACCOUNT);
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn num(o: &Value, key: &str) -> f64 {
    o.get(key)
        .and_then(|x| x.as_f64().or_else(|| x.as_u64().map(|n| n as f64)))
        .unwrap_or(0.0)
}

/// Fetch credits balance + (optional) key spending limit. Mirror of the
/// OpenRouter provider: `/credits` is the required source, `/key` is
/// a best-effort enrichment (may be slow/unavailable → degrade to credits only).
pub fn collect(cfg: &Config) -> OpenRouterStatus {
    let Some(key) = load_key(cfg) else {
        return OpenRouterStatus {
            needs_key: true,
            ..Default::default()
        };
    };
    let auth = format!("Bearer {key}");
    let headers = [
        ("authorization", auth.as_str()),
        ("accept", "application/json"),
    ];

    let d = match http::get_json(&format!("{BASE}/credits"), &headers, 8) {
        Ok(v) => v,
        // A key is present but rejected (401/403) or unreachable: keep the
        // provider visible with an error so the user knows to fix the key.
        Err(e) => {
            return OpenRouterStatus {
                needs_key: false,
                error: Some(e),
                ..Default::default()
            };
        }
    };
    let data = d
        .get("data")
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or_default();
    let total_credits = num(&data, "total_credits");
    let total_usage = num(&data, "total_usage");
    let balance = (total_credits - total_usage).max(0.0);

    let mut st = OpenRouterStatus {
        needs_key: false,
        balance_usd: balance,
        total_credits_usd: total_credits,
        total_usage_usd: total_usage,
        ..Default::default()
    };

    if let Ok(kd) = http::get_json(&format!("{BASE}/key"), &headers, 5) {
        let Some(k) = kd.get("data").filter(|v| v.is_object()) else {
            return st;
        };
        // key-level spend windows — populate whenever the endpoint reports them
        st.usage_today = num(k, "usage_daily");
        st.usage_week = num(k, "usage_weekly");
        st.usage_month = num(k, "usage_monthly");
        let limit = num(k, "limit");
        if !(limit.is_finite() && limit > 0.0) {
            // no configured spending limit → the meter stays on credits only
            st.reset_window = k
                .get("limit_reset")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string());
            return st;
        }
        // Prefer the server-reported remaining amount, else the usage field for
        // the declared reset window, else cumulative usage.
        let used = if k.get("limit_remaining").is_some() {
            let remaining = num(k, "limit_remaining");
            limit - remaining.clamp(0.0, limit)
        } else {
            let win_key = match k.get("limit_reset").and_then(|x| x.as_str()).unwrap_or("") {
                "daily" => "usage_daily",
                "weekly" => "usage_weekly",
                "monthly" => "usage_monthly",
                _ => "usage",
            };
            num(k, win_key).max(0.0)
        };
        st.key_limit_usd = Some(limit);
        st.key_used_usd = Some(used.max(0.0));
        st.key_remaining_usd = Some((limit - used).max(0.0));
        st.used_pct = Some((((used / limit) * 100.0).round().clamp(0.0, 100.0)) as u8);
        st.reset_window = k
            .get("limit_reset")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string());
    }
    st
}
