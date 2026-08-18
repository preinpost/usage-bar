//! Provider credential storage: OS keyring first, 0600-file fallback.
//!
//! Credentials (Copilot OAuth token, OpenRouter API key, OpenCode Go API key)
//! are JSON blobs — `{ "access_token": … }` / `{ "api_key": … }` — stored for
//! their provider "account" under a shared [`SERVICE`] namespace:
//!
//! * macOS        → Keychain (Keychain Services)
//! * Windows      → Credential Manager
//! * Linux/*nix   → Secret Service (gnome-keyring / KWallet over DBus)
//!
//! When no secure store is reachable — headless Linux, CI, a locked keychain —
//! we fall back to the legacy 0600 JSON files under `<config>/secrets/`
//! (`copilot.json`, `openrouter.json`, `opencode-go.json`). This mirrors how
//! GitHub's `gh` CLI stores auth: keyring first, file as a safety net, and it
//! keeps old installs working without a migration step. Legacy files are
//! promoted into the keyring lazily on read so existing tokens move over as
//! soon as a secure store is available.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::config::Config;

/// Keyring service/namespace used for every provider credential.
pub const SERVICE: &str = "usage-bar";

/// Account (keyring item name) used to probe store reachability. Never holds
/// an actual secret — at most returns `NoEntry` when the store is reachable.
const PROBE_ACCOUNT: &str = "__usage_bar_probe__";

/// Forced-file mode: set by unit tests so they never touch the real
/// keychain/secret-service (hermetic, works on CI runners too).
static KEYRING_DISABLED: AtomicBool = AtomicBool::new(false);
/// Cached result of the one-time reachability probe (per process).
static KEYRING_READY: OnceLock<bool> = OnceLock::new();

/// Is the OS keyring usable? Probed once per process and cached; the poll
/// loop in the TUI only reads the cached result after the first refresh.
fn keyring_ready() -> bool {
    if KEYRING_DISABLED.load(Ordering::Relaxed) {
        return false;
    }
    *KEYRING_READY.get_or_init(probe_keyring)
}

fn probe_keyring() -> bool {
    // `store_status` runs the platform store constructor once (Keychain /
    // Credential Manager / Secret Service). A failing store init means there
    // is no secure storage to talk to.
    if let Err(e) = keyring::v1::Entry::store_status() {
        eprintln!("usage-bar: OS keyring unavailable ({e}); using 0600 file secrets");
        return false;
    }
    // Confirm the store is actually reachable with a real round-trip. A fresh
    // entry yields `NoEntry` (reachable, nothing stored yet); anything else
    // (missing DBus session, locked keychain, …) counts as unavailable.
    let probe = match keyring::v1::Entry::new(SERVICE, PROBE_ACCOUNT) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("usage-bar: OS keyring unavailable ({e}); using 0600 file secrets");
            return false;
        }
    };
    match probe.get_password() {
        Ok(_) => true,
        Err(keyring::Error::NoEntry) => true,
        Err(e) => {
            eprintln!("usage-bar: OS keyring probe failed ({e}); using 0600 file secrets");
            false
        }
    }
}

/// Fallback file path for an account, e.g. `secrets/copilot.json`.
fn file_path(cfg: &Config, account: &str) -> PathBuf {
    cfg.secrets_dir.join(format!("{account}.json"))
}

fn keyring_get(account: &str) -> Option<String> {
    if !keyring_ready() {
        return None;
    }
    let entry = keyring::v1::Entry::new(SERVICE, account).ok()?;
    // `Err` here is either `NoEntry` (simply not stored) or a transient
    // keychain hiccup — both mean “fall through to the 0600 file”.
    entry.get_password().ok()
}

fn keyring_set(account: &str, json: &str) -> bool {
    if !keyring_ready() {
        return false;
    }
    keyring::v1::Entry::new(SERVICE, account)
        .and_then(|e| e.set_password(json))
        .is_ok()
}

/// Read a secret JSON blob for `account`: OS keyring first, then the 0600
/// fallback file. If the file held the only copy and the keyring is usable,
/// the file is promoted into the keyring (lazy migration of legacy installs).
pub fn read_json(cfg: &Config, account: &str) -> Option<String> {
    if keyring_ready()
        && let Some(v) = keyring_get(account)
    {
        return Some(v);
    }
    let p = file_path(cfg, account);
    let text = std::fs::read_to_string(&p).ok()?;
    // best-effort one-way promotion; the file stays as the fallback copy
    if keyring_ready() {
        let _ = keyring_set(account, &text);
    }
    Some(text)
}

/// Write a secret JSON blob for `account`. Prefers the OS keyring; on any
/// failure the blob lands in a 0600 file so the app still works headless.
pub fn write_json(cfg: &Config, account: &str, json: &str) {
    if keyring_set(account, json) {
        return;
    }
    let _ = std::fs::create_dir_all(&cfg.secrets_dir);
    let p = file_path(cfg, account);
    let _ = std::fs::write(&p, json);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
    }
}

/// Remove a credential from both the keyring and the fallback file.
pub fn delete(cfg: &Config, account: &str) {
    if keyring_ready()
        && let Ok(entry) = keyring::v1::Entry::new(SERVICE, account)
    {
        let _ = entry.delete_credential();
    }
    let _ = std::fs::remove_file(file_path(cfg, account));
}

/// Read the JSON blob from an explicit path — used by Copilot's legacy
/// multi-config-dir scan (plugin state dir + standalone config dir).
pub fn read_file(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok()
}

/// Force file-only storage for this process. Test helper: keeps unit tests
/// hermetic (no access to the real keychain / secret service / CI hosts).
#[cfg(test)]
pub fn force_file_mode_for_tests() {
    KEYRING_DISABLED.store(true, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering as AO};

    fn tmp_dir() -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, AO::SeqCst);
        let d = std::env::temp_dir().join(format!(
            "usage-bar-secrets-test-{}-{}",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    // All unit tests here pin to file-only storage so they never touch a real
    // keychain/secret-service (also keeps CI runners hermetic).
    fn start() -> Config {
        force_file_mode_for_tests();
        let dir = tmp_dir();
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
    fn write_read_roundtrip_in_file_mode() {
        let cfg = start();
        write_json(&cfg, "mytoken", r#"{"api_key":"sk-test"}"#);
        let p = cfg.secrets_dir.join("mytoken.json");
        assert!(p.exists(), "fallback file should be written in file mode");
        assert_eq!(
            read_json(&cfg, "mytoken").as_deref(),
            Some(r#"{"api_key":"sk-test"}"#)
        );
    }

    #[cfg(unix)]
    #[test]
    fn fallback_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let cfg = start();
        write_json(&cfg, "mytoken", r#"{"api_key":"sk-secret"}"#);
        let mode = std::fs::metadata(cfg.secrets_dir.join("mytoken.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn delete_removes_both_keyring_and_file() {
        let cfg = start();
        write_json(&cfg, "mytoken", r#"{"api_key":"sk-test"}"#);
        assert!(cfg.secrets_dir.join("mytoken.json").exists());
        delete(&cfg, "mytoken");
        assert!(!cfg.secrets_dir.join("mytoken.json").exists());
        assert_eq!(read_json(&cfg, "mytoken"), None);
    }

    #[test]
    fn missing_secret_returns_none() {
        let cfg = start();
        assert_eq!(read_json(&cfg, "nope"), None);
    }

    /// Real-OS secure-storage round trip, run opt-in so the normal test suite
    /// never prompts on or writes to a user's keychain: `cargo test -- --ignored`.
    /// Exercises probe → set → get → delete against the actual platform store,
    /// plus the legacy-file → keyring migration in `read_json`.
    #[test]
    #[ignore = "needs an OS keyring/secret-service; run explicitly on a dev machine"]
    fn real_keyring_roundtrip() {
        // KEYRING_DISABLED is only ever turned on by normal tests; an --ignored
        // run skips those, so the real store stays reachable here.
        assert!(
            keyring_ready(),
            "OS keyring should be available on this host"
        );
        const ACCOUNT: &str = "__usage_bar_manual_roundtrip__";
        let json = r#"{"api_key":"sk-probe-check"}"#;
        assert!(keyring_set(ACCOUNT, json));
        assert_eq!(keyring_get(ACCOUNT).as_deref(), Some(json));
        // delete_credential, then a fresh read must see nothing
        keyring::v1::Entry::new(SERVICE, ACCOUNT)
            .unwrap()
            .delete_credential()
            .unwrap();
        assert_eq!(keyring_get(ACCOUNT), None);

        // migration: a legacy 0600 file is promoted into the keyring on read
        let dir = tmp_dir();
        let cfg = Config {
            claude_budget: 0,
            codex_budget: 0,
            opencode_budget: 0,
            reset_hour: 0,
            refresh_seconds: 30,
            show_no_data_providers: false,
            prices: crate::config::Prices::default(),
            secrets_dir: dir.join("secrets"),
        };
        let p = cfg.secrets_dir.join(format!("{ACCOUNT}.json"));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let legacy = r#"{"api_key":"sk-legacy-file"}"#;
        std::fs::write(&p, legacy).unwrap();
        // keyring currently empty for this account → file is promoted
        assert_eq!(keyring_get(ACCOUNT), None);
        assert_eq!(read_json(&cfg, ACCOUNT).as_deref(), Some(legacy));
        assert_eq!(
            keyring_get(ACCOUNT).as_deref(),
            Some(legacy),
            "legacy file should have been migrated into the keyring"
        );
        // cleanup both copies
        keyring::v1::Entry::new(SERVICE, ACCOUNT)
            .unwrap()
            .delete_credential()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
