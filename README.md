# codex-usage-monit

**English** · [简体中文](README.zh-CN.md)

[![CI](https://github.com/ghostroller/codex-usage-monit/actions/workflows/ci.yml/badge.svg)](https://github.com/ghostroller/codex-usage-monit/actions/workflows/ci.yml)
[![Latest release](https://img.shields.io/github/v/release/ghostroller/codex-usage-monit)](https://github.com/ghostroller/codex-usage-monit/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**Codex usage monitoring, entirely in your terminal.**

`codex-usage-monit` tracks Codex quota windows, reset times, reset credits, tasks, turns, models, and locally observed token usage. Run it as an interactive TUI, or use its non-interactive CLI to produce plain text or JSON for scripts, cron jobs, and CI.

It is local-first and terminal-native: no desktop application, browser, daemon, database, or listening port is required. Prebuilt binaries run on macOS and Linux, including headless development servers over SSH.

## Highlights

- **Account usage at a glance** — See used/remaining percentages, all available quota buckets, and server-reported reset times.
- **Reset-credit details** — See the authoritative available count plus grant and expiry times when the server returns per-credit details.
- **Expiry reminder** — The weekly Overview gauge warns when the earliest fully known available reset credit expires before the ordinary Codex weekly reset, with the exact local expiry time.
- **Local usage breakdown** — Explore tasks, turns, models, token totals, and token share for the current 5-hour or weekly reset cycle.
- **Interactive terminal UI** — Filter, search, switch scopes, expand task trees, inspect turns/models, and resume tasks without leaving the terminal.
- **Scriptable CLI** — Export human-readable text or schema-versioned camelCase JSON, select individual sections, and filter turns by thread.
- **Server-friendly** — Works in SSH, tmux, and Zellij sessions; Linux release binaries are static musl builds with no host glibc dependency.
- **Read-only monitoring** — Reads local Codex data and account gauges without reading `auth.json` or consuming reset credits.
- **Low-overhead refreshes** — Uses a persistent incremental cache and event-driven TUI updates to avoid repeatedly parsing unchanged rollouts.

## Installation

### Install a release binary

The installer supports macOS and Linux on x86_64 and ARM64. It verifies the release archive against `SHA256SUMS`; the default installation requires no `sudo` and uses `~/.local/bin`.

```bash
curl --proto '=https' --tlsv1.2 -fsSLO \
  https://github.com/ghostroller/codex-usage-monit/releases/latest/download/install.sh
sh install.sh
```

You can inspect `install.sh` before running it. The installer adds its directory to PATH for common POSIX shells when it can do so safely; otherwise it prints the exact PATH command to run manually.

Useful installer options:

```bash
# Install a specific release
sh install.sh --version vX.Y.Z

# Choose the destination
sh install.sh --install-dir "$HOME/bin"

# Leave shell profiles unchanged
sh install.sh --no-modify-path
```

To upgrade, download the latest installer again and rerun it, then restart any running TUI. The application does not provide a self-update function.

### Install from source

The repository pins Rust 1.97.0.

```bash
git clone https://github.com/ghostroller/codex-usage-monit.git
cd codex-usage-monit
cargo install --locked --path .
codex-usage-monit
```

For a repository-local build instead:

```bash
cargo build --locked --release
./target/release/codex-usage-monit
```

## Quick start

Start the interactive TUI:

```bash
codex-usage-monit
```

Print account limits and reset times:

```bash
codex-usage-monit limits
```

Export a compact JSON snapshot:

```bash
codex-usage-monit snapshot --format json --compact
```

Run on a remote development server with a pseudo-terminal:

```bash
ssh -t dev-server codex-usage-monit
```

For local history, run the monitor as the same Unix user that runs Codex. If the data belongs to another location, pass `--codex-home`:

```bash
codex-usage-monit --codex-home /path/to/.codex
```

The live account gauges require an installed, signed-in Codex CLI, whose App Server this program queries. Local rollout monitoring still works without Codex Desktop. If the App Server is unavailable or network access is intentionally disabled, use offline mode:

```bash
codex-usage-monit --offline snapshot --format json --compact
```

Offline quota data comes from the newest usable snapshot in local rollouts and is marked stale/partial when appropriate.

## Usage

Running `codex-usage-monit` without a subcommand starts the TUI. One-shot subcommands are intended for shell scripts and automation.

| Command | Purpose |
| --- | --- |
| `snapshot` | Print a complete snapshot or selected sections. |
| `limits` | Print account quota windows and reset credits. |
| `tasks` | Print recent tasks. |
| `turns` | Print turns, optionally for one thread. |
| `models` | Print model usage for the preferred current quota window. |
| `attribution` | Print quota-attribution and data-quality details. |
| `windows` | Print task, turn, and model usage for every current reset cycle. |
| `debug-startup` | Profile the normal TUI cold-start path without entering interactive mode. |

Every command above supports `--format text|json`. The data commands also support `--compact`, which writes JSON on one line instead of pretty-printing it; `debug-startup` instead provides `--width` and `--height` for its headless render.

```bash
# Only selected snapshot sections
codex-usage-monit snapshot \
  --section limits \
  --section tasks \
  --format json

# Turns from one Codex thread
codex-usage-monit turns \
  --thread 019abcde-0000-7000-8000-000000000000 \
  --format json

# Account limits in a shell pipeline
codex-usage-monit limits --format json | jq '.limits'

# Redact titles and omit message previews in output and new cache entries
codex-usage-monit --redact-content tasks --format json
```

`jq` is optional and is only used in the pipeline example above.

The valid `snapshot --section` values are `limits`, `tasks`, `turns`, `models`, `attribution`, `windows`, and `health`. The TUI calls its second top-level tab **Other**; `health` remains the one-shot snapshot section name.

### Common options

Global options should appear before the subcommand.

| Option | Meaning |
| --- | --- |
| `--codex-home <DIR>` | Read a custom Codex data directory instead of `$CODEX_HOME` or `~/.codex`. |
| `--days <N>` | Scan rollouts from the last N days; default: `7`. |
| `--max-files <N>` | Scan at most N rollout files; default: `500`. |
| `--active-grace-minutes <N>` | Freshness window used to infer active task state; default: `5`. |
| `--offline` | Do not query the App Server; use local fallback data. |
| `--redact-content` | Replace titles with `[redacted]`, omit message previews, and use a separate redacted cache. |
| `--no-rollout-cache` | Disable the persistent parsed-rollout cache. |
| `--theme dark|light` | Choose the TUI theme. `bright` is accepted as an alias for `light`. |
| `--startup-log <FILE>` | Write startup timing events as JSONL. |
| `--perf-log <FILE>` | Write runtime performance events as JSONL. |

Run `codex-usage-monit --help` or `codex-usage-monit <command> --help` for the complete option list.

## Interactive TUI

The **Overview** tab combines account limits with Tasks, Turns, and Models. Its weekly quota gauge also shows an expiry reminder when a fully known available Codex reset credit expires before the current server-defined weekly reset. The **Other** tab shows source health, collection statistics, diagnostics, quota windows, and reset-credit details returned by the App Server, including grant and expiry times.

The default scan covers the last 7 days and at most 500 rollout files. The TUI refreshes changing local rollouts incrementally and refreshes remote account state less frequently.

### Keyboard controls

| Keys | Action |
| --- | --- |
| `Tab` / `→`, `Shift+Tab` / `←` | Move between views. |
| `1`, `2` | Open Overview or Other. |
| `5`, `w` | Select the 5-hour or weekly reset cycle. |
| `↑` / `k`, `↓` / `j`, `Home`, `End`, `PgUp`, `PgDn` | Navigate lists. |
| `Enter`, `Backspace` | Open a task's turns or return to Tasks. |
| `/` or `f` | Filter the focused Tasks or Turns list. |
| `r`, `E`, `-`, `+` | Toggle flat/tree mode, collapse/expand all, or collapse/expand one parent. |
| `a`, `d`, `s`, `c`, `[` / `]` | Filter All, Desktop, Subagent, or CLI sources, or cycle source filters. |
| `v`, `m` | Show/hide Turns or Models. |
| `o` | Open the selected stopped root task in a new Zellij pane, or offer a resume command for other terminals. |
| `t` | Toggle dark/light theme. |
| `q` | Quit. `Esc` opens quit confirmation from the main view. |

Printable keys are consumed by a focused text field before global shortcuts. Mouse input is also available for controls, Tasks/Turns rows, tabs, and scrollbars.

## Understanding the fields

### Account and reset fields

| Field | Meaning |
| --- | --- |
| `5h` / `Week` | The current server-defined 5-hour or weekly reset cycle. Week is not a rolling seven-day period or necessarily a calendar week. |
| `USED` | Account quota consumed in the window, from the App Server gauge. |
| `LEFT` | `100 - USED`, clamped to 0–100%. |
| `ITEM` | A quota bucket `limitId` or a reset-credit title; reset credits fall back to `resetType` when no title is available. |
| `RESET TIME` on a quota window | The server-provided `resetsAt` time. |
| Reset-credit available count | The authoritative number of reset opportunities currently available. |
| `GRANTED` on a reset credit | When that reset opportunity was granted, if returned by the server. |
| `RESET TIME` on a reset credit | The credit's `expiresAt`; `never` means the server returned no expiry time. |
| `STATE` | The raw reset-credit status returned by the server. |
| `resetType` in JSON | The raw reset-credit type returned by the server. |

The service can return fewer detail rows than the available count. In that case, `DETAILS n/N` means the server supplied details for only `n` of `N` available credits; the count remains authoritative. `SHOWING n/N` means the terminal is currently too short to display all credit details already received, and `WINDOWS n/N` means it is too short to display all quota-window rows.

The Overview reminder is conservative: it uses only complete, current details whose status is `available` and whose `resetType` is `codexRateLimits`. It compares the earliest future expiry strictly before the ordinary `codex` weekly reset. Truncated, partial, or stale details do not produce a reminder.

Reset and turn timestamps in the TUI are shown in local time; Collection/Snapshot `asOf` timestamps remain UTC. One-shot text output uses UTC, and JSON uses RFC 3339 timestamps.

### Task, turn, and model fields

| Field | Meaning |
| --- | --- |
| `TOKENS` | Locally observed total tokens. In scoped TUI views, this is eligible non-Spark usage inside the selected ordinary `codex` reset cycle; in one-shot `tasks`/`turns` and their JSON `tokenUsage`, it covers the configured scan range. |
| `TOKEN5H%` / `TOKENWK%` / `TOKEN%` | The entity's share of locally observed, eligible non-Spark tokens in the selected ordinary `codex` cycle. This is a token share, not an account quota percentage. |
| `EST.Q5H` / `EST.QWK` / `EST.Q` | A low-confidence estimate of account quota percentage points attributed to the entity. `~` means approximate; `-` means unavailable. |
| `EFFORT` | The reasoning-effort value recorded by Codex. |
| `FAST` | The rollout used `serviceTier=priority`; attribution uses the corresponding Fast price weights. |
| `MESSAGE` | A short local preview of the turn message, up to 72 characters. |
| `SOURCE` | The recorded task source. TUI filters include All (no source filter), Desktop (including `vscode`), Subagent, and CLI. |

When a task tree is collapsed, a visible parent row includes the tokens/share of its hidden descendants.

### Status markers

| Marker | Meaning |
| --- | --- |
| `R RUN` | Running task or in-progress turn. For rollout-only tasks, recency is inferred; it does not prove an OS process is alive. |
| `W WAIT` | Groups waiting-for-approval and waiting-for-input states when such a state is available. |
| `D DONE` | Completed turn/task or an idle task. |
| `X STOP` | Interrupted. |
| `F FAIL` | Failed. |
| `? STALE` | Stale or unknown. |

Task status evidence and confidence are separate JSON fields. Task `statusProvenance` can be `live`, `server_snapshot`, `local_exact`, `inferred`, `estimated`, `stale`, or `unknown`; `statusConfidence` values are `high`, `medium`, `low`, or `unknown`. Turn records currently expose `status` without these two evidence fields.

Quota-attribution confidence uses the same enum for schema consistency, but the current estimator emits only `low` when it can calculate an estimate and `unknown` when it cannot. The TUI intentionally communicates this as `~` or `-` instead of adding a confidence column to every row.

## Accuracy and limitations

- **Tokens are local observations.** Task/turn/model counts are derived from monotonic counter deltas. They are exact within the scanned local data when the relevant logs are complete and counters have not reset ambiguously.
- **Account gauges are server data.** Current quota-window percentages and reset times come from the Codex App Server, or from a stale local fallback in offline/degraded mode.
- **Entity quota is always estimated.** Codex does not provide an official per-task or per-turn quota bill. `EST.Q*` projects the current ordinary `codex` gauge onto local model/service-tier price weights, so activity from other machines or clients can distort it.
- **Partial is still usable, not complete.** A short lookback, `--max-files`, unreadable/bad lines, counter resets, stale sources, or a missing cycle boundary can mark a snapshot/window `partial`. An estimate may still be displayed.
- **Attribution is bucket-specific.** All quota buckets are displayed, but task/turn/model attribution currently uses the ordinary `codex` bucket. Exact `gpt-5.3-codex-spark` usage is excluded from the local attribution denominator.
- **Finished does not mean billable-exact.** Settled tasks can have exact locally observed token totals, while their quota estimate remains low confidence.

See [Data capabilities and limits](docs/codex-data-capabilities.md) for formulas, pricing fallbacks, counter handling, and detailed partial-reason semantics.

## JSON output

JSON output uses camelCase and currently reports `"schemaVersion": 1`.

| Field | Meaning |
| --- | --- |
| `asOf` | Snapshot timestamp. |
| `partial` | The result is usable but one or more sources/cycles are incomplete or degraded. |
| `sources` | Source freshness, provenance, and collection details. |
| `limits` | All quota windows. |
| `rateLimitResetCredits` | Authoritative reset-credit count plus optional per-credit details. |
| `tasks`, `turns`, `models` | Local entity records and preferred-window compatibility fields. |
| `attribution` | Preferred-window attribution summary and data-quality details. |
| `windowAnalyses` | Independent task/turn/model attribution for every current reset cycle. |
| `accountUsage` | Lifetime, daily, longest-turn, and activity-streak summaries returned by the App Server when available. |
| `stats` | Scan and parser statistics. |
| `warnings`, `errors` | Warning and error diagnostics from collection. A source error does not necessarily make the whole snapshot unusable. |

Token usage contains `inputTokens`, `cachedInputTokens`, `outputTokens`, `reasoningOutputTokens`, and `totalTokens`. Do not sum all five: cached input is a subset of input, reasoning output is part of output, and `totalTokens` is already the total.

For reset credits, `availableCount` is authoritative. `credits: null` means only the count is known; `credits: []` means details were fetched and the returned list was empty. A non-empty list can still be shorter than `availableCount` if the service truncated details.

The legacy per-entity attribution fields project the preferred 5-hour window for compatibility. Use `windowAnalyses[]` when you need both 5-hour and weekly cycle data.

### Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Complete result. |
| `1` | No valid result could be produced. |
| `2` | Usable partial result. |
| `64` | Command-line usage error. |

## Privacy and local data

The monitor reads Codex rollouts under `sessions` and `archived_sessions`, reads `session_index.jsonl`, and queries the local App Server. It does not read `auth.json`, and monitoring never consumes a reset credit.

Opening a task is an explicit action: `o` can launch `codex resume` in Zellij, while choosing Copy only writes the resume command to the terminal clipboard. See [Terminal resume behavior](docs/codex-terminal-resume.md) for details.

The persistent cache can contain limited task titles and message previews. Use `--redact-content` to replace titles with `[redacted]`, omit message previews, and write new data to a separate redacted cache, or use `--no-rollout-cache` to disable the parsed cache. Redacted mode does not delete a cache created by an earlier non-redacted run.

Default cache locations:

- macOS: `~/Library/Caches/codex-usage-monit`
- Linux: `$XDG_CACHE_HOME/codex-usage-monit`, or `~/.cache/codex-usage-monit` when `XDG_CACHE_HOME` is unset

Set `CODEX_USAGE_MONIT_CACHE_DIR` to override the cache directory.

## Troubleshooting and diagnostics

### `codex-usage-monit: command not found`

Open a new shell, or follow the PATH command printed by the installer. With the default destination:

```bash
export PATH="$HOME/.local/bin:$PATH"
```

### Missing or stale account limits

Confirm that `codex` is installed and signed in for the same user. Use `--offline` when only local rollout data is available. Offline/stale limits deliberately produce a partial result.

### Incomplete weekly totals

Increase the scan range and file cap so they cover the complete server reset cycle:

```bash
codex-usage-monit --days 14 --max-files 2000
```

### Startup or runtime performance

```bash
codex-usage-monit debug-startup
codex-usage-monit --startup-log /tmp/codex-usage-startup.jsonl
codex-usage-monit --perf-log /tmp/codex-usage-perf.jsonl
```

The first run, a parser-version change, or a disabled cache can be slower because rollouts must be parsed from scratch.

## Documentation

- [Data capabilities and limits](docs/codex-data-capabilities.md)
- [Terminal resume behavior](docs/codex-terminal-resume.md)
- [TUI integration testing](docs/tui-integration-testing.md)
- [Changelog](CHANGELOG.md)

## License

[MIT](LICENSE)
