//! Local-only collectors: Claude Code, Codex, OpenCode (Go).

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, Local, TimeZone, Timelike};
use rusqlite::Connection;
use serde_json::Value;
use walkdir::WalkDir;

use crate::config::{self, Prices};
use crate::model::{Item, LocalStats, Tokens};

/// Parse an ISO-8601 string or epoch-seconds number into local time.
pub fn parse_ts(v: &Value) -> Option<DateTime<Local>> {
    match v {
        Value::Number(n) => {
            let secs = n.as_i64().or_else(|| n.as_f64().map(|f| f as i64))?;
            Local.timestamp_opt(secs, 0).single()
        }
        Value::String(s) => {
            let norm = s.trim().trim_end_matches('Z');
            DateTime::parse_from_rfc3339(&format!("{norm}Z"))
                .ok()
                .map(|d| d.with_timezone(&Local))
                .or_else(|| Local.datetime_from_str(s, "%Y-%m-%dT%H:%M:%S%.f").ok())
        }
        _ => None,
    }
}

pub fn usage_cost(u: &Value, p: &Prices) -> f64 {
    let g = |k: &str| u.get(k).and_then(|v| v.as_i64()).unwrap_or(0) as f64;
    g("input_tokens") / 1e6 * p.input
        + g("output_tokens") / 1e6 * p.output
        + g("cache_read_input_tokens") / 1e6 * p.cache_read
        + g("cache_creation_input_tokens") / 1e6 * p.cache_write
}

fn mtime_local(p: &Path) -> Option<DateTime<Local>> {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .map(DateTime::<Local>::from)
}

/// Today's window start (midnight at `reset_hour`).
pub fn window_start(reset_hour: u32) -> DateTime<Local> {
    let now = Local::now();
    let mut start = now
        .with_hour(reset_hour)
        .and_then(|d| d.with_minute(0))
        .and_then(|d| d.with_second(0))
        .and_then(|d| d.with_nanosecond(0))
        .unwrap_or_else(|| {
            Local
                .with_ymd_and_hms(now.year(), now.month(), now.day(), 0, 0, 0)
                .single()
                .unwrap_or(now)
        });
    if start > now {
        start = start - chrono::Duration::days(1);
    }
    start
}

pub fn window_end(start: DateTime<Local>) -> DateTime<Local> {
    start + chrono::Duration::days(1)
}

fn is_today_ish(p: &Path, start: DateTime<Local>) -> bool {
    match mtime_local(p) {
        Some(t) => t >= start - chrono::Duration::days(2),
        None => true,
    }
}

// ------------------------------------------------------------ Claude Code

fn add_usage(t: &mut Tokens, u: &Value) {
    let g = |k: &str| u.get(k).and_then(|v| v.as_i64()).unwrap_or(0) as u64;
    t.input += g("input_tokens");
    t.output += g("output_tokens");
    t.cache_read += g("cache_read_input_tokens");
    t.cache_write += g("cache_creation_input_tokens");
}

pub fn collect_claude(start: DateTime<Local>, prices: &Prices) -> LocalStats {
    let mut out = LocalStats::default();
    let base = config::claude_projects_dir();
    if !base.is_dir() {
        return out;
    }
    // newest activity across everything (any project, any window)
    let mut last: Option<i64> = None;
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&base)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    dirs.sort();
    for pdir in dirs {
        let entries = match std::fs::read_dir(&pdir) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .collect::<Vec<_>>(),
            Err(_) => continue,
        };
        let mut item = Item {
            name: pdir
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default(),
            ..Default::default()
        };
        for jf in entries {
            if jf.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            if let Ok(md) = std::fs::metadata(&jf) {
                if let Ok(mt) = md.modified() {
                    let secs = mt
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    last = Some(last.map_or(secs, |l| l.max(secs)));
                }
            }
            if !is_today_ish(&jf, start) {
                continue;
            }
            let Ok(f) = File::open(&jf) else { continue };
            let reader = BufReader::new(f);
            for line in reader.lines().map_while(Result::ok) {
                let Ok(v) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let usage = v
                    .get("usage")
                    .or_else(|| v.get("message").and_then(|m| m.get("usage")))
                    .or_else(|| v.get("message").and_then(|m| m.get("modelUsage")))
                    .and_then(|u| if u.is_object() { Some(u) } else { None });
                let Some(usage) = usage else { continue };
                let ts = v
                    .get("timestamp")
                    .or_else(|| v.get("ts"))
                    .and_then(parse_ts);
                if let Some(t) = ts {
                    if t < start {
                        continue;
                    }
                }
                add_usage(&mut item.tokens, usage);
                item.cost += usage_cost(usage, prices);
                item.msgs += 1;
            }
        }
        if item.msgs > 0 || item.tokens.total() > 0 {
            out.msgs += item.msgs;
            out.cost += item.cost;
            out.tokens.input += item.tokens.input;
            out.tokens.output += item.tokens.output;
            out.tokens.cache_read += item.tokens.cache_read;
            out.tokens.cache_write += item.tokens.cache_write;
            out.items.push(item);
        }
    }
    out.items
        .sort_by(|a, b| b.tokens.total().cmp(&a.tokens.total()));
    out.has_token_data = out.tokens.total() > 0;
    out.last_activity_secs = last;
    out
}

// ---------------------------------------------------------------- Codex

pub fn collect_codex(start: DateTime<Local>) -> LocalStats {
    let mut out = LocalStats::default();
    let mut last: Option<i64> = None;
    let history = config::codex_history_path();
    if history.exists() {
        if let Ok(md) = std::fs::metadata(&history) {
            if let Ok(mt) = md.modified() {
                if let Ok(d) = mt.duration_since(std::time::UNIX_EPOCH) {
                    last = Some(d.as_secs() as i64);
                }
            }
        }
        if let Ok(f) = File::open(&history) {
            let reader = BufReader::new(f);
            for line in reader.lines().map_while(Result::ok) {
                let Ok(v) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let Some(ts) = v.get("ts").and_then(parse_ts) else {
                    continue;
                };
                if ts >= start {
                    out.turns += 1;
                }
            }
        }
    }
    let base = config::codex_sessions_dir();
    if base.is_dir() {
        let mut session_ids: Vec<String> = Vec::new();
        for entry in WalkDir::new(&base).into_iter().filter_map(Result::ok) {
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            if let Ok(md) = std::fs::metadata(p) {
                if let Ok(mt) = md.modified() {
                    if let Ok(d) = mt.duration_since(std::time::UNIX_EPOCH) {
                        let secs = d.as_secs() as i64;
                        last = Some(last.map_or(secs, |l| l.max(secs)));
                    }
                }
            }
            if !is_today_ish(p, start) {
                continue;
            }
            let Ok(f) = File::open(p) else { continue };
            let reader = BufReader::new(f);
            for line in reader.lines().map_while(Result::ok) {
                let Ok(v) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let typ = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
                let payload = v.get("payload");
                if typ == "session_meta" {
                    let ts = payload.and_then(|p| p.get("timestamp")).and_then(parse_ts);
                    if let Some(t) = ts {
                        if t >= start {
                            if let Some(id) =
                                payload.and_then(|p| p.get("id")).and_then(|x| x.as_str())
                            {
                                session_ids.push(id.to_string());
                            }
                        }
                    }
                } else if typ == "response_item" {
                    let ts = payload
                        .and_then(|p| p.get("timestamp"))
                        .or_else(|| v.get("timestamp"))
                        .and_then(parse_ts);
                    if let Some(t) = ts {
                        if t < start {
                            continue;
                        }
                    }
                    // some builds record tokens/cost inside payload
                    if let Some(u) = payload
                        .and_then(|p| p.get("usage"))
                        .or_else(|| payload.and_then(|p| p.get("tokens")))
                        .or_else(|| payload.and_then(|p| p.get("cost")))
                    {
                        out.tokens.input += u
                            .get("prompt_tokens")
                            .and_then(|x| x.as_i64())
                            .unwrap_or(0)
                            .max(0) as u64;
                        out.tokens.output += u
                            .get("completion_tokens")
                            .and_then(|x| x.as_i64())
                            .unwrap_or(0)
                            .max(0) as u64;
                        out.cost += u.get("cost_usd").and_then(|x| x.as_f64()).unwrap_or(0.0);
                    }
                    out.turns += 1;
                }
            }
        }
        session_ids.sort();
        session_ids.dedup();
        out.sessions = session_ids.len() as u64;
    }
    out.has_token_data = out.tokens.total() > 0;
    out.last_activity_secs = last;
    out
}

// -------------------------------------------------------------- OpenCode

pub fn collect_opencode(start: DateTime<Local>) -> Option<LocalStats> {
    let dbp = config::opencode_db_path();
    if !dbp.exists() {
        return None;
    }
    let uri = format!("file:{}?mode=ro", dbp.display());
    let conn = Connection::open_with_flags(
        &uri,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .ok()?;
    let start_ms = start.timestamp_millis();
    let mut stmt = conn
        .prepare(
            "SELECT COALESCE(slug,''), COALESCE(title,''), cost,
                    tokens_input, tokens_output, tokens_reasoning,
                    tokens_cache_read, tokens_cache_write
             FROM session WHERE time_created >= ?1 ORDER BY time_created DESC",
        )
        .ok()?;
    let rows = stmt
        .query_map([start_ms], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, f64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, i64>(7)?,
            ))
        })
        .ok()?;
    let mut out = LocalStats::default();
    for row in rows.flatten() {
        let (slug, title, cost, ti, to, tr, tcr, tcw) = row;
        let name = if !slug.is_empty() { slug } else { title };
        out.cost += cost;
        out.sessions += 1;
        out.tokens.input += ti.max(0) as u64;
        out.tokens.output += to.max(0) as u64;
        out.tokens.reasoning += tr.max(0) as u64;
        out.tokens.cache_read += tcr.max(0) as u64;
        out.tokens.cache_write += tcw.max(0) as u64;
        let item = Item {
            name: truncate(&name, 40),
            tokens: Tokens {
                input: ti.max(0) as u64,
                output: to.max(0) as u64,
                reasoning: tr.max(0) as u64,
                cache_read: tcr.max(0) as u64,
                cache_write: tcw.max(0) as u64,
            },
            cost,
            msgs: 0,
        };
        out.items.push(item);
    }
    if let Ok(mut s2) = conn.prepare("SELECT MAX(time_created) FROM session") {
        if let Ok(mut rows) = s2.query([]) {
            if let Ok(Some(r)) = rows.next() {
                if let Ok(Some(t)) = r.get::<_, Option<i64>>(0) {
                    out.last_activity_secs = Some(t / 1000);
                }
            }
        }
    }
    out.items.sort_by(|a, b| {
        b.cost
            .partial_cmp(&a.cost)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out.has_token_data = out.tokens.total() > 0;
    Some(out)
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
}

/// Read a whole small text file.
pub fn read_to_string_lossy(p: &Path) -> Option<String> {
    let mut s = String::new();
    File::open(p).ok()?.read_to_string(&mut s).ok()?;
    Some(s)
}
