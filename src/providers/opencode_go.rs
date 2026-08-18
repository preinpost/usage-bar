//! OpenCode Go (opencode.ai): official quota/usage API.
//!
//! Key sources, in order: `OPENCODE_API_KEY` env, an explicitly saved key
//! (OS keyring, with the old `secrets/opencode-go.json` 0600 file as fallback
//! — see `crate::secrets`), then auto-detection from the pi credential store
//! (`~/.pi/agent/auth.json`) or the opencode CLI
//! (`~/.local/share/opencode/auth.json`). The official usage endpoint
//! (`GET /zen/go/v1/usage`, added upstream in "feat(console): add go usage
//! endpoint") reports the rolling / weekly / monthly quota windows for the
//! workspace tied to that key:
//!
//! ```json
//! { "usage": {
//!     "rolling": { "status": "ok",           "percent": 12, "resetsAt": "…" },
//!     "weekly":  { "status": "ok",           "percent": 5,  "resetsAt": "…" },
//!     "monthly": { "status": "rate-limited", "percent": 100,"resetsAt": "…" } } }
//! ```
//!
//! 401 = bad key, 403 = key is valid but the workspace has no active Go
//! subscription (free tier — quota is enforced per request, not reported).

use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, Local, NaiveDateTime, TimeZone};
use serde_json::Value;

use crate::config::Config;
use crate::http;
use crate::model::{GoUsageStatus, GoUsageWindow};

/// Official OpenCode Go usage/quota endpoint.
const USAGE_URL: &str = "https://opencode.ai/zen/go/v1/usage";

/// Account name in the OS keyring / fallback file (`secrets/opencode-go.json`).
const ACCOUNT: &str = "opencode-go";

/// Parse the saved key from its JSON blob (`api_key` or legacy `key`).
fn parse_saved_key(text: &str) -> Option<String> {
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

/// `OPENCODE_API_KEY` env → saved key (keyring, file fallback) → auto-detect
/// from pi credential store (`~/.pi/agent/auth.json`) → opencode CLI
/// (`~/.local/share/opencode/auth.json`). Auto-detection exists so a key
/// already provisioned by pi/opencode works out of the box; saving a key via
/// `login opencode` or the TUI overrides it for every harness.
pub fn load_key(cfg: &Config) -> Option<String> {
    if let Ok(k) = std::env::var("OPENCODE_API_KEY") {
        let k = k.trim().to_string();
        if !k.is_empty() {
            return Some(k);
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    load_key_from(&home, cfg)
}

/// Key lookup below the env check, with an explicit home root (injectable so
/// tests can isolate the auto-detection paths).
fn load_key_from(home: &str, cfg: &Config) -> Option<String> {
    // 1) explicitly saved key (OS keyring first, 0600 file fallback)
    if let Some(k) = crate::secrets::read_json(cfg, ACCOUNT).and_then(|j| parse_saved_key(&j)) {
        return Some(k);
    }
    // 2) pi's credential store — opencode-go used from pi keeps its key here
    if let Some(k) = auth_key(
        &PathBuf::from(home).join(".pi/agent/auth.json"),
        ["opencode-go", "opencode"],
    ) {
        return Some(k);
    }
    // 3) opencode CLI auth
    auth_key(
        &PathBuf::from(home).join(".local/share/opencode/auth.json"),
        ["opencode-go", "opencode"],
    )
}

/// Save an explicitly pasted key: OS keyring when available, else the 0600
/// `secrets/opencode-go.json` fallback file.
pub fn save_key(cfg: &Config, key: &str) {
    let data = serde_json::json!({ "api_key": key.trim() });
    if let Ok(text) = serde_json::to_string(&data) {
        crate::secrets::write_json(cfg, ACCOUNT, &text);
    }
}

pub fn clear_key(cfg: &Config) {
    crate::secrets::delete(cfg, ACCOUNT);
}

fn auth_key<const N: usize>(path: &Path, prefs: [&str; N]) -> Option<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return None;
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return None;
    };
    for pref in prefs {
        let Some(ent) = v.get(pref).and_then(|x| x.as_object()) else {
            continue;
        };
        let k = ent.get("key").and_then(|x| x.as_str()).unwrap_or("").trim();
        if !k.is_empty() {
            return Some(k.to_string());
        }
    }
    None
}

fn parse_window(v: &Value) -> Option<GoUsageWindow> {
    let status = v.get("status").and_then(|x| x.as_str())?.to_string();
    let percent = v
        .get("percent")
        .and_then(|x| x.as_u64().or_else(|| x.as_f64().map(|f| f as u64)))
        .unwrap_or(0)
        .min(100) as u8;
    let resets_at = v
        .get("resetsAt")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    Some(GoUsageWindow {
        status,
        percent,
        resets_at,
    })
}

pub fn collect(cfg: &Config) -> GoUsageStatus {
    let Some(key) = load_key(cfg) else {
        return GoUsageStatus {
            needs_key: true,
            ..Default::default()
        };
    };
    let auth = format!("Bearer {key}");
    let headers = [
        ("authorization", auth.as_str()),
        ("accept", "application/json"),
    ];

    let d = match http::get_json(USAGE_URL, &headers, 8) {
        Ok(v) => v,
        Err(e) => {
            // A key was found but the endpoint refused it: keep the provider
            // visible with an error (like OpenRouter) instead of hiding it.
            let msg = match e.as_str() {
                "HTTP 401" => "invalid key (HTTP 401)".into(),
                // Key is valid but the workspace has no committed quota rows —
                // free tier or a console account without an active Go plan.
                "HTTP 403" => "no Go subscription (free tier)".into(),
                other => other.to_string(),
            };
            return GoUsageStatus {
                needs_key: false,
                error: Some(msg),
                ..Default::default()
            };
        }
    };
    let usage = d.get("usage").and_then(|x| x.as_object());
    GoUsageStatus {
        needs_key: false,
        error: None,
        subscribed: usage.is_some(),
        rolling: usage.and_then(|u| u.get("rolling")).and_then(parse_window),
        weekly: usage.and_then(|u| u.get("weekly")).and_then(parse_window),
        monthly: usage.and_then(|u| u.get("monthly")).and_then(parse_window),
    }
}

/// Compact reset time for a window's `resetsAt` ISO timestamp:
/// `03:00` when it lands today, `08-25 03:00` otherwise.
pub fn format_resets_at(iso: Option<&str>) -> String {
    let Some(iso) = iso else { return "—".into() };
    let parsed = DateTime::parse_from_rfc3339(iso)
        .map(|d| d.with_timezone(&Local))
        .ok()
        .or_else(|| {
            NaiveDateTime::parse_from_str(iso, "%Y-%m-%dT%H:%M:%S%.fZ")
                .ok()
                .and_then(|d| Local.from_local_datetime(&d).single())
        });
    let Some(t) = parsed else {
        return iso.chars().take(16).collect();
    };
    let now = Local::now();
    let same_day = (t.year(), t.month(), t.day()) == (now.year(), now.month(), now.day());
    if same_day {
        t.format("%H:%M").to_string()
    } else {
        t.format("%m-%d %H:%M").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering as AO};

    fn tmp_dir() -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, AO::SeqCst);
        let d =
            std::env::temp_dir().join(format!("usage-bar-ogo-test-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn cfg_with(dir: &Path) -> Config {
        // Never touch the real macOS Keychain / Linux secret-service in tests:
        // everything here exercises the 0600-file fallback path instead.
        crate::secrets::force_file_mode_for_tests();
        Config {
            claude_budget: 0,
            codex_budget: 0,
            opencode_budget: 0,
            reset_hour: 0,
            refresh_seconds: 30,
            show_no_data_providers: false,
            prices: crate::config::Prices::default(),
            secrets_dir: dir.join("secrets"),
        }
    }

    #[test]
    fn saved_key_takes_priority_over_auto_detection() {
        let dir = tmp_dir();
        let cfg = cfg_with(&dir);
        let home = dir.join("home");
        // simulate a different workspace key provisioned by pi
        let pi_auth = home.join(".pi/agent/auth.json");
        std::fs::create_dir_all(pi_auth.parent().unwrap()).unwrap();
        std::fs::write(
            &pi_auth,
            r#"{"opencode-go":{"type":"api","key":"sk-pi-workspace"}}"#,
        )
        .unwrap();

        // no saved key yet → auto-detected from pi
        assert_eq!(
            load_key_from(home.to_str().unwrap(), &cfg).as_deref(),
            Some("sk-pi-workspace")
        );

        // save an explicit key → it wins
        save_key(&cfg, "sk-pasted");
        assert_eq!(
            load_key_from(home.to_str().unwrap(), &cfg).as_deref(),
            Some("sk-pasted")
        );

        // clearing falls back to auto-detection for pi again
        clear_key(&cfg);
        assert_eq!(
            load_key_from(home.to_str().unwrap(), &cfg).as_deref(),
            Some("sk-pi-workspace")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_key_anywhere() {
        let dir = tmp_dir();
        let cfg = cfg_with(&dir);
        let home = dir.join("empty-home");
        std::fs::create_dir_all(&home).unwrap();
        assert_eq!(load_key_from(home.to_str().unwrap(), &cfg), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saved_key_file_is_0600() {
        let dir = tmp_dir();
        let cfg = cfg_with(&dir);
        save_key(&cfg, "sk-secret");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(cfg.secrets_dir.join("opencode-go.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_full_payload() {
        let v: Value = serde_json::from_str(
            r#"{"usage":{
                "rolling":{"status":"ok","percent":12,"resetsAt":"2026-08-25T03:00:00.000Z"},
                "weekly":{"status":"ok","percent":5,"resetsAt":"2026-08-30T03:00:00.000Z"},
                "monthly":{"status":"rate-limited","percent":100,"resetsAt":"2026-09-01T00:00:00.000Z"}
            }}"#,
        )
        .unwrap();
        let usage = v.get("usage").unwrap();
        let rolling = parse_window(usage.get("rolling").unwrap()).unwrap();
        assert_eq!(rolling.status, "ok");
        assert_eq!(rolling.percent, 12);
        assert!(!rolling.rate_limited());
        let monthly = parse_window(usage.get("monthly").unwrap()).unwrap();
        assert!(monthly.rate_limited());
        assert_eq!(monthly.percent, 100);
    }

    #[test]
    fn resets_at_falls_back_on_garbage() {
        assert_eq!(format_resets_at(None), "—");
        assert_eq!(format_resets_at(Some("not-a-date")), "not-a-date");
    }
}
