//! GitHub Copilot: OAuth device flow + internal usage API.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::config::Config;
use crate::http;
use crate::model::{CopilotQuota, CopilotStatus, Status};

const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";

fn token_path(cfg: &Config) -> PathBuf {
    cfg.secrets_dir.join("copilot.json")
}

/// All directories that may hold a saved token (herdr-managed or standalone).
fn token_candidates(cfg: &Config) -> Vec<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut out = vec![token_path(cfg)];
    if let Ok(env_dir) = std::env::var("HERDR_PLUGIN_CONFIG_DIR") {
        if !env_dir.is_empty() {
            out.push(PathBuf::from(env_dir).join("secrets").join("copilot.json"));
        }
    }
    out.push(
        PathBuf::from(&home)
            .join(".config")
            .join("herdr")
            .join("plugins")
            .join("config")
            .join("local.codexbar")
            .join("secrets")
            .join("copilot.json"),
    );
    out.push(
        PathBuf::from(&home)
            .join(".config")
            .join("codexbar-status")
            .join("secrets")
            .join("copilot.json"),
    );
    out
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn load_token(cfg: &Config) -> Option<String> {
    for p in token_candidates(cfg) {
        if std::env::var("CODEBARX_DEBUG").is_ok() {
            eprintln!("load_token path: {}", p.display());
        }
        if !p.exists() {
            continue;
        }
        let text = std::fs::read_to_string(&p).ok()?;
        let v: Value = serde_json::from_str(&text).ok()?;
        let tok = v.get("access_token")?.as_str()?.to_string();
        let created = v
            .get("created_at")
            .and_then(|x| x.as_u64().or_else(|| x.as_f64().map(|f| f as u64)))
            .unwrap_or(0);
        if std::env::var("CODEBARX_DEBUG").is_ok() {
            eprintln!(
                "  found token, created={created} now={} expired={}",
                now(),
                created + 7 * 86400 < now()
            );
        }
        if tok.is_empty() || created + 7 * 86400 < now() {
            continue; // expired/empty -> try next location
        }
        return Some(tok);
    }
    None
}

fn as_u64(v: Option<&Value>) -> u64 {
    v.and_then(|x| x.as_u64())
        .or_else(|| v.and_then(|x| x.as_f64()).map(|f| f.max(0.0) as u64))
        .unwrap_or(0)
}

pub fn save_token(cfg: &Config, token: &str) {
    let _ = std::fs::create_dir_all(&cfg.secrets_dir);
    let data = serde_json::json!({ "access_token": token, "created_at": now() });
    if let Ok(text) = serde_json::to_string(&data) {
        let p = token_path(cfg);
        let _ = std::fs::write(&p, text);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
        }
    }
}

pub fn clear_token(cfg: &Config) {
    let _ = std::fs::remove_file(token_path(cfg));
}

/// Run the GitHub device flow. `progress` receives human-readable lines shown
/// in the UI as they happen. Returns the saved access token.
pub fn device_login(cfg: &Config, progress: impl Fn(&str)) -> Option<String> {
    let r = http::post_form_json(
        "https://github.com/login/device/code",
        &[("client_id", CLIENT_ID), ("scope", "read:user")],
        &[("accept", "application/json")],
        15,
    )
    .ok()?;
    let device_code = r.get("device_code")?.as_str()?;
    let user_code = r.get("user_code").and_then(|x| x.as_str()).unwrap_or("?");
    let verification = r
        .get("verification_uri")
        .and_then(|x| x.as_str())
        .unwrap_or("https://github.com/login/device");
    let interval = r.get("interval").and_then(|x| x.as_u64()).unwrap_or(5);
    let expires = r.get("expires_in").and_then(|x| x.as_u64()).unwrap_or(899);
    progress(&format!(
        "1) Open {verification}\n2) Enter code:  {user_code}\nWaiting for authorization… (close the popup to cancel)"
    ));
    let deadline = now() + expires;
    while now() < deadline {
        std::thread::sleep(std::time::Duration::from_secs(interval.max(1)));
        let tok = http::post_form_json(
            "https://github.com/login/oauth/access_token",
            &[
                ("client_id", CLIENT_ID),
                ("device_code", device_code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ],
            &[("accept", "application/json")],
            15,
        );
        match tok {
            Ok(v) => {
                if let Some(at) = v.get("access_token").and_then(|x| x.as_str()) {
                    save_token(cfg, at);
                    progress("✓ token saved");
                    return Some(at.to_string());
                }
                match v.get("error").and_then(|x| x.as_str()) {
                    Some("authorization_pending") => {}
                    Some("slow_down") => std::thread::sleep(std::time::Duration::from_secs(5)),
                    Some(e) => {
                        progress(&format!("login failed: {e}"));
                        return None;
                    }
                    None => {}
                }
            }
            Err(e) => {
                progress(&format!("poll error: {e}"));
                return None;
            }
        }
    }
    progress("login timed out");
    None
}

/// Fetch usage from the internal Copilot API.
pub fn collect(cfg: &Config) -> Status {
    let Some(tok) = load_token(cfg) else {
        return Status::NeedsLogin {
            hint: "login-copilot (L)".into(),
        };
    };
    let auth = format!("token {tok}");
    let headers = [
        ("authorization", auth.as_str()),
        ("accept", "application/json"),
        ("editor-version", "vscode/1.96.2"),
        ("editor-plugin-version", "copilot-chat/0.26.7"),
        ("user-agent", "GitHubCopilotChat/0.26.7"),
        ("x-github-api-version", "2025-04-01"),
    ];
    let d = match http::get_json("https://api.github.com/copilot_internal/user", &headers, 10) {
        Ok(v) => v,
        Err(e) => {
            if e.starts_with("HTTP 401") || e.starts_with("HTTP 403") {
                return Status::NeedsLogin {
                    hint: "token expired — login-copilot (L)".into(),
                };
            }
            return Status::Err { msg: e };
        }
    };
    let plan = d
        .get("copilot_plan")
        .or_else(|| d.get("copilotPlan"))
        .and_then(|x| x.as_str())
        .unwrap_or("unknown")
        .to_string();
    let reset = d
        .get("quota_reset_date")
        .or_else(|| d.get("quota_reset_date_utc"))
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .chars()
        .take(10)
        .collect();
    let login = d
        .get("login")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let snap = d.get("quota_snapshots").or_else(|| d.get("quotaSnapshots"));
    let mut quotas = Vec::new();
    const FIELDS: [(&str, &str); 3] = [
        // (api key, display label)
        ("premium_interactions", "AI credits"),
        ("chat", "chat"),
        ("completions", "completions"),
    ];
    for (key, label) in FIELDS {
        let Some(q) = snap.and_then(|s| s.get(key)) else {
            continue;
        };
        let unlimited = q
            .get("unlimited")
            .and_then(|x| x.as_bool())
            .unwrap_or(false);
        let used_pct = q
            .get("percent_remaining")
            .and_then(|x| x.as_f64())
            .map(|r| (100.0 - r).round() as u8);
        quotas.push(CopilotQuota {
            name: label.to_string(),
            used_pct: if unlimited { None } else { used_pct },
            unlimited,
            // token-based plans surface the raw AI-credit numbers here
            used: as_u64(q.get("credits_used")),
            entitlement: as_u64(q.get("entitlement")),
        });
    }
    if quotas.is_empty() {
        return Status::Err {
            msg: "no quota data in API response".into(),
        };
    }
    Status::Ok(CopilotStatus {
        plan,
        reset,
        login,
        quotas,
    })
}
