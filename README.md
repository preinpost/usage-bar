# usage-bar

A terminal usage dashboard for AI coding agents. Shows daily token/cost budgets
and provider quotas at a glance, both as a live TUI and as one-shot text/JSON
output for scripts. Originally built as the core of a Herdr plugin panel — it
runs standalone today and can be ported into any panel/plugin.

## Providers

| Provider        | Source                                                                 |
| --------------- | ---------------------------------------------------------------------- |
| Claude Code     | `~/.claude/projects/*/*.jsonl` (usage/cost per message)                |
| Codex           | `~/.codex/sessions` + `~/.codex/history.jsonl`                         |
| OpenCode        | `~/.local/share/opencode/opencode.db` (SQLite sessions)                |
| GitHub Copilot  | OAuth device flow + internal usage API (AI-credit quotas)              |
| Grok (xAI)      | `~/.grok/auth.json` / `GROK_OAUTH_TOKEN` + billing proxy               |
| OpenRouter      | `/credits` + `/key` API (balance, key-limit meter, daily/weekly/monthly spend) |
| OpenCode Go     | official quota API `GET https://opencode.ai/zen/go/v1/usage` (rolling / weekly / monthly windows) |

Each provider shows only when it is actually connected or has data this window
(set `show_no_data_providers: true` to always show everything).

## Install

```sh
cargo build --release
# binary: target/release/usage-bar
```

## Usage

```sh
usage-bar                    # live TUI dashboard
usage-bar status             # one-shot text summary (for actions/scripts)
usage-bar status --json      # JSON snapshot
usage-bar watch --json       # same, via watch mode
usage-bar login copilot      # GitHub Copilot device flow (prints code, polls, saves token)
usage-bar login grok         # instructions (grok login / GROK_OAUTH_TOKEN)
usage-bar login openrouter   # paste an API key (sk-or-...)
usage-bar login opencode     # paste an OpenCode Go key (sk-...)
usage-bar logout             # clear ALL saved tokens/keys
usage-bar logout opencode    # clear one provider only (copilot|openrouter|opencode|grok|all)
usage-bar sync-openrouter    # refresh the per-model usage cache from the web dashboard
```

### JSON output

`status --json` emits a flat snapshot: `claude`, `codex`, `opencode`,
`copilot`, `grok`, `opencode_go`, `openrouter`, `openrouter_usage`,
plus `countdown` / `reset_seconds` / `reset_at` for the daily window.

### TUI keys

| Key              | Action                                  |
| ---------------- | --------------------------------------- |
| `q` / `x` / `Esc`| quit                                    |
| `c`              | open the Connect menu (login / logout)  |
| `l`              | start Copilot login                     |
| `g`              | show Grok login instructions            |
| `r`              | refresh now (+ resync OpenRouter cache) |
| `1`–`0`          | fold/unfold the section bound to that digit |
| `?`              | settings — assign fold digits to sections  |
| `j`/`k` or `↑`/`↓`| scroll the body (when it overflows)    |
| `PgUp`/`PgDn`    | scroll by a page                        |
| `↑`/`↓` / `k`/`j`| move in menus                           |
| `Enter`          | select                                  |

The footer buttons are clickable with the mouse; provider rows in the Connect
menu are clickable too. On the dashboard, the fold marker (`1▾ …`) left of each
provider title is clickable, and the mouse wheel scrolls the body. `?` opens
the fold-key settings: highlight a section and press a digit (`1`–`0`) to bind
it, `x` to unbind — bindings persist in `keymap.json`. Defaults are the
registration order (Claude Code = 1 … OpenRouter = 7, 8/9/0 free). On small
panes both the body and the Connect menu scroll (wheel / arrows), and modals
size themselves to their content instead of overflowing.

## Credentials

Keys and tokens are stored in `<config>/secrets/` with `0600` permissions.
Config dir: `$HERDR_PLUGIN_CONFIG_DIR` or `~/.config/usage-bar`.

| Provider    | Config file                    | Env fallback             |
| ----------- | ------------------------------ | ------------------------ |
| Copilot     | `secrets/copilot.json`         | — (device flow only)     |
| OpenRouter  | `secrets/openrouter.json`      | `OPENROUTER_API_KEY`     |
| OpenCode Go | `secrets/opencode-go.json`     | `OPENCODE_API_KEY`       |
| Grok        | `~/.grok/auth.json`            | `GROK_OAUTH_TOKEN`       |

OpenCode Go is also auto-detected from `~/.pi/agent/auth.json` (pi's
credential store) and `~/.local/share/opencode/auth.json` when no key was
saved explicitly. Saved key priority: env → saved key → auto-detection.

## Config

`~/.config/usage-bar/config.json` (or `$HERDR_PLUGIN_CONFIG_DIR`):

```json
{
  "claude_daily_budget_tokens": 10000000,
  "codex_daily_budget_tokens": 10000000,
  "opencode_daily_budget_tokens": 10000000,
  "reset_hour": 0,
  "refresh_seconds": 30,
  "show_no_data_providers": false,
  "prices": {
    "input": 3.0,
    "output": 15.0,
    "cache_read": 0.30,
    "cache_write": 3.75
  }
}
```

The daily window starts at `reset_hour` (local midnight by default); budget
bars compare the window's token usage against the configured budgets.

## Development

```sh
cargo test
cargo clippy --all-targets
```