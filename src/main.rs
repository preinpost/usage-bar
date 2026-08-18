//! herdr-codexbar-tui — ratatui usage dashboard for Herdr plugin panes.
//!
//! Modes:
//!   watch (default)  full TUI dashboard
//!   status           one-shot text summary to stdout (for actions/scripts)
//!   login copilot    headless device flow (prints code, polls, saves token)
//!   login grok       instructions
//!   logout           clear saved plugin tokens
//!   --json           JSON snapshot (with status or watch --once)

mod config;
mod http;
mod model;
mod openrouter;
mod providers;
mod snapshot;
mod ui;

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let json_out = args.iter().any(|a| a == "--json");
    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();

    let cfg = config::load();
    config::ensure_dirs(&cfg);

    let mode = positional.first().map(|s| s.as_str()).unwrap_or("watch");
    match mode {
        "watch" => {
            if json_out {
                let snap = snapshot::snapshot(&cfg);
                print_json(&snap);
                return ExitCode::SUCCESS;
            }
            match ui::run(cfg) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("tui error: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        "status" => {
            let snap = snapshot::snapshot(&cfg);
            if json_out {
                print_json(&snap);
            } else {
                print!("{}", snapshot::summarize(&snap, &cfg));
            }
            ExitCode::SUCCESS
        }
        "login" => {
            let provider = positional.get(1).map(|s| s.as_str()).unwrap_or("");
            match provider {
                "copilot" | "github" => {
                    providers::copilot::device_login(&cfg, |line| println!("{line}"));
                    let snap = snapshot::snapshot(&cfg);
                    print!("{}", snapshot::summarize(&snap, &cfg));
                    ExitCode::SUCCESS
                }
                "grok" | "xai" => {
                    println!("▶ Grok login");
                    println!("  1) Install the Grok Build CLI and run:  grok login");
                    println!("  2) Or set GROK_OAUTH_TOKEN=<bearer> for the panel process.");
                    ExitCode::SUCCESS
                }
                "opencode" | "opencode-go" => {
                    use std::io::BufRead as _;
                    println!("▶ OpenCode Go API key");
                    println!(
                        "  1) Find the key in the opencode console: https://opencode.ai/console → Keys"
                    );
                    println!("     (when using opencode-go from pi, it is already auto-detected)");
                    println!("  2) Or set OPENCODE_API_KEY=<sk-...> for this process.");
                    println!("Paste the key (sk-...) and press Enter:");
                    let mut key = String::new();
                    if std::io::stdin().lock().read_line(&mut key).is_err() || key.trim().is_empty()
                    {
                        eprintln!("no key given — aborting (use OPENCODE_API_KEY env instead)");
                        return ExitCode::FAILURE;
                    }
                    providers::opencode_go::save_key(&cfg, key.trim());
                    let snap = snapshot::snapshot(&cfg);
                    print!("{}", snapshot::summarize(&snap, &cfg));
                    ExitCode::SUCCESS
                }
                "openrouter" => {
                    use std::io::BufRead as _;
                    println!("▶ OpenRouter API key");
                    println!("  1) Create a key: https://openrouter.ai/settings/keys");
                    println!(
                        "  2) (optional) Set a key spending limit at https://openrouter.ai/settings/limits"
                    );
                    println!("Paste the key (sk-or-v1-...) and press Enter:");
                    let mut key = String::new();
                    if std::io::stdin().lock().read_line(&mut key).is_err() || key.trim().is_empty()
                    {
                        eprintln!("no key given — aborting (use OPENROUTER_API_KEY env instead)");
                        return ExitCode::FAILURE;
                    }
                    openrouter::save_key(&cfg, key.trim());
                    let snap = snapshot::snapshot(&cfg);
                    print!("{}", snapshot::summarize(&snap, &cfg));
                    ExitCode::SUCCESS
                }
                _ => {
                    eprintln!(
                        "unknown provider: {provider} (copilot | grok | openrouter | opencode)"
                    );
                    ExitCode::FAILURE
                }
            }
        }
        "logout" => {
            // per-provider target; bare `logout` clears everything (backwards compat)
            let target = positional.get(1).map(|s| s.as_str()).unwrap_or("all");
            match target {
                "all" => {
                    providers::copilot::clear_token(&cfg);
                    openrouter::clear_key(&cfg);
                    providers::opencode_go::clear_key(&cfg);
                    println!("✓ cleared all saved OAuth/API tokens (~/.grok/auth.json untouched)");
                }
                "copilot" | "github" => {
                    providers::copilot::clear_token(&cfg);
                    println!("✓ cleared GitHub Copilot OAuth token");
                }
                "openrouter" => {
                    openrouter::clear_key(&cfg);
                    println!("✓ cleared OpenRouter API key");
                }
                "opencode" | "opencode-go" | "opencode_go" => {
                    providers::opencode_go::clear_key(&cfg);
                    println!("✓ cleared saved OpenCode Go API key (auto-detection still applies)");
                }
                "grok" | "xai" => {
                    println!(
                        "Grok keeps no token in this app — credentials live in ~/.grok/auth.json and are removed separately."
                    );
                }
                other => {
                    eprintln!(
                        "unknown logout target: {other} (all | copilot | openrouter | opencode | grok)"
                    );
                    return ExitCode::FAILURE;
                }
            }
            ExitCode::SUCCESS
        }
        "sync-openrouter" => {
            // one-shot refresh of the per-model usage cache (Chrome session)
            match openrouter::sync_now(&cfg) {
                Ok(s) => {
                    println!("{s}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("sync-openrouter failed: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        _ => {
            eprintln!("unknown mode: {mode} (watch | status | login | logout)");
            ExitCode::FAILURE
        }
    }
}

fn print_json(snap: &model::Snapshot) {
    let v = serde_json::json!({
        "countdown": snap.countdown,
        "reset_seconds": snap.reset_seconds,
        "reset_at": snap.reset_at,
        "claude": {
            "tokens": snap.claude.tokens.total(),
            "cost": snap.claude.cost,
            "msgs": snap.claude.msgs,
            "input": snap.claude.tokens.input,
            "output": snap.claude.tokens.output,
            "cache_read": snap.claude.tokens.cache_read,
            "cache_write": snap.claude.tokens.cache_write,
            "per_project": snap.claude.items.iter().take(3).map(|i| {
                serde_json::json!({"name": i.name, "tokens": i.tokens.total(), "cost": i.cost})
            }).collect::<Vec<_>>(),
        },
        "codex": {
            "sessions": snap.codex.sessions,
            "turns": snap.codex.turns,
            "tokens": snap.codex.tokens.total(),
            "cost": snap.codex.cost,
            "has_token_data": snap.codex.has_token_data,
        },
        "opencode": snap.opencode.as_ref().map(|o| serde_json::json!({
            "sessions": o.sessions,
            "tokens": o.tokens.total(),
            "cost": o.cost,
            "top": o.items.iter().take(3).map(|i| serde_json::json!({"name": i.name, "tokens": i.tokens.total(), "cost": i.cost})).collect::<Vec<_>>(),
        })),
        "copilot": match &snap.copilot {
            model::Status::Ok(cp) => serde_json::json!({
                "needs_login": false,
                "plan": cp.plan,
                "reset": cp.reset,
                "login": cp.login,
                "quotas": cp.quotas.iter().map(|q| serde_json::json!({
                    "name": q.name,
                    "used_pct": q.used_pct,
                    "unlimited": q.unlimited,
                    "used": q.used,
                    "entitlement": q.entitlement,
                })).collect::<Vec<_>>(),
            }),
            model::Status::NeedsLogin { hint } => serde_json::json!({"needs_login": true, "hint": hint}),
            model::Status::Err { msg } => serde_json::json!({"needs_login": false, "error": msg}),
        },
        "grok": {
            "needs_login": snap.grok.needs_login,
            "error": snap.grok.error,
            "used_pct": snap.grok.used_pct,
            "resets_at": snap.grok.resets_at,
            "local_sessions": snap.grok.local_sessions,
            "local_tokens": snap.grok.local_tokens,
        },
        "opencode_go": {
            "needs_key": snap.opencode_go.needs_key,
            "error": snap.opencode_go.error,
            "subscribed": snap.opencode_go.subscribed,
            "rolling": snap.opencode_go.rolling.as_ref().map(|w| serde_json::json!({"status": w.status, "percent": w.percent, "resets_at": w.resets_at})),
            "weekly": snap.opencode_go.weekly.as_ref().map(|w| serde_json::json!({"status": w.status, "percent": w.percent, "resets_at": w.resets_at})),
            "monthly": snap.opencode_go.monthly.as_ref().map(|w| serde_json::json!({"status": w.status, "percent": w.percent, "resets_at": w.resets_at})),
        },
        "openrouter": {
            "needs_key": snap.openrouter.needs_key,
            "error": snap.openrouter.error,
            "balance_usd": snap.openrouter.balance_usd,
            "total_credits_usd": snap.openrouter.total_credits_usd,
            "total_usage_usd": snap.openrouter.total_usage_usd,
            "key_limit_usd": snap.openrouter.key_limit_usd,
            "key_used_usd": snap.openrouter.key_used_usd,
            "key_remaining_usd": snap.openrouter.key_remaining_usd,
            "reset_window": snap.openrouter.reset_window,
            "used_pct": snap.openrouter.used_pct,
            "usage_today": snap.openrouter.usage_today,
            "usage_week": snap.openrouter.usage_week,
            "usage_month": snap.openrouter.usage_month,
        },
        "openrouter_usage": snap.or_usage.as_ref().map(|u| serde_json::json!({
            "fetched_at": u.fetched_at,
            "today": { "total": u.today_total, "models": u.today_models.iter().map(|m| serde_json::json!({ "label": m.label, "cost": m.cost, "tokens": m.tokens })).collect::<Vec<_>>() },
            "month": { "total": u.month_total, "models": u.month_models.iter().map(|m| serde_json::json!({ "label": m.label, "cost": m.cost, "tokens": m.tokens })).collect::<Vec<_>>() },
        })),
    });
    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
}
