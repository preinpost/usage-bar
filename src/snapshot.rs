//! Assemble a full Snapshot from all collectors.

use chrono::Local;

use crate::config::Config;
use crate::providers::{copilot, data, grok, opencode_go};
use crate::model::{Snapshot, Status};
use crate::openrouter;

pub fn snapshot(cfg: &Config) -> Snapshot {
    let start = data::window_start(cfg.reset_hour);
    let end = data::window_end(start);
    let reset = end;
    let now = Local::now();
    let reset_secs = (reset - now).num_seconds().max(0);
    let countdown = crate::model::countdown_text(reset_secs);
    Snapshot {
        countdown,
        reset_seconds: reset_secs,
        reset_at: reset.to_rfc3339(),
        claude: data::collect_claude(start, &cfg.prices),
        codex: data::collect_codex(start),
        opencode: data::collect_opencode(start),
        copilot: copilot::collect(cfg),
        grok: grok::collect(start),
        opencode_go: opencode_go::collect(cfg),
        openrouter: openrouter::collect(cfg),
        or_usage: crate::openrouter::load(),
    }
}

/// Human-readable multi-line summary (used by `status` mode).
pub fn summarize(s: &Snapshot, cfg: &Config) -> String {
    use crate::model::{fmt_money, fmt_tok};

    // Only show providers that are actually connected / have data, so the
    // one-shot summary matches the panel (unless show_no_data_providers).
    let show_all = cfg.show_no_data_providers;
    let c_active = s.claude.msgs > 0 || s.claude.has_token_data;
    let x_active = s.codex.sessions > 0 || s.codex.turns > 0 || s.codex.has_token_data;
    let o_active = s.opencode.as_ref().map(|o| o.sessions > 0 || o.has_token_data).unwrap_or(false);
    let cp_active = matches!(&s.copilot, Status::Ok(_));
    let g_active = !s.grok.needs_login || s.grok.local_sessions > 0;
    let ogo_active = !s.opencode_go.needs_key;
    let or_active = !s.openrouter.needs_key;

    let mut out = String::new();
    let _ = write!(out, "╭ Usage · resets in {} ╮\n", s.countdown);

    if !(show_all || c_active || x_active || o_active || cp_active || g_active || ogo_active || or_active) {
        let _ = write!(out, "(no active providers — connect with `login` or set s API key)\n");
        return out;
    }

    // ---- Claude Code
    if show_all || c_active {
        let c = &s.claude;
        let _ = write!(
            out,
            "Claude Code in {} out {} cache {}   {}\n",
            fmt_tok(c.tokens.input),
            fmt_tok(c.tokens.output),
            fmt_tok(c.tokens.cache_read + c.tokens.cache_write),
            fmt_money(c.cost)
        );
        if c.tokens.total() > 0 || c.msgs > 0 {
            let _ = write!(out, "  {}", crate::model::budget_line("budget", c.tokens.total(), cfg.claude_budget));
        }
        for item in c.items.iter().take(3) {
            let _ = write!(out, "  · {}  {}\n", truncate(&item.name, 28), fmt_tok(item.tokens.total()));
        }
    }

    // ---- Codex
    if show_all || x_active {
        let x = &s.codex;
        let token_s = if x.has_token_data {
            format!("in {} out {} cache {}", fmt_tok(x.tokens.input), fmt_tok(x.tokens.output), fmt_tok(x.tokens.cache_read + x.tokens.cache_write))
        } else {
            format!("{} sessions · {} turns (no token data)", x.sessions, x.turns)
        };
        let _ = write!(out, "Codex {token_s}   {}\n", fmt_money(x.cost));
    }

    // ---- OpenCode
    if show_all || o_active {
        if let Some(o) = &s.opencode {
            let _ = write!(
                out,
                "OpenCode in {} out {} cache {}   {}\n",
                fmt_tok(o.tokens.input),
                fmt_tok(o.tokens.output),
                fmt_tok(o.tokens.cache_read + o.tokens.cache_write),
                fmt_money(o.cost)
            );
            for item in o.items.iter().take(2) {
                let _ = write!(out, "  · {}  {}\n", truncate(&item.name, 28), fmt_tok(item.tokens.total()));
            }
        }
    }

    // ---- OpenCode Go
    if show_all || ogo_active {
        use crate::providers::opencode_go::format_resets_at;
        if s.opencode_go.needs_key {
            let _ = write!(out, "OpenCode Go — no key (pi auth.json / OPENCODE_API_KEY)\n");
        } else if let Some(e) = &s.opencode_go.error {
            let _ = write!(out, "OpenCode Go — {e}\n");
        } else {
            let _ = write!(out, "OpenCode Go\n");
            for (label, w) in [
                ("rolling", &s.opencode_go.rolling),
                ("weekly", &s.opencode_go.weekly),
                ("monthly", &s.opencode_go.monthly),
            ] {
                if let Some(w) = w {
                    let _ = write!(
                        out,
                        "  {label:<8} {}% · resets {}\n",
                        w.percent,
                        format_resets_at(w.resets_at.as_deref()),
                    );
                }
            }
        }
    }

    // ---- Copilot
    if show_all || cp_active {
        match &s.copilot {
            Status::Ok(cp) => {
                // Display already carries the AI-credit counts and skips the
                // unlimited chat/completions rows, so no extra quota/bar line.
                let meta = format!("{} · reset {}", cp.plan, cp.reset);
                let _ = write!(out, "Copilot {cp}   {meta}\n");
            }
            other => {
                let _ = write!(out, "Copilot — {other}\n");
            }
        }
    }

    // ---- Grok
    if show_all || g_active {
        if s.grok.needs_login {
            let _ = write!(out, "Grok — needs 'grok login' or GROK_OAUTH_TOKEN\n");
        } else if let Some(e) = &s.grok.error {
            let _ = write!(out, "Grok — error ({e})\n");
        } else if let Some(p) = s.grok.used_pct {
            let resets = s.grok.resets_at.as_deref().unwrap_or("—");
            let _ = write!(out, "Grok credits {p:.0}% used · resets {resets}\n");
        }
    }
    if s.grok.local_sessions > 0 {
        let _ = write!(out, "  · grok local: {} sessions", s.grok.local_sessions);
        if s.grok.local_tokens > 0 {
            let _ = write!(out, " · {}", fmt_tok(s.grok.local_tokens));
        }
        let _ = write!(out, "\n");
    }

    // ---- OpenRouter
    if show_all || or_active {
        if let Some(e) = &s.openrouter.error {
            let _ = write!(out, "OpenRouter — error ({e})\n");
        } else {
            let bal = fmt_money(s.openrouter.balance_usd);
            let _ = write!(out, "OpenRouter credits {bal}\n");
            if s.openrouter.total_credits_usd > 0.0 {
                let _ = write!(
                    out,
                    "  credits {} used of {} added\n",
                    fmt_money(s.openrouter.total_usage_usd),
                    fmt_money(s.openrouter.total_credits_usd),
                );
            }
            if let (Some(p), Some(limit)) = (s.openrouter.used_pct, s.openrouter.key_limit_usd) {
                let used = s.openrouter.key_used_usd.unwrap_or(0.0);
                let win = s.openrouter.reset_window.as_deref().unwrap_or("");
                let _ = write!(
                    out,
                    "  key {p}% used · {}/{}{}\n",
                    fmt_money(used),
                    fmt_money(limit),
                    if win.is_empty() { String::new() } else { format!(" · resets {win}") },
                );
            }
            if s.openrouter.usage_today > 0.0 || s.openrouter.usage_week > 0.0 || s.openrouter.usage_month > 0.0 {
                let _ = write!(
                    out,
                    "  spend today {} · week {} · month {}\n",
                    fmt_money(s.openrouter.usage_today),
                    fmt_money(s.openrouter.usage_week),
                    fmt_money(s.openrouter.usage_month),
                );
            } else if s.openrouter.key_limit_usd.is_none() && s.openrouter.total_usage_usd > 0.0 {
                let _ = write!(out, "  hint: set a key spending limit for daily/weekly/monthly spend\n");
            }
            // per-model usage from the web-dashboard scrub
            if let Some(u) = &s.or_usage {
                let _ = write!(out, "  month {}\n", fmt_money(u.month_total));
                for m in u.month_models.iter().take(6) {
                    let _ = write!(
                        out,
                        "     · {:<22} {:>9}  {:>8}\n",
                        truncate(&m.label, 22),
                        crate::model::fmt_compact(m.tokens),
                        fmt_money(m.cost),
                    );
                }
                if !u.today_models.is_empty() {
                    let _ = write!(out, "  today {}\n", fmt_money(u.today_total));
                    for m in u.today_models.iter().take(3) {
                        let _ = write!(
                            out,
                            "     · {:<22} {:>9}  {:>8}\n",
                            truncate(&m.label, 22),
                            crate::model::fmt_compact(m.tokens),
                            fmt_money(m.cost),
                        );
                    }
                }
            }
        }
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
}

use std::fmt::Write as _;