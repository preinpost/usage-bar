//! Grok (xAI): read ~/.grok/auth.json, query CLI-proxy billing, local signals.

use chrono::{DateTime, Local};
use serde_json::Value;
use walkdir::WalkDir;

use crate::config;
use crate::http;
use crate::model::GrokStatus;

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

pub fn collect(start: DateTime<Local>) -> GrokStatus {
    let (local_sessions, local_tokens) = scan_sessions(start);
    let token = read_auth()
        .map(|(t, _)| t)
        .or_else(|| std::env::var("GROK_OAUTH_TOKEN").ok());
    let Some(token) = token else {
        return GrokStatus {
            needs_login: true,
            local_sessions,
            local_tokens,
            ..Default::default()
        };
    };
    let d = match http::get_json(
        "https://cli-chat-proxy.grok.com/v1/billing?format=credits",
        &[
            ("authorization", &format!("Bearer {token}")),
            ("x-xai-token-auth", "xai-grok-cli"),
            ("accept", "application/json"),
        ],
        8,
    ) {
        Ok(v) => v,
        Err(e) => {
            return GrokStatus {
                needs_login: e.starts_with("HTTP 401") || e.starts_with("HTTP 403"),
                error: Some(e),
                local_sessions,
                local_tokens,
                ..Default::default()
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
            local_sessions,
            local_tokens,
            ..Default::default()
        };
    }
    GrokStatus {
        needs_login: false,
        error: None,
        used_pct: pct,
        resets_at: resets,
        local_sessions,
        local_tokens,
    }
}
