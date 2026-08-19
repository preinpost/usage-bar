//! Internalized OpenRouter usage sync — replaces the former Python/shell
//! scripts. Pulls the user's Chrome login, decrypts the openrouter.ai session
//! cookies, queries the web dashboard's `analytics-query` endpoint and writes
//! the per-model usage cache.
//!
//! Platform support for the cookie pull:
//! * macOS   — Keychain "Chrome Safe Storage" password → PBKDF2(saltysalt,
//!   1003 iters) → AES-128-CBC ("v10" cookies).
//! * Windows — `Local State` `os_crypt.encrypted_key` → DPAPI
//!   (CryptUnprotectData) → AES-256-GCM ("v10" cookies).
//! * Linux/other — not supported (no Secret Service / kwallet integration);
//!   the sync reports a clear error instead of guessing.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(target_os = "macos")]
use aes::Aes128;
#[cfg(target_os = "macos")]
use cbc::Decryptor;
#[cfg(target_os = "macos")]
use cipher::block_padding::Pkcs7;
#[cfg(target_os = "macos")]
use cipher::{BlockDecryptMut, KeyIvInit};
#[cfg(target_os = "macos")]
use hmac::Hmac;
#[cfg(target_os = "macos")]
use pbkdf2::pbkdf2;
#[cfg(target_os = "macos")]
use sha1::Sha1;

#[cfg(target_os = "windows")]
use aes_gcm::aead::Aead;
#[cfg(target_os = "windows")]
use aes_gcm::{Aes256Gcm, KeyInit as _, Nonce};

use rusqlite::Connection;
use serde_json::{Value, json};

use crate::config::Config;
use crate::http;

use chrono::TimeZone as _;

#[cfg(target_os = "macos")]
type HmacSha1 = Hmac<Sha1>;
#[cfg(target_os = "macos")]
type Aes128CbcDec = Decryptor<Aes128>;

const ANALYTICS_URL: &str = "https://openrouter.ai/api/frontend/v1/private/analytics-query";
const COOKIE_HOST: &str = "%openrouter.ai%";

fn home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/"))
}

// ---------------------------------------------------------------- locations

#[cfg(target_os = "macos")]
fn chrome_cookie_db() -> PathBuf {
    home()
        .join("Library")
        .join("Application Support")
        .join("Google")
        .join("Chrome")
        .join("Default")
        .join("Cookies")
}

#[cfg(target_os = "windows")]
fn local_app_data() -> PathBuf {
    std::env::var("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join("AppData").join("Local"))
}

#[cfg(target_os = "windows")]
fn chrome_cookie_db() -> PathBuf {
    // Chrome moved the cookie store into `Network\` (Chrome 104+); older
    // installs keep it in `Default\`. Prefer the current location.
    let ud = local_app_data().join("Google").join("Chrome").join("User Data");
    let net = ud.join("Default").join("Network").join("Cookies");
    if net.exists() {
        net
    } else {
        ud.join("Default").join("Cookies")
    }
}

#[cfg(target_os = "windows")]
fn chrome_local_state() -> PathBuf {
    local_app_data()
        .join("Google")
        .join("Chrome")
        .join("User Data")
        .join("Local State")
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn chrome_cookie_db() -> PathBuf {
    PathBuf::new()
}

// ------------------------------------------------------------- key material

/// 16-byte AES key (macOS): PBKDF2-HMAC-SHA1 of the Keychain "Chrome Safe
/// Storage" password (salt `saltysalt`, 1003 rounds).
#[cfg(target_os = "macos")]
fn chrome_key() -> Result<Vec<u8>, String> {
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
    Ok(key.to_vec())
}

/// 32-byte AES key (Windows): the `os_crypt.encrypted_key` from `Local State`
/// is a base64 blob prefixed with `DPAPI`; unwrap it with CryptUnprotectData.
#[cfg(target_os = "windows")]
fn chrome_key() -> Result<Vec<u8>, String> {
    use base64::Engine as _;

    let text = std::fs::read_to_string(chrome_local_state())
        .map_err(|e| format!("read chrome Local State: {e}"))?;
    let v: Value =
        serde_json::from_str(&text).map_err(|e| format!("parse chrome Local State: {e}"))?;
    let b64 = v
        .get("os_crypt")
        .and_then(|o| o.get("encrypted_key"))
        .and_then(|x| x.as_str())
        .ok_or("os_crypt.encrypted_key not found in chrome Local State")?;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|e| format!("decode os_crypt.encrypted_key: {e}"))?;
    let blob = raw
        .strip_prefix(b"DPAPI")
        .ok_or("os_crypt.encrypted_key: missing DPAPI prefix")?;
    let key = dpapi_unprotect(blob)?;
    if key.len() != 32 {
        return Err(format!("DPAPI unwrap returned {} bytes, expected 32", key.len()));
    }
    Ok(key)
}

#[cfg(target_os = "windows")]
fn dpapi_unprotect(blob: &[u8]) -> Result<Vec<u8>, String> {
    use windows_sys::Win32::Foundation::{GetLastError, LocalFree};
    use windows_sys::Win32::Security::Cryptography::{
        CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    let mut in_blob = CRYPT_INTEGER_BLOB {
        cbData: blob.len() as u32,
        pbData: blob.as_ptr() as *mut u8,
    };
    let mut out_blob = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // SAFETY: valid input blob (borrowed for the call), no entropy/reserved
    // args, output blob owned by the OS until we LocalFree it.
    let ok = unsafe {
        CryptUnprotectData(
            &mut in_blob as *const _,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out_blob,
        )
    };
    if ok == 0 {
        let err = unsafe { GetLastError() };
        return Err(format!("CryptUnprotectData failed (win32 error 0x{err:08X})"));
    }
    let data = if out_blob.cbData > 0 && !out_blob.pbData.is_null() {
        // SAFETY: out_blob is owned by the OS for the lifetime of this scope;
        // we copy before LocalFree.
        unsafe { std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize) }.to_vec()
    } else {
        Vec::new()
    };
    if !out_blob.pbData.is_null() {
        // SAFETY: pointer came from CryptUnprotectData (LocalAlloc).
        unsafe { LocalFree(out_blob.pbData as _) };
    }
    Ok(data)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn chrome_key() -> Result<Vec<u8>, String> {
    Err("OpenRouter dashboard sync is only supported on macOS and Windows".into())
}

// ----------------------------------------------------------- cookie decrypt

/// `v10`-scheme cookie decrypt. `key` is 16 bytes (macOS, AES-128-CBC) or
/// 32 bytes (Windows, AES-256-GCM); `strip_prefix` drops the 32-byte
/// SHA-256-of-domain integrity prefix that Chromium prepends since cookie
/// database version 24.
fn decrypt_cookie(enc: &[u8], key: &[u8], strip_prefix: bool) -> Result<String, String> {
    if enc.len() < 3 || &enc[..3] != b"v10" {
        return match &enc[..enc.len().min(3)] {
            b"v20" | b"v21" => Err(
                "cookie uses Chrome App-Bound Encryption (v20/v21) — not supported".into(),
            ),
            _ => Err("unsupported cookie scheme".into()),
        };
    }
    let plain = decrypt_body(&enc[3..], key)?;
    let value = if strip_prefix && plain.len() > 32 {
        &plain[32..]
    } else {
        &plain[..]
    };
    let s = String::from_utf8_lossy(value).to_string();
    if s.is_empty() {
        Err("empty cookie value".into())
    } else {
        Ok(s)
    }
}

#[cfg(target_os = "macos")]
fn decrypt_body(body: &[u8], key: &[u8]) -> Result<Vec<u8>, String> {
    let iv = [0x20u8; 16]; // b' ' * 16
    let mut buf = body.to_vec();
    let pt = Aes128CbcDec::new(key[..16].into(), &iv.into())
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|_| "aes-128-cbc decrypt failed".to_string())?;
    Ok(pt.to_vec())
}

#[cfg(target_os = "windows")]
fn decrypt_body(body: &[u8], key: &[u8]) -> Result<Vec<u8>, String> {
    // v10 layout after the prefix: nonce(12) + ciphertext + tag(16)
    if body.len() < 12 + 16 {
        return Err("cookie payload too short for aes-256-gcm".into());
    }
    let cipher =
        Aes256Gcm::new_from_slice(key).map_err(|_| "bad aes-256-gcm key".to_string())?;
    let nonce = Nonce::from_slice(&body[..12]);
    cipher
        .decrypt(nonce, &body[12..])
        .map_err(|_| "aes-256-gcm decrypt failed".to_string())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn decrypt_body(_body: &[u8], _key: &[u8]) -> Result<Vec<u8>, String> {
    Err("unsupported platform".into())
}

// ----------------------------------------------------------- database open

/// Owns the read-only cookie connection plus (on Windows) the temp copy of
/// the Chrome DB, which is cleaned up when dropped.
struct CookieDb {
    conn: Connection,
    tmp: Option<PathBuf>,
}

impl CookieDb {
    fn conn(&self) -> &Connection {
        &self.conn
    }
}

impl Drop for CookieDb {
    fn drop(&mut self) {
        if let Some(p) = &self.tmp {
            let _ = std::fs::remove_dir_all(p);
        }
    }
}

#[cfg(target_os = "macos")]
fn open_cookie_db(db: &Path) -> Result<CookieDb, String> {
    let uri = format!("file:{}?mode=ro", db.display());
    let conn = Connection::open_with_flags(
        &uri,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| format!("open cookies db: {e}"))?;
    Ok(CookieDb { conn, tmp: None })
}

#[cfg(target_os = "windows")]
fn open_cookie_db(db: &Path) -> Result<CookieDb, String> {
    // Chrome keeps the cookie store open (WAL, locked) — copy the file to a
    // private temp location and open that instead. The copy is left behind by
    // Chrome's own checkpointing: cookies we need change rarely, so a
    // slightly stale snapshot is fine.
    let tmp = std::env::temp_dir().join(format!("usage-bar-cookies-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&tmp);
    let copy = tmp.join("Cookies");
    std::fs::copy(db, &copy).map_err(|e| format!("copy cookies db: {e}"))?;
    let conn = Connection::open(&copy).map_err(|e| format!("open cookies copy: {e}"))?;
    Ok(CookieDb {
        conn,
        tmp: Some(tmp),
    })
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn open_cookie_db(_db: &Path) -> Result<CookieDb, String> {
    Err("unsupported platform".into())
}

/// Chromium cookie-DB schema version 24+ prepends a 32-byte SHA-256 of the
/// domain to the plaintext; read the stored version instead of guessing.
fn cookie_db_has_prefix(conn: &Connection) -> bool {
    conn.prepare("SELECT value FROM meta WHERE key = 'version'")
        .ok()
        .and_then(|mut stmt| stmt.query_row([], |r| r.get::<_, String>(0)).ok())
        .and_then(|s| s.parse::<i64>().ok())
        .map(|v| v >= 24)
        .unwrap_or(true) // modern Chrome by default
}

struct OrCookie {
    name: String,
    value: String,
}

/// Cookie header for authenticated OpenRouter web-dashboard API calls.
pub fn cookie_header() -> Result<String, String> {
    let cookies = chrome_openrouter_cookies()?;
    if cookies.is_empty() {
        return Err("no openrouter.ai cookies in Chrome — log in at openrouter.ai first".into());
    }
    Ok(cookies
        .iter()
        .map(|c| format!("{}={}", c.name, c.value))
        .collect::<Vec<_>>()
        .join("; "))
}

/// Read + decrypt every openrouter.ai cookie from the user's Chrome profile.
fn chrome_openrouter_cookies() -> Result<Vec<OrCookie>, String> {
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        return Err("OpenRouter dashboard sync is only supported on macOS and Windows".into());
    }
    let db = chrome_cookie_db();
    if !db.exists() {
        return Err(format!("chrome cookie db not found: {}", db.display()));
    }
    let db = open_cookie_db(&db)?;
    let strip_prefix = cookie_db_has_prefix(db.conn());
    let key = chrome_key()?;

    let mut stmt = db
        .conn()
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
        if let Ok(value) = decrypt_cookie(&enc, &key, strip_prefix) {
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
        .map(|r| {
            let raw = r.get("model").and_then(|x| x.as_str()).unwrap_or("?");
            let cost = r.get("total_usage").and_then(|x| x.as_f64()).unwrap_or(0.0);
            let tokens = r
                .get("tokens_total")
                .and_then(|x| x.as_str())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            let label = labels.get(raw).and_then(|x| x.as_str()).unwrap_or(raw);
            total += cost;
            json!({
                "model": raw,
                "label": label,
                "cost": (cost * 10000.0).round() / 10000.0,
                "tokens": tokens,
            })
        })
        .collect();
    json!({ "total": (total * 10000.0).round() / 10000.0, "models": models })
}

/// Full sync: Chrome cookies → analytics-query (today + month) → cache file.
pub fn sync_now(_cfg: &Config) -> Result<String, String> {
    let header = cookie_header()?;

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
