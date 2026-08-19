//! OpenRouter recent-request logs via the web dashboard's private
//! `user-transactions` endpoint. Auth is the same Chrome-session cookie pull
//! used by `dashboard.rs`, so this is available on macOS/Windows (and on other
//! platforms whenever Chrome cookies can be reached).
//!
//! The endpoint returns the same per-request rows the `openrouter.ai/logs`
//! page shows: one entry per generation with model / provider / tokens / cost /
//! finish reason / latency. Prompt & response bodies require an additional
//! permission (`can_view_private_prompt_logs`) and live behind a second
//! call — this module only fetches the list, which is enough for a usage view.

use serde_json::Value;

use crate::model::{OrLogEntry, OrLogs};

const TX_URL: &str = "https://openrouter.ai/api/frontend/v1/private/user-transactions";

fn as_u64(v: Option<&Value>) -> u64 {
    v.and_then(|x| x.as_u64())
        .or_else(|| v.and_then(|x| x.as_str()).and_then(|s| s.parse::<u64>().ok()))
        .or_else(|| v.and_then(|x| x.as_f64()).map(|f| f.max(0.0) as u64))
        .unwrap_or(0)
}

fn as_f64(v: Option<&Value>) -> f64 {
    v.and_then(|x| x.as_f64()).unwrap_or(0.0)
}

fn as_str(v: Option<&Value>) -> String {
    v.and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string()
}

fn parse_ts(s: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.timestamp())
        .unwrap_or(0)
}

/// Fetch the most recent `limit` requests. `from_secs` optionally limits the
/// window to entries newer than that unix timestamp (the API's `from` field).
pub fn fetch(limit: u32, from_secs: Option<i64>) -> Result<OrLogs, String> {
    let header = crate::openrouter::dashboard::cookie_header()?;
    let mut url = format!("{TX_URL}?page=1&limit={limit}");
    if let Some(s) = from_secs {
        let iso = chrono::DateTime::from_timestamp(s, 0)
            .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S.000Z").to_string())
            .unwrap_or_default();
        if !iso.is_empty() {
            url.push_str("&from=");
            url.push_str(&crate::http::urlencode(&iso));
        }
    }
    let headers = [
        ("origin", "https://openrouter.ai"),
        ("referer", "https://openrouter.ai/logs"),
        (
            "user-agent",
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36",
        ),
        ("cookie", header.as_str()),
    ];
    let v = crate::http::get_json(&url, &headers, 20)?;
    let data = v
        .get("data")
        .and_then(|d| d.as_array())
        .cloned()
        .unwrap_or_default();
    let can_view_private = v
        .get("can_view_private_prompt_logs")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);

    let mut entries = Vec::with_capacity(data.len());
    for t in &data {
        let app = t
            .get("app")
            .and_then(|a| a.get("title"))
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .unwrap_or_default();
        let api_key = t
            .get("api_key")
            .and_then(|a| a.get("name"))
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .unwrap_or_default();
        entries.push(OrLogEntry {
            generation_id: as_str(t.get("generation_id")),
            request_id: as_str(t.get("request_id")),
            model: as_str(t.get("model")),
            provider: as_str(t.get("provider_name")),
            app,
            api_key,
            api_type: as_str(t.get("api_type")),
            created_at: parse_ts(&as_str(t.get("created_at"))),
            tokens_prompt: as_u64(t.get("tokens_prompt")),
            tokens_completion: as_u64(t.get("tokens_completion")),
            tokens_cached: as_u64(t.get("native_tokens_cached")),
            tokens_prompt_native: as_u64(t.get("native_tokens_prompt")),
            tokens_reasoning: as_u64(t.get("native_tokens_reasoning")),
            cost: as_f64(t.get("usage")),
            latency_ms: as_u64(t.get("latency")),
            generation_time_ms: as_u64(t.get("generation_time")),
            finish_reason: as_str(t.get("finish_reason")),
            streamed: t.get("streamed").and_then(|x| x.as_bool()).unwrap_or(false),
            cancelled: t.get("cancelled").and_then(|x| x.as_bool()).unwrap_or(false),
            response_cached: t
                .get("response_cache_source_id")
                .and_then(|x| x.as_str())
                .map(|s| !s.is_empty())
                .unwrap_or(false),
        });
    }

    Ok(OrLogs {
        can_view_private_prompt_logs: can_view_private,
        entries,
    })
}

/// Compact model slug: drop the vendor prefix, strip a trailing `-YYYYMMDD`
/// build-date, and cap the length. `deepseek/deepseek-v4-flash-20260731` →
/// `deepseek-v4-flash`.
pub fn short_model(model: &str, max: usize) -> String {
    let base = model.rsplit('/').next().unwrap_or(model);
    let base = strip_build_date(base);
    let count = base.chars().count();
    if count <= max {
        return base.to_string();
    }
    if max <= 1 {
        return base.chars().take(max).collect();
    }
    let mut out: String = base.chars().take(max - 2).collect();
    out.push_str("..");
    out
}

/// Provider name kept short: 6+ chars collapse to `abcd..` (6 wide).
/// `GMICloud` → `GMIC..`, `OpenAI` stays `OpenAI`.
pub fn short_provider(p: &str) -> String {
    if p.chars().count() > 6 {
        let head: String = p.chars().take(4).collect();
        format!("{head}..")
    } else {
        p.to_string()
    }
}

/// Generation throughput — completion tokens / generation duration, using the
/// same definition as the OpenRouter web dashboard: the duration is
/// `generation_time`, plus `latency` when the response was not streamed.
pub fn tps(e: &OrLogEntry) -> f64 {
    let ms = e.generation_time_ms + if e.streamed { 0 } else { e.latency_ms };
    if ms == 0 {
        return 0.0;
    }
    e.tokens_completion as f64 / (ms as f64 / 1000.0)
}

/// `tps` formatted compactly for a table cell (empty when 0/unknown).
pub fn fmt_tps(v: f64) -> String {
    if v <= 0.0 {
        return String::new();
    }
    if v >= 1000.0 {
        format!("{:.1}k/s", v / 1000.0)
    } else if v >= 100.0 {
        format!("{:.0}/s", v)
    } else {
        format!("{:.1}/s", v)
    }
}

fn strip_build_date(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 9
        && bytes[bytes.len() - 9] == b'-'
        && bytes[bytes.len() - 8..].iter().all(|b| b.is_ascii_digit())
    {
        &s[..s.len() - 9]
    } else {
        s
    }
}

/// Prompt-cache hit % (`cNN%`) or a full response-cache replay marker
/// (`c-hit`), empty when the request had no cache.
pub fn cache_label(e: &OrLogEntry) -> String {
    if e.response_cached {
        return "c-hit".into();
    }
    if e.tokens_cached > 0 && e.tokens_prompt_native > 0 {
        let pct = ((e.tokens_cached as f64 / e.tokens_prompt_native as f64) * 100.0)
            .clamp(0.0, 100.0)
            .round() as u8;
        format!("c{pct}%")
    } else {
        String::new()
    }
}

/// Fixed log-table column layout shared by the TUI modal and `ub logs` text:
/// `(width, right_align)` per column in the order time · model · provider ·
/// tokens · tps · cache · cost. Widths are the hard cap — anything longer is
/// trimmed to `..`, so rows always fit and the columns stay tight.
pub const LOG_COLUMNS: [(usize, bool); 7] = [
    (8, false),   // time      HH:MM:SS
    (18, false),  // model     deepseek-v4-flash fits
    (8, false),   // provider  GMIC..
    (10, true),   // tokens    327.2k tok (sums fit)
    (6, true),    // tps       96.4/s
    (5, true),    // cache     c100% / c-hit
    (8, true),    // cost      $0.0038
];
/// spaces between columns (kept minimal)
pub const LOG_COL_SEP: usize = 1;

/// Full rendered width of one log row: column slots + separators. Deterministic
/// because every cell is padded/trimmed to its slot, so the TUI can size the
/// modal exactly to the table instead of a fraction of the terminal.
pub fn log_table_width() -> usize {
    LOG_COLUMNS.iter().map(|(w, _)| w).sum::<usize>() + LOG_COL_SEP * (LOG_COLUMNS.len() - 1)
}

/// Pad a cell to its fixed slot; content longer than the slot is truncated
/// to `..` so the table keeps its width no matter what.
pub fn fit_cell(cell: &str, (w, right): (usize, bool)) -> String {
    let cell = truncate_wide(cell, w);
    if right {
        format!("{cell:>w$}")
    } else {
        format!("{cell:<w$}")
    }
}

/// Trim a string that overflows `w` columns down to `w` using a `..` tail.
fn truncate_wide(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        return s.to_string();
    }
    if w <= 1 {
        return s.chars().take(w).collect();
    }
    let head: String = s.chars().take(w - 2).collect();
    format!("{head}..")
}

/// Render a full row (any set of 7 cells — header / data / summary) into one
/// line with every column padded to its fixed width.
pub fn table_text(cells: &[String; 7]) -> String {
    let mut out = String::new();
    for (i, c) in cells.iter().enumerate() {
        if i > 0 {
            out.push_str(&" ".repeat(LOG_COL_SEP));
        }
        out.push_str(&fit_cell(c, LOG_COLUMNS[i]));
    }
    out
}

pub fn header_cells() -> [String; 7] {
    [
        "time".into(),
        "model".into(),
        "provider".into(),
        "tokens".into(),
        "tps".into(),
        "cache".into(),
        "cost".into(),
    ]
}

fn time_str(e: &OrLogEntry) -> String {
    chrono::DateTime::from_timestamp(e.created_at, 0)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "--:--:--".into())
}

/// Per-request row cells.
pub fn row_cells(e: &OrLogEntry) -> [String; 7] {
    let toks = e.tokens_prompt + e.tokens_completion + e.tokens_cached;
    [
        time_str(e),
        short_model(&e.model, 18),
        short_provider(&e.provider),
        format!("{} tok", crate::model::fmt_compact(toks)),
        fmt_tps(tps(e)),
        cache_label(e),
        if e.cost > 0.0 {
            format!("${:.4}", e.cost)
        } else {
            "$0".into()
        },
    ]
}

/// Prompt-cache hit % for one entry (`None` when there's no cache to speak of).
fn cache_pct(e: &OrLogEntry) -> Option<u8> {
    if e.response_cached || e.tokens_cached == 0 || e.tokens_prompt_native == 0 {
        return None;
    }
    Some(
        ((e.tokens_cached as f64 / e.tokens_prompt_native as f64) * 100.0)
            .clamp(0.0, 100.0)
            .round() as u8,
    )
}

/// Is the rendered value of this cell shortened to `..` (so a click can reveal
/// the full value)? Checks the padded/trimmed cell, not just the raw one.
pub fn cell_ellipsized(e: &OrLogEntry, i: usize) -> bool {
    if i >= 7 {
        return false;
    }
    fit_cell(&row_cells(e)[i], LOG_COLUMNS[i]).contains("..")
}

/// Full, untruncated value for a cell — what clicking a `..` cell reveals.
pub fn full_cell(e: &OrLogEntry, i: usize) -> String {
    let toks = e.tokens_prompt + e.tokens_completion + e.tokens_cached;
    match i {
        0 => time_str(e),
        1 => e.model.clone(), // full slug incl. vendor + build date
        2 => e.provider.clone(),
        3 => format!("{} tok total", crate::model::fmt_int(toks)),
        4 => format!("{:.2} tok/s", tps(e)),
        5 => match cache_pct(e) {
            Some(p) => format!("prompt cache {}%", p),
            None if e.response_cached => "response-cache replay (c-hit)".into(),
            None => "no prompt cache".into(),
        },
        6 => format!("${:.6}", e.cost),
        _ => String::new(),
    }
}

/// Aggregate row across all fetched entries: tokens sum, average tps, average
/// prompt-cache hit %, and total cost. Zero-duration / no-cache entries are
/// skipped from their averages so they don't drag them down.
pub fn summary_cells(entries: &[OrLogEntry]) -> [String; 7] {
    let toks: u64 = entries
        .iter()
        .map(|e| e.tokens_prompt + e.tokens_completion + e.tokens_cached)
        .sum();
    let tps_vals: Vec<f64> = entries.iter().map(tps).filter(|v| *v > 0.0).collect();
    let tps_avg = if tps_vals.is_empty() {
        0.0
    } else {
        tps_vals.iter().sum::<f64>() / tps_vals.len() as f64
    };
    let caches: Vec<u8> = entries.iter().filter_map(cache_pct).collect();
    let cache_avg = if caches.is_empty() {
        String::new()
    } else {
        let avg = caches.iter().map(|c| *c as u64).sum::<u64>() as f64 / caches.len() as f64;
        format!("c{:.0}%", avg)
    };
    let cost_sum: f64 = entries.iter().map(|e| e.cost).sum();
    [
        String::new(),
        // Σ + how many entries the aggregates cover
        format!("Σ({})", entries.len()),
        String::new(),
        format!("{} tok", crate::model::fmt_compact(toks)),
        fmt_tps(tps_avg),
        cache_avg,
        format!("${:.4}", cost_sum),
    ]
}

/// Multi-line text block (`ub logs`): header + rows, with an aggregate
/// (avg tps/cache, sum tokens/cost) footer line.
pub fn summarize(logs: &OrLogs) -> String {
    if logs.entries.is_empty() {
        return "no OpenRouter requests in the selected window".into();
    }
    let total_w: usize =
        LOG_COLUMNS.iter().map(|(w, _)| w).sum::<usize>() + LOG_COL_SEP * (LOG_COLUMNS.len() - 1);
    let rule = "─".repeat(total_w);
    let mut out = String::new();
    out.push_str(&table_text(&header_cells()));
    out.push('\n');
    out.push_str(&rule);
    for e in &logs.entries {
        out.push('\n');
        out.push_str(&table_text(&row_cells(e)));
    }
    out.push('\n');
    out.push_str(&rule);
    out.push('\n');
    out.push_str(&table_text(&summary_cells(&logs.entries)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> OrLogEntry {
        OrLogEntry {
            generation_id: "gen-123".into(),
            request_id: "req-123".into(),
            model: "vendor/model-name-abc-20260801".into(),
            provider: "SomeProvider".into(),
            app: "pi".into(),
            api_key: "key1".into(),
            api_type: "completions".into(),
            created_at: 1_700_000_000,
            tokens_prompt: 1500,
            tokens_completion: 250,
            tokens_cached: 1000,
            tokens_prompt_native: 2750,
            tokens_reasoning: 40,
            cost: 0.0012,
            latency_ms: 123,
            generation_time_ms: 5000,
            finish_reason: "stop".into(),
            streamed: true,
            cancelled: false,
            response_cached: false,
        }
    }

    #[test]
    fn line_renders_columns() {
        let l = table_text(&row_cells(&sample()));
        assert!(l.contains("model-name-abc")); // vendor+date stripped
        assert!(!l.contains("vendor/"));
        assert!(l.contains("Some..")); // provider >6 chars collapsed
        assert!(l.contains("$0.0012"));
        assert!(l.contains("2.8k tok")); // 1500+250+1000 = 2750 → 2.8k
        assert!(l.contains("c36%")); // 1000/2750 ≈ 36%
        assert!(l.contains("50.0/s")); // 250 completion / 5s = 50 tok/s
        // finish/app columns were dropped
        assert!(!l.contains("stop"));
        assert!(!l.contains("pi"));
    }

    #[test]
    fn provider_collapses_long_names() {
        assert_eq!(short_provider("OpenAI"), "OpenAI");
        assert_eq!(short_provider("Relace"), "Relace");
        assert_eq!(short_provider("GMICloud"), "GMIC..");
        assert_eq!(short_provider("Anthropic"), "Anth..");
    }

    #[test]
    fn tps_follows_openrouter_definition() {
        // streamed: duration = generation_time only
        assert_eq!(tps(&sample()), 50.0); // 250 / 5s
        // non-streamed: generation_time + latency count
        let mut e = sample();
        e.streamed = false;
        e.tokens_completion = 1000;
        e.generation_time_ms = 3900;
        e.latency_ms = 100;
        assert_eq!(tps(&e), 250.0); // 1000 / 4s
        // no timing reported → 0 (empty cell)
        let mut z = sample();
        z.generation_time_ms = 0;
        assert_eq!(tps(&z), 0.0);
        assert_eq!(fmt_tps(0.0), "");
        assert_eq!(fmt_tps(50.0), "50.0/s");
        assert_eq!(fmt_tps(150.0), "150/s");
        assert_eq!(fmt_tps(2200.0), "2.2k/s");
    }

    #[test]
    fn short_model_trims_vendor_and_date() {
        assert_eq!(
            short_model("deepseek/deepseek-v4-flash-20260731", 24),
            "deepseek-v4-flash"
        );
        assert_eq!(
            short_model("openai/gpt-5.6-luna-20260709", 24),
            "gpt-5.6-luna"
        );
        assert_eq!(short_model("anthropic/claude-3-7-sonnet", 24), "claude-3-7-sonnet");
        // still capped (as `..`) when it stays too long
        assert_eq!(
            short_model("a/really-really-long-model-name-20260101", 12),
            "really-rea.."
        );
        assert_eq!(short_model("plain-model", 24), "plain-model");
        assert_eq!(short_model("deepseek/deepseek-v4-flash-20260731", 3), "d..");
        assert_eq!(short_model("deepseek/deepseek-v4-flash-20260731", 1), "d");
    }

    #[test]
    fn cache_label_covers_hit_and_replay() {
        assert_eq!(cache_label(&sample()), "c36%");
        let mut e = sample();
        e.tokens_cached = 0;
        e.tokens_prompt_native = 0;
        assert_eq!(cache_label(&e), "");
        e.response_cached = true;
        assert_eq!(cache_label(&e), "c-hit");
    }

    #[test]
    fn parse_ts_accepts_rfc3339() {
        assert_eq!(parse_ts("2026-08-19T11:47:01.693Z"), 1787140021);
        assert_eq!(parse_ts("not-a-date"), 0);
    }

    #[test]
    fn sum_and_average_row_lines_up_with_columns() {
        // resolve column offsets from the shared layout so this test survives
        // future width tweaks
        let col_start = |i: usize| -> usize {
            LOG_COLUMNS[..i].iter().map(|(w, _)| w).sum::<usize>() + LOG_COL_SEP * i
        };
        let col_of = |s: &str, i: usize| -> String {
            let chars: Vec<char> = s.chars().collect();
            let start = col_start(i);
            chars
                .iter()
                .skip(start)
                .take(LOG_COLUMNS[i].0)
                .collect::<String>()
                .trim()
                .to_string()
        };
        let total_w = LOG_COLUMNS.iter().map(|(w, _)| w).sum::<usize>()
            + LOG_COL_SEP * (LOG_COLUMNS.len() - 1);
        let header = table_text(&header_cells());
        let row = table_text(&row_cells(&sample()));
        let sum = table_text(&summary_cells(&vec![sample()]));
        assert_eq!(header.chars().count(), total_w);
        assert_eq!(row.chars().count(), total_w);
        assert_eq!(sum.chars().count(), total_w);
        // column headers sit in the same slots the data uses
        assert_eq!(col_of(&header, 0), "time");
        assert_eq!(col_of(&header, 1), "model");
        assert_eq!(col_of(&header, 2), "provider");
        assert_eq!(col_of(&header, 3), "tokens");
        assert_eq!(col_of(&header, 4), "tps");
        assert_eq!(col_of(&header, 5), "cache");
        assert_eq!(col_of(&header, 6), "cost");
        // data fills the same slots (right-aligned numerics)
        assert_eq!(col_of(&row, 1), "model-name-abc");
        assert_eq!(col_of(&row, 2), "Some..");
        assert_eq!(col_of(&row, 3), "2.8k tok");
        assert_eq!(col_of(&row, 4), "50.0/s");
        assert_eq!(col_of(&row, 5), "c36%");
        assert_eq!(col_of(&row, 6), "$0.0012");
        // aggregate footer: Σ label, total tokens, avg tps/cache, total cost
        assert_eq!(col_of(&sum, 1), "Σ(1)"); // 1 entry covered
        assert_eq!(col_of(&sum, 3), "2.8k tok");
        assert_eq!(col_of(&sum, 4), "50.0/s");
        assert_eq!(col_of(&sum, 5), "c36%");
        assert_eq!(col_of(&sum, 6), "$0.0012");
        // zero-timing / no-cache entries are skipped from the averages
        let mut e2 = sample();
        e2.tokens_prompt = 1000;
        e2.tokens_completion = 0;
        e2.tokens_cached = 0;
        e2.tokens_prompt_native = 0;
        e2.generation_time_ms = 0;
        e2.cost = 0.0001;
        let s = summary_cells(&[sample(), e2]);
        assert_eq!(s[3], "3.8k tok"); // 2750+1000
        assert_eq!(s[4], "50.0/s"); // only the timed sample counts
        assert_eq!(s[5], "c36%");
        assert_eq!(s[6], "$0.0013"); // 0.0012 + 0.0001
    }

    #[test]
    fn summarize_prints_header_and_footer() {
        let logs = OrLogs {
            can_view_private_prompt_logs: true,
            entries: vec![sample()],
        };
        let txt = summarize(&logs);
        assert!(txt.contains("model"));
        assert!(txt.contains("Σ"));
        assert!(txt.contains("time"));
    }

    #[test]
    fn zero_cost_renders_as_dollar_zero() {
        let mut e = sample();
        e.cost = 0.0;
        // not panicking and still has the model column
        assert!(table_text(&row_cells(&e)).contains("model-name-abc"));
    }
}
