//! Internalized OpenRouter usage sync — replaces the former Python/shell
//! scripts. Pulls the user's Chrome login, decrypts the openrouter.ai session
//! cookies (macOS Keychain + AES-128-CBC), queries the web dashboard's
//! `analytics-query` endpoint and writes the per-model usage cache.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use aes::Aes128;
use cbc::Decryptor;
use cipher::block_padding::Pkcs7;
use cipher::{BlockDecryptMut, KeyIvInit};
use hmac::Hmac;
use pbkdf2::pbkdf2;
use rusqlite::Connection;
use serde_json::{Value, json};
use sha1::Sha1;

use crate::config::Config;
use crate::http;

use chrono::TimeZone as _;

type HmacSha1 = Hmac<Sha1>;
type Aes128CbcDec = Decryptor<Aes128>;

const ANALYTICS_URL: &str = "https://openrouter.ai/api/frontend/v1/private/analytics-query";
const COOKIE_HOST: &str = "%openrouter.ai%";

fn home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/"))
}

fn chrome_cookie_db() -> PathBuf {
    home()
        .join("Library")
        .join("Application Support")
        .join("Google")
        .join("Chrome")
        .join("Default")
        .join("Cookies")
}

/// 16-byte AES key: PBKDF2-HMAC-SHA1 of the Keychain "Chrome Safe Storage"
/// password (salt `saltysalt`, 1003 rounds).
fn chrome_key() -> Result<[u8; 16], String> {
    let out = std::process::Command::new("security")
        .args([
            "-q",
            "find-generic-password",
            "-w",
            "-a",
            "Chrome",
            "-s",
            "Chrome Safe Storage",
        ])
        .output()
        .map_err(|e| format!("keychain call failed: {e}"))?;
    if !out.status.success() {
        return Err("Chrome Safe Storage item not found in Keychain".into());
    }
    let password = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let mut key = [0u8; 16];
    pbkdf2::<HmacSha1>(password.as_bytes(), b"saltysalt", 1003, &mut key)
        .map_err(|e| format!("key derivation failed: {e}"))?;
    Ok(key)
}

/// Decrypt a macOS Chrome cookie: `v10` prefix, AES-128-CBC with the fixed
/// 16-space IV, PKCS#7 unpad, then drop the 32-byte domain-integrity prefix
/// newer Chrome builds prepend to the plaintext.
fn decrypt_cookie(enc: &[u8], key: &[u8; 16]) -> Result<String, String> {
    if enc.len() < 3 || &enc[..3] != b"v10" {
        return Err("unsupported cookie scheme".into());
    }
    let iv = [0x20u8; 16]; // b' ' * 16
    let mut buf = enc[3..].to_vec();
    let pt = Aes128CbcDec::new(key.into(), &iv.into())
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|_| "aes-128-cbc decrypt failed".to_string())?;
    let value = if pt.len() > 32 { &pt[32..] } else { pt };
    let s = String::from_utf8_lossy(value).to_string();
    if s.is_empty() {
        Err("empty cookie value".into())
    } else {
        Ok(s)
    }
}

struct OrCookie {
    name: String,
    value: String,
}

/// Read + decrypt every openrouter.ai cookie from the user's Chrome profile.
fn chrome_openrouter_cookies() -> Result<Vec<OrCookie>, String> {
    let db = chrome_cookie_db();
    if !db.exists() {
        return Err(format!("chrome cookie db not found: {}", db.display()));
    }
    let uri = format!("file:{}?mode=ro", db.display());
    let conn = Connection::open_with_flags(
        &uri,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| format!("open cookies db: {e}"))?;
    let key = chrome_key()?;

    let mut stmt = conn
        .prepare("SELECT name, encrypted_value FROM cookies WHERE host_key LIKE ?1")
        .map_err(|e| format!("query cookies: {e}"))?;
    let rows = stmt
        .query_map([COOKIE_HOST], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
        })
        .map_err(|e| format!("read cookies: {e}"))?;

    let mut out = Vec::new();
    for row in rows.flatten() {
        let (name, enc) = row;
        if name.starts_with("_ga") || name.starts_with("_dd_s") || name.starts_with("or_statsig") {
            continue;
        }
        if let Ok(value) = decrypt_cookie(&enc, &key) {
            out.push(OrCookie { name, value });
        }
    }
    Ok(out)
}

fn iso(dt: chrono::DateTime<chrono::Utc>) -> String {
    dt.format("%Y-%m-%dT%H:%M:%S.000Z").to_string()
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn query(cookie_header: &str, start: &str, end: &str) -> Result<Value, String> {
    let body = json!({
        "metrics": ["total_usage", "tokens_total"],
        "dimensions": ["model"],
        "time_range": { "start": start, "end": end },
        "order_by": { "field": "total_usage", "direction": "desc" },
        "limit": 40,
        "includeEnrichment": true,
    });
    http::post_json(
        ANALYTICS_URL,
        &body,
        &[
            ("origin", "https://openrouter.ai"),
            ("referer", "https://openrouter.ai/activity"),
            (
                "user-agent",
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36",
            ),
            ("cookie", cookie_header),
        ],
        20,
    )
}

fn parse_usage(resp: &Value) -> Value {
    let data = resp
        .get("data")
        .and_then(|d| d.get("data"))
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    let labels = resp
        .get("data")
        .and_then(|d| d.get("labels"))
        .and_then(|l| l.get("model"))
        .cloned()
        .unwrap_or(Value::Null);
    let mut total = 0.0f64;
    let models: Vec<Value> = data
        .iter()
        .filter_map(|r| {
            let raw = r.get("model").and_then(|x| x.as_str()).unwrap_or("?");
            let cost = r.get("total_usage").and_then(|x| x.as_f64()).unwrap_or(0.0);
            let tokens = r
                .get("tokens_total")
                .and_then(|x| x.as_str())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            let label = labels.get(raw).and_then(|x| x.as_str()).unwrap_or(raw);
            total += cost;
            Some(json!({
                "model": raw,
                "label": label,
                "cost": (cost * 10000.0).round() / 10000.0,
                "tokens": tokens,
            }))
        })
        .collect();
    json!({ "total": (total * 10000.0).round() / 10000.0, "models": models })
}

/// Full sync: Chrome cookies → analytics-query (today + month) → cache file.
pub fn sync_now(_cfg: &Config) -> Result<String, String> {
    let cookies = chrome_openrouter_cookies()?;
    if cookies.is_empty() {
        return Err("no openrouter.ai cookies in Chrome — log in at openrouter.ai first".into());
    }
    let header = cookies
        .iter()
        .map(|c| format!("{}={}", c.name, c.value))
        .collect::<Vec<_>>()
        .join("; ");

    // time window: local midnight → now, and the same 30 days back
    let local_now = chrono::Local::now();
    let midnight = local_now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .ok_or("bad local time")?;
    let start_today = chrono::Local
        .from_local_datetime(&midnight)
        .single()
        .ok_or("timezone conversion failed")?
        .with_timezone(&chrono::Utc);
    let start_month = start_today - chrono::Duration::days(30);
    let now_utc = chrono::Utc::now();

    let today = query(&header, &iso(start_today), &iso(now_utc))?;
    let month = query(&header, &iso(start_month), &iso(now_utc))?;

    let payload = json!({
        "fetched_at": now_unix(),
        "today": parse_usage(&today),
        "month": parse_usage(&month),
    });

    let dir = crate::config::state_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("state dir: {e}"))?;
    let path = dir.join("openrouter-usage.json");
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&payload).map_err(|e| format!("json: {e}"))?,
    )
    .map_err(|e| format!("write cache: {e}"))?;

    let t = payload["today"]["total"].as_f64().unwrap_or(0.0);
    let m = payload["month"]["total"].as_f64().unwrap_or(0.0);
    Ok(format!(
        "✓ {}  today ${t:.2} · month ${m:.2}",
        path.display()
    ))
}
