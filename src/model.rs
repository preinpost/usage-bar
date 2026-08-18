//! Shared data model + formatting helpers.

use std::fmt::Write as _;

#[derive(Debug, Clone, Default)]
pub struct Item {
    pub name: String,
    pub tokens: Tokens,
    pub cost: f64,
    pub msgs: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
}

impl Tokens {
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write + self.reasoning
    }
}

#[derive(Debug, Clone, Default)]
pub struct LocalStats {
    pub tokens: Tokens,
    pub cost: f64,
    pub items: Vec<Item>,
    pub msgs: u64,
    pub sessions: u64,
    pub turns: u64,
    pub has_token_data: bool,
}

#[derive(Debug, Clone, Default)]
pub struct CopilotQuota {
    pub name: String,
    pub used_pct: Option<u8>,
    pub unlimited: bool,
    /// tokens / credits consumed so far (token-based plans only)
    pub used: u64,
    /// total tokens / credits entitled for the window
    pub entitlement: u64,
}

#[derive(Debug, Clone)]
pub enum Status {
    NeedsLogin { hint: String },
    Err { msg: String },
    Ok(CopilotStatus),
}

impl Default for Status {
    fn default() -> Self {
        Status::NeedsLogin { hint: String::new() }
    }
}

#[derive(Debug, Clone)]
pub enum _StatusWorking {
    NeedsLogin { hint: String },
    Err { msg: String },
    Ok(CopilotStatus),
}

#[derive(Debug, Clone, Default)]
pub struct CopilotStatus {
    pub plan: String,
    pub reset: String,
    pub login: String,
    pub quotas: Vec<CopilotQuota>,
}

impl std::fmt::Display for CopilotStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut out = String::new();
        for q in &self.quotas {
            // skip unlimited (∞) rows — chat/completions are noise on
            // token-based plans; only report what actually runs out.
            if q.unlimited {
                continue;
            }
            if !out.is_empty() {
                out.push_str(" · ");
            }
            if let Some(p) = q.used_pct {
                let _ = write!(out, "{} {p}%", q.name);
            } else {
                let _ = write!(out, "{}", q.name);
            }
            if q.entitlement > 0 {
                let _ = write!(out, " ({} / {})", fmt_int(q.used), fmt_int(q.entitlement));
            }
        }
        write!(f, "{out}")
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Status::NeedsLogin { hint } => write!(f, "needs login ({hint})"),
            Status::Err { msg } => write!(f, "error: {msg}"),
            Status::Ok(s) => write!(f, "{s}"),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct GrokStatus {
    pub needs_login: bool,
    pub error: Option<String>,
    pub used_pct: Option<f64>,
    pub resets_at: Option<String>,
    pub local_sessions: u64,
    pub local_tokens: u64,
}

/// One quota window from the OpenCode Go usage API.
#[derive(Debug, Clone, Default)]
pub struct GoUsageWindow {
    /// `ok` or `rate-limited`
    pub status: String,
    pub percent: u8,
    /// ISO-8601 timestamp of the window's reset
    pub resets_at: Option<String>,
}

impl GoUsageWindow {
    pub fn rate_limited(&self) -> bool {
        self.status == "rate-limited"
    }
}

/// OpenCode Go (opencode.ai) subscription quota — rolling / weekly / monthly
/// usage windows from the official `GET /zen/go/v1/usage` endpoint.
#[derive(Debug, Clone, Default)]
pub struct GoUsageStatus {
    /// no opencode-go key found (env or `~/.local/share/opencode/auth.json`)
    pub needs_key: bool,
    pub error: Option<String>,
    /// true when the workspace actually returned quota rows (active Go plan)
    pub subscribed: bool,
    pub rolling: Option<GoUsageWindow>,
    pub weekly: Option<GoUsageWindow>,
    pub monthly: Option<GoUsageWindow>,
}

#[derive(Debug, Clone, Default)]
pub struct OpenRouterStatus {
    /// no API key configured (env `OPENROUTER_API_KEY` or secrets/openrouter.json)
    pub needs_key: bool,
    pub error: Option<String>,
    /// prepaid credits remaining (USD)
    pub balance_usd: f64,
    pub total_credits_usd: f64,
    pub total_usage_usd: f64,
    /// API-key spending limit — only set when the key has a configured limit
    pub key_limit_usd: Option<f64>,
    pub key_used_usd: Option<f64>,
    pub key_remaining_usd: Option<f64>,
    pub reset_window: Option<String>,
    pub used_pct: Option<u8>,
    /// key-level spend by window (from `/key`; nonzero once a limit is set)
    pub usage_today: f64,
    pub usage_week: f64,
    pub usage_month: f64,
}

#[derive(Debug, Clone, Default)]
pub struct ModelUsage {
    pub label: String,
    pub cost: f64,
    pub tokens: u64,
}

/// Per-model + daily/monthly usage pulled from the OpenRouter web dashboard
/// (via the internalized `or_sync` / `sync-openrouter` cache).
#[derive(Debug, Clone, Default)]
pub struct OpenRouterUsage {
    pub fetched_at: i64,
    pub today_total: f64,
    pub today_models: Vec<ModelUsage>,
    pub month_total: f64,
    pub month_models: Vec<ModelUsage>,
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub countdown: String,
    pub reset_seconds: i64,
    pub reset_at: String,
    pub claude: LocalStats,
    pub codex: LocalStats,
    pub opencode: Option<LocalStats>,
    pub copilot: Status,
    pub grok: GrokStatus,
    pub opencode_go: GoUsageStatus,
    pub openrouter: OpenRouterStatus,
    pub or_usage: Option<OpenRouterUsage>,
}

/// Compact count like `3.4k`, `34.9M`, `592.9M`, `1.25B`.
pub fn fmt_compact(n: u64) -> String {
    if n >= 1_000_000_000 {
        format!("{:.2}B", n as f64 / 1e9)
    } else if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1e3)
    } else {
        format!("{n}")
    }
}

pub fn fmt_tok(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1e3)
    } else {
        format!("{n}")
    }
}

/// Thousands-separated integer, e.g. `10,000`.
pub fn fmt_int(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub fn fmt_money(v: f64) -> String {
    if v.abs() < 100_000.0 {
        format!("${:.2}", v)
    } else {
        format!("${:.1}K", v / 1e3)
    }
}

pub fn bar(frac: f64, width: usize) -> String {
    let frac = frac.clamp(0.0, 1.0);
    let filled = (frac * width as f64).round() as usize;
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

/// One line like `budget 10.00M  ██░░░░ 5%`
pub fn budget_line(label: &str, total: u64, budget: u64) -> String {
    let frac = if budget > 0 { total as f64 / budget as f64 } else { 0.0 };
    format!(
        "{label} {}  {} {:.0}%",
        fmt_tok(budget),
        bar(frac, 20),
        frac * 100.0
    )
}

/// Human countdown like `12h 03m`, `45m`, or `1d 4h`.
pub fn countdown_text(secs: i64) -> String {
    let secs = secs.max(0);
    if secs == 0 {
        return "0m".into();
    }
    let d = secs / 86_400;
    let h = (secs % 86_400) / 3_600;
    let m = (secs % 3_600) / 60;
    let s = secs % 60;
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m:02}m")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tokens_sum() {
        let t = Tokens { input: 1, output: 2, cache_read: 3, cache_write: 4, reasoning: 5 };
        assert_eq!(t.total(), 15);
    }

    #[test]
    fn countdown_text_formats() {
        assert_eq!(countdown_text(0), "0m");
        assert_eq!(countdown_text(-10), "0m");
        assert_eq!(countdown_text(3 * 3600), "3h 00m");
        assert_eq!(countdown_text(12 * 3600 + 3 * 60), "12h 03m");
        assert_eq!(countdown_text(2 * 86_400 + 5 * 3600), "2d 5h");
        assert_eq!(countdown_text(75), "1:15");
    }
}