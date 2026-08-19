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

Prebuilt binaries for linux / macos / windows (amd64 + arm64) are published on
every GitHub [release](https://github.com/preinpost/usage-bar/releases), ad-hoc
codesigned on macOS and produced by GitHub Actions.

### mise (recommended)

```sh
mise use github:preinpost/usage-bar          # latest release
mise use github:preinpost/usage-bar@0.1.2    # pin a version
mise x github:preinpost/usage-bar -- ub status   # run once, no config
```

mise auto-detects the right per-platform binary, verifies GitHub artifact
attestations + SLSA provenance, strips the `-<os>-<arch>` suffix, and puts
`ub` on PATH. For a team, pin it portably in `mise.toml`:

```toml
[tool_alias]
ub = "github:preinpost/usage-bar"

[tools.ub]
version = "0.1.2"
```

### from source

```sh
cargo build --release
# binary: target/release/ub
```

## Usage

```sh
ub                          # live TUI dashboard
ub status                   # one-shot text summary (for actions/scripts)
ub status --json            # JSON snapshot
ub watch --json             # same, via watch mode
ub login copilot            # GitHub Copilot device flow (prints code, polls, saves token)
ub login grok               # instructions (grok login / GROK_OAUTH_TOKEN)
ub login openrouter         # paste an API key (sk-or-...)
ub login opencode           # paste an OpenCode Go key (sk-...)
ub logout                   # clear ALL saved tokens/keys
ub logout opencode          # clear one provider only (copilot|openrouter|opencode|grok|all)
ub sync-openrouter          # refresh the per-model usage cache from the web dashboard
```

`sync-openrouter` pulls your openrouter.ai login from **Chrome** to query the
web dashboard's per-model analytics (the public API only exposes totals). On
macOS it decrypts the Chrome cookies with the Keychain + AES-128-CBC; on
Windows it unwraps the DPAPI key from `Local State` and decrypts the
AES-256-GCM cookies. Requires an active openrouter.ai login in the default
Chrome profile; Chrome 127+ App-Bound Encryption (`v20/v21`) cookies are not
supported yet (the sync reports a clear error). On platforms without the
Chrome-cookie pull (Linux, and headless environments) the per-model breakdown
is omitted and usage falls back to the public-API credits / key / windowed-
spend view — the provider still works, just without per-model detail.

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

Provider credentials are stored in the **OS keyring** when available —
macOS Keychain, Windows Credential Manager, or Linux Secret Service
(gnome-keyring / KWallet) — under the `usage-bar` service namespace. On
machines without a reachable secure store (headless Linux, CI, locked
keychain) they transparently fall back to `<config>/secrets/` with `0600`
permissions, and legacy files are promoted into the keyring on first read
(new tokens still go through the file only if the keyring is unusable).
Config dir: `$HERDR_PLUGIN_CONFIG_DIR` or `~/.config/usage-bar`.

| Provider    | Keyring account / file      | Env fallback             |
| ----------- | --------------------------- | ------------------------ |
| Copilot     | `secrets/copilot.json`      | — (device flow only)     |
| OpenRouter  | `secrets/openrouter.json`   | `OPENROUTER_API_KEY`     |
| OpenCode Go | `secrets/opencode-go.json`  | `OPENCODE_API_KEY`       |
| Grok        | `~/.grok/auth.json`         | `GROK_OAUTH_TOKEN`       |

OpenCode Go is also auto-detected from `~/.pi/agent/auth.json` (pi's
credential store) and `~/.local/share/opencode/auth.json` when no key was
saved explicitly. Saved key priority: env → keyring → auto-detection.

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

## Release

Versioned releases build and publish 5 binaries (Linux amd64/arm64, macOS
arm64, Windows amd64/arm64) via a shared build workflow
(`.github/workflows/build.yml`):

- **Every push to master** (`.github/workflows/ci.yml`) validates all 5 build
  targets and runs the test suite only — nothing is published automatically
- **Versioned releases** are manual (`release.yml`):

- all 5 combos build on native GitHub-hosted runners — no QEMU, no zig
- run it from the **Actions → bump & release → Run workflow**, optionally with
  an explicit version (default: auto patch/minor/major bump from Cargo.toml)
- the workflow bumps the version (Cargo.toml + Cargo.lock), commits, tags
  `vX.Y.Z`, builds all targets against the tag and attaches them to a GitHub
  Release with auto-generated notes

Note: the bump commit is pushed to the branch the workflow runs on, so that
branch must not be protected against direct pushes. The macOS binary is
ad-hoc signed (it runs locally; for wide distribution sign with a real
Developer ID certificate instead).
