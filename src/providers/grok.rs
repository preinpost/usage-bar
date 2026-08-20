//! Grok (xAI): OAuth device flow → keyring token with auto-refresh, then
//! CLI-proxy billing, plus local session signals.
//!
//! No Grok CLI install required: `ub login grok` runs xAI's standard device
//! authorization flow (browser at accounts.x.ai + a one-time code) and stores
//! the tokens in the OS keyring via `crate::secrets`, like Copilot. When the
//! access token nears expiry the stored `refresh_token` is exchanged
//! transparently, so the panel keeps working without re-login. The legacy
//! Grok CLI file (`~/.grok/auth.json`) and `GROK_OAUTH_TOKEN` still work as
//! fallbacks for users who logged in through the CLI.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Local};
use serde_json::{Value, json};
use walkdir::WalkDir;

use crate::config;
use crate::config::Config;
use crate::http;
use crate::model::GrokStatus;

/// Public xAI OAuth app the Grok CLI authenticates against — verified live
/// against auth.x.ai's device-authorization and token endpoints.
const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
/// Scope observed on working Grok CLI tokens. `offline_access` is what makes
/// x.ai return a refresh token so usage-bar can renew without re-login.
const SCOPE: &str = "openid profile email offline_access grok-cli:access";
/// Keyring account (fallback file `secrets/grok.json`).
const ACCOUNT: &str = "grok";
const DEVICE_CODE_URL: &str = "https://auth.x.ai/oauth2/device/code";
const TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
const BILLING_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing?format=credits";
/// Refresh (instead of serving) a stored token when it is this close to
/// expiry, so a single poll never rides a dead token into "needs login".
const REFRESH_BEFORE_SECS: u64 = 120;

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Read a numeric field that may arrive as int, float, or numeric string.
fn num(v: &Value, key: &str) -> u64 {
    v.get(key)
        .and_then(|x| x.as_u64().or_else(|| x.as_f64().map(|f| f.max(0.0) as u64)))
        .unwrap_or(0)
}

// ---------------- Grok CLI auth file (`~/.grok/auth.json`) ----------------

fn read_auth() -> Option<(String, String)> {
    // returns (token, email)
    let candidates = [
        std::env::var("GROK_AUTH_FILE")
            .ok()
            .map(std::path::PathBuf::from),
        Some(config::grok_home().join("auth.json")),
    ];
    for p in candidates.into_iter().flatten() {
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let Some(obj) = v.as_object() else { continue };
        let keys: Vec<&String> = obj.keys().collect();
        for pref in ["https://auth.x.ai::", "https://accounts.x.ai/sign-in"] {
            for k in &keys {
                if !k.starts_with(pref) {
                    continue;
                }
                let Some(ent) = obj.get(*k).and_then(|x| x.as_object()) else {
                    continue;
                };
                let Some(key) = ent.get("key").and_then(|x| x.as_str()) else {
                    continue;
                };
                if key.is_empty() {
                    continue;
                }
                let exp = ent.get("expires_at").and_then(|x| x.as_i64());
                let now = chrono::Utc::now().timestamp();
                if let Some(e) = exp {
                    if e < now {
                        continue; // expired, try next entry
                    }
                }
                let email = ent
                    .get("email")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                return Some((key.to_string(), email));
            }
        }
    }
    None
}

// ------------- usage-bar keyring token (device login, refreshable) ---------

struct StoredToken {
    access: String,
    refresh: String,
    /// Unix seconds when the access token expires; 0 when unrecoverable, in
    /// which case the token is served until the server rejects it.
    expires_at: u64,
}

fn parse_stored(text: &str) -> Option<StoredToken> {
    let v: Value = serde_json::from_str(text).ok()?;
    let access = v.get("access_token")?.as_str()?.to_string();
    if access.is_empty() {
        return None;
    }
    let refresh = v
        .get("refresh_token")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let created = num(&v, "created_at");
    let expires_in = num(&v, "expires_in");
    let expires_at = if created > 0 && expires_in > 0 {
        created + expires_in
    } else {
        0
    };
    Some(StoredToken {
        access,
        refresh,
        expires_at,
    })
}

fn save_stored(cfg: &Config, access: &str, refresh: &str, expires_in: u64) {
    let blob = json!({
        "access_token": access,
        "refresh_token": refresh,
        "created_at": now(),
        "expires_in": expires_in,
    });
    if let Ok(text) = serde_json::to_string(&blob) {
        crate::secrets::write_json(cfg, ACCOUNT, &text);
    }
}

/// Exchange a stored refresh token for a fresh access token, rewriting the
/// keyring blob (keeps the old refresh token when the server returns none).
fn refresh_stored(cfg: &Config, stored: &StoredToken) -> Option<String> {
    if stored.refresh.is_empty() {
        return None;
    }
    let v = http::post_form_json(
        TOKEN_URL,
        &[
            ("client_id", CLIENT_ID),
            ("refresh_token", &stored.refresh),
            ("grant_type", "refresh_token"),
        ],
        &[("accept", "application/json")],
        15,
    )
    .ok()?;
    let access = v.get("access_token")?.as_str()?;
    let refresh = v
        .get("refresh_token")
        .and_then(|x| x.as_str())
        .unwrap_or(&stored.refresh);
    save_stored(cfg, access, refresh, num(&v, "expires_in"));
    Some(access.to_string())
}

/// Primary auth source: the keyring-stored device-flow token, refreshed
/// transparently near expiry. Returns `None` when there is no usable stored
/// token (or refresh failed), so callers fall through to legacy sources.
fn token_from_keyring(cfg: &Config) -> Option<String> {
    let text = crate::secrets::read_json(cfg, ACCOUNT)?;
    let stored = parse_stored(&text)?;
    if stored.expires_at == 0 || now() < stored.expires_at.saturating_sub(REFRESH_BEFORE_SECS) {
        return Some(stored.access);
    }
    refresh_stored(cfg, &stored)
}

fn load_token(cfg: &Config) -> Option<String> {
    if let Some(t) = token_from_keyring(cfg) {
        return Some(t);
    }
    read_auth()
        .map(|(t, _)| t)
        .or_else(|| std::env::var("GROK_OAUTH_TOKEN").ok())
}

/// Run the xAI device flow (no CLI install needed). `progress` receives
/// human-readable lines shown in the UI as they happen. On success the
/// access + refresh tokens are stored in the keyring; returns the access token.
pub fn device_login(cfg: &Config, progress: impl Fn(&str)) -> Option<String> {
    let r = http::post_form_json(
        DEVICE_CODE_URL,
        &[("client_id", CLIENT_ID), ("scope", SCOPE)],
        &[("accept", "application/json")],
        15,
    )
    .ok()?;
    let device_code = r.get("device_code")?.as_str()?;
    let user_code = r.get("user_code").and_then(|x| x.as_str()).unwrap_or("?");
    let verification = r
        .get("verification_uri")
        .and_then(|x| x.as_str())
        .unwrap_or("https://accounts.x.ai/oauth2/device");
    let interval = r.get("interval").and_then(|x| x.as_u64()).unwrap_or(5);
    let expires = r.get("expires_in").and_then(|x| x.as_u64()).unwrap_or(1800);
    progress(&format!(
        "1) Open {verification}\n2) Enter code:  {user_code}\nWaiting for authorization… (close the popup to cancel)"
    ));
    let deadline = now() + expires;
    while now() < deadline {
        std::thread::sleep(Duration::from_secs(interval.max(1)));
        let tok = http::post_form_json(
            TOKEN_URL,
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
                    let refresh = v
                        .get("refresh_token")
                        .and_then(|x| x.as_str())
                        .unwrap_or("");
                    save_stored(cfg, at, refresh, num(&v, "expires_in"));
                    progress("✓ token saved");
                    return Some(at.to_string());
                }
                match v.get("error").and_then(|x| x.as_str()) {
                    Some("authorization_pending") => {}
                    Some("slow_down") => std::thread::sleep(Duration::from_secs(5)),
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

pub fn clear_token(cfg: &Config) {
    crate::secrets::delete(cfg, ACCOUNT);
}

// ------------------------------ usage scan --------------------------------

pub fn scan_sessions(start: DateTime<Local>) -> (u64, u64) {
    // (session_count, tokens)
    let base = config::grok_home().join("sessions");
    if !base.is_dir() {
        return (0, 0);
    }
    let mut n = 0u64;
    let mut toks = 0u64;
    for entry in WalkDir::new(&base).into_iter().filter_map(Result::ok) {
        let p = entry.path();
        if p.file_name().and_then(|s| s.to_str()) != Some("signals.json") {
            continue;
        }
        let Ok(md) = std::fs::metadata(p) else {
            continue;
        };
        if let Ok(mt) = md.modified() {
            let t: DateTime<Local> = mt.into();
            if t < start {
                continue;
            }
        }
        let Ok(text) = std::fs::read_to_string(p) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        n += 1;
        toks += v
            .get("contextTokensUsed")
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
    }
    (n, toks)
}

// ------------------------------- billing ----------------------------------

fn is_auth_err(e: &str) -> bool {
    e.starts_with("HTTP 401") || e.starts_with("HTTP 403")
}

fn fetch_billing(token: &str) -> Result<Value, String> {
    http::get_json(
        BILLING_URL,
        &[
            ("authorization", &format!("Bearer {token}")),
            ("x-xai-token-auth", "xai-grok-cli"),
            ("accept", "application/json"),
        ],
        8,
    )
}

pub fn collect(cfg: &Config, start: DateTime<Local>) -> GrokStatus {
    let (local_sessions, local_tokens) = scan_sessions(start);
    let base = GrokStatus {
        local_sessions,
        local_tokens,
        ..Default::default()
    };
    let Some(mut token) = load_token(cfg) else {
        return GrokStatus {
            needs_login: true,
            ..base
        };
    };
    let d = match fetch_billing(&token) {
        Ok(v) => v,
        Err(e) if is_auth_err(&e) => {
            // token rejected server-side — force one refresh, then retry once
            if token_from_keyring(cfg).is_some() {
                if let Some(t2) = load_token(cfg) {
                    token = t2;
                }
                match fetch_billing(&token) {
                    Ok(v) => v,
                    Err(e2) => {
                        return GrokStatus {
                            needs_login: is_auth_err(&e2),
                            error: Some(e2),
                            ..base
                        };
                    }
                }
            } else {
                return GrokStatus {
                    needs_login: true,
                    error: Some(e),
                    ..base
                };
            }
        }
        Err(e) => {
            return GrokStatus {
                needs_login: false,
                error: Some(e),
                ..base
            };
        }
    };
    let cfgobj = d.get("config").and_then(|x| x.as_object());
    let mut pct = cfgobj
        .and_then(|c| c.get("creditUsagePercent"))
        .and_then(|x| x.as_f64());
    if pct.is_none() {
        let used = d
            .get("onDemandUsed")
            .and_then(|x| x.get("val"))
            .and_then(|x| x.as_i64())
            .unwrap_or(0);
        let cap = d
            .get("onDemandCap")
            .and_then(|x| x.get("val"))
            .and_then(|x| x.as_i64())
            .unwrap_or(1);
        if cap > 0 {
            pct = Some(used as f64 / cap as f64 * 100.0);
        }
    }
    let resets = cfgobj
        .and_then(|c| c.get("currentPeriod"))
        .and_then(|c| c.get("end"))
        .or_else(|| cfgobj.and_then(|c| c.get("billingPeriodEnd")))
        .and_then(|x| x.as_str())
        .map(|s| s.chars().take(10).collect());
    if pct.is_none() && resets.is_none() {
        return GrokStatus {
            needs_login: true,
            error: Some("empty billing payload".into()),
            ..base
        };
    }
    GrokStatus {
        needs_login: false,
        error: None,
        used_pct: pct,
        resets_at: resets,
        ..base
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_stored_reads_blob_and_rejects_empty() {
        let s = parse_stored(
            r#"{"access_token":"at","refresh_token":"rt","created_at":1000,"expires_in":900}"#,
        )
        .expect("valid blob");
        assert_eq!(s.access, "at");
        assert_eq!(s.refresh, "rt");
        assert_eq!(s.expires_at, 1900, "expires_at = created + expires_in");
        assert!(
            parse_stored(r#"{"refresh_token":"rt"}"#).is_none(),
            "no access"
        );
        assert!(
            parse_stored(r#"{"access_token":""}"#).is_none(),
            "empty access"
        );
        assert!(parse_stored("not json").is_none());
    }

    #[test]
    fn num_reads_integer_and_float() {
        let v = serde_json::json!({ "a": 5, "b": 5.0 });
        assert_eq!(num(&v, "a"), 5);
        assert_eq!(num(&v, "b"), 5);
        assert_eq!(num(&v, "missing"), 0);
        assert_eq!(num(&serde_json::json!({}), "x"), 0);
    }
}
