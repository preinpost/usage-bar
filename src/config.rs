//! Config: HERDR_PLUGIN_CONFIG_DIR/config.json or ~/.config/codexbar-status/config.json

use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Config {
    pub claude_budget: u64,
    pub codex_budget: u64,
    pub opencode_budget: u64,
    pub reset_hour: u32,
    pub refresh_seconds: u64,
    /// also show providers that are not connected / have no usage this window
    pub show_no_data_providers: bool,
    pub prices: Prices,
    pub secrets_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Prices {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

impl Default for Prices {
    fn default() -> Self {
        Self { input: 3.0, output: 15.0, cache_read: 0.30, cache_write: 3.75 }
    }
}

fn home() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/"))
}

pub fn plugin_config_dir() -> PathBuf {
    std::env::var("HERDR_PLUGIN_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".config").join("codexbar-status"))
}

pub fn secrets_dir() -> PathBuf {
    plugin_config_dir().join("secrets")
}

pub fn state_dir() -> PathBuf {
    std::env::var("HERDR_PLUGIN_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".config").join("codexbar-status").join("state"))
}

pub fn load() -> Config {
    let mut cfg = Config {
        claude_budget: 10_000_000,
        codex_budget: 10_000_000,
        opencode_budget: 10_000_000,
        reset_hour: 0,
        refresh_seconds: 30,
        show_no_data_providers: false,
        prices: Prices::default(),
        secrets_dir: secrets_dir(),
    };
    let candidates = [
        plugin_config_dir().join("config.json"),
        home().join(".config").join("codexbar-status").join("config.json"),
    ];
    for p in candidates {
        if let Ok(text) = std::fs::read_to_string(&p) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(n) = v.get("claude_daily_budget_tokens").and_then(|x| x.as_u64()) {
                    cfg.claude_budget = n;
                }
                if let Some(n) = v.get("codex_daily_budget_tokens").and_then(|x| x.as_u64()) {
                    cfg.codex_budget = n;
                }
                if let Some(n) = v.get("opencode_daily_budget_tokens").and_then(|x| x.as_u64()) {
                    cfg.opencode_budget = n;
                }
                if let Some(n) = v.get("reset_hour").and_then(|x| x.as_u64()) {
                    cfg.reset_hour = n as u32;
                }
                if let Some(n) = v.get("refresh_seconds").and_then(|x| x.as_u64()) {
                    cfg.refresh_seconds = n.max(5);
                }
                if let Some(b) = v.get("show_no_data_providers").and_then(|x| x.as_bool()) {
                    cfg.show_no_data_providers = b;
                }
                if let Some(pr) = v.get("prices").and_then(|x| x.as_object()) {
                    if let Some(n) = pr.get("input").and_then(|x| x.as_f64()) {
                        cfg.prices.input = n;
                    }
                    if let Some(n) = pr.get("output").and_then(|x| x.as_f64()) {
                        cfg.prices.output = n;
                    }
                    if let Some(n) = pr.get("cache_read").and_then(|x| x.as_f64()) {
                        cfg.prices.cache_read = n;
                    }
                    if let Some(n) = pr.get("cache_write").and_then(|x| x.as_f64()) {
                        cfg.prices.cache_write = n;
                    }
                }
                break;
            }
        }
    }
    cfg
}

pub fn ensure_dirs(cfg: &Config) {
    let _ = std::fs::create_dir_all(&cfg.secrets_dir);
    let _ = std::fs::create_dir_all(state_dir());
}

pub fn claude_projects_dir() -> PathBuf {
    home().join(".claude").join("projects")
}
pub fn codex_sessions_dir() -> PathBuf {
    home().join(".codex").join("sessions")
}
pub fn codex_history_path() -> PathBuf {
    home().join(".codex").join("history.jsonl")
}
pub fn opencode_db_path() -> PathBuf {
    home().join(".local").join("share").join("opencode").join("opencode.db")
}
pub fn grok_home() -> PathBuf {
    Path::new(
        &std::env::var("GROK_HOME").unwrap_or_else(|_| home().join(".grok").to_string_lossy().to_string()),
    )
    .to_path_buf()
}