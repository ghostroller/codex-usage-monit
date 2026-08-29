# codex-usage-monit

**English** · [简体中文](README.zh-CN.md)

[![CI](https://github.com/ghostroller/codex-usage-monit/actions/workflows/ci.yml/badge.svg)](https://github.com/ghostroller/codex-usage-monit/actions/workflows/ci.yml)
[![Latest release](https://img.shields.io/github/v/release/ghostroller/codex-usage-monit)](https://github.com/ghostroller/codex-usage-monit/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**Codex usage monitoring, entirely in your terminal.**

`codex-usage-monit` tracks Codex quota windows, reset times, reset credits, tasks, turns, models, locally observed token usage, API-equivalent model-call cost, and usage history. Run it as an interactive TUI, or use its non-interactive CLI to produce plain text or JSON for scripts, cron jobs, and CI.

It is local-first and terminal-native: no desktop application, browser, database, or listening port is required. The TUI works as a standalone process; an optional per-user background recorder can keep quota history continuous while the TUI is closed. Prebuilt binaries run on Windows, macOS, and Linux, including headless development servers over SSH.

## TUI preview

[![Codex usage monitor TUI overview in a 120 by 40 dark terminal](docs/assets/tui/overview-dark-120x40.svg)](docs/assets/tui/overview-dark-120x40.svg)

_Deterministically rendered from the integration-test fixture. The synchronization check in CI prevents this preview from drifting from the current TUI._

## Highlights

- **Account usage at a glance** — See used/remaining percentages, all available quota buckets, and server-reported reset times.
- **Reset-credit details** — See the authoritative available count plus grant and expiry times when the server returns per-credit details.
- **Expiry reminder** — The weekly Overview gauge warns when the earliest fully known available reset credit expires before the ordinary Codex weekly reset, with the exact local expiry time.
- **Local usage breakdown** — Explore tasks, turns, models, token totals, and token share for the current 5-hour or weekly reset cycle.
- **API-equivalent model cost** — Value locally observed model tokens at current API rates with exact fixed-point math, long-context ranges, and explicit priced coverage; non-model tool charges are excluded.
- **Usage trends** — Record server remaining quota, weekly local tokens, low-confidence weekly estimates, and 15-minute token/estimate buckets in local state.
- **Project usage summary** — Rank projects for the current cycle, last 7 days, or last 30 days; expand clearly labeled project/session/subagent trees and compare token, estimated credit-rate, or API-equivalent usage with ranked bars and a project-colored stacked-area chart from 1-day down to 1-hour buckets.
- **Optional background recorder** — Keep collecting while the TUI is closed with launchd, systemd user services, or Windows Task Scheduler; no administrator account is required.
- **Interactive terminal UI** — Filter, search, switch scopes, expand task trees, inspect turns/models, and resume tasks without leaving the terminal.
- **Scriptable CLI** — Export human-readable text or schema-versioned camelCase JSON, select individual sections, and filter turns by thread.
- **Server-friendly** — Works in SSH, tmux, and Zellij sessions; Linux release binaries are static musl builds with no host glibc dependency.
- **Read-only monitoring** — Reads local Codex data and account gauges without reading `auth.json` or consuming reset credits.
- **Low-overhead refreshes** — Uses a persistent incremental cache and event-driven TUI updates to avoid repeatedly parsing unchanged rollouts.

## Installation

### Install a release binary

The shell installer supports macOS and Linux on x86_64 and ARM64. It verifies the release archive against `SHA256SUMS`; the default installation requires no `sudo` and uses `~/.local/bin`.

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

To upgrade, download the latest installer again and rerun it, then restart any running TUI. If the background recorder is installed, also run `codex-usage-monit service install` again after replacing the executable; this restarts the resident process on the new version. The application does not provide a self-update function.

On 64-bit Windows, download `codex-usage-monit-x86_64-pc-windows-msvc.exe` and `SHA256SUMS` from the [latest release](https://github.com/ghostroller/codex-usage-monit/releases/latest). Verify the executable in PowerShell, rename it if desired, and place it in a directory on `PATH`:

```powershell
$binary = "codex-usage-monit-x86_64-pc-windows-msvc.exe"
$actual = (Get-FileHash $binary -Algorithm SHA256).Hash.ToLowerInvariant()
$expected = ((Select-String -Path SHA256SUMS -Pattern " $([regex]::Escape($binary))$").Line -split "\s+")[0]
if ($actual -ne $expected) { throw "checksum mismatch" }
Move-Item $binary codex-usage-monit.exe
.\codex-usage-monit.exe --version
```

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

On Windows, the repository-local binary is `.\target\release\codex-usage-monit.exe`.

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
| `record` | Continuously record local and account history without opening the TUI. |
| `service` | Install, inspect, or remove the optional per-user recorder. |
| `debug-startup` | Profile the normal TUI cold-start path without entering interactive mode. |

The one-shot data commands support `--format text|json` and `--compact`, which writes JSON on one line instead of pretty-printing it. `debug-startup` instead provides `--width` and `--height` for its headless render.

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

The valid `snapshot --section` values are `limits`, `tasks`, `turns`, `models`, `attribution`, `windows`, and `health`. The TUI's top-level tabs are **Overview**, **Trends**, **Summary**, **Other**, and **Settings**; `health` remains the one-shot snapshot section name.

### Continuous history recording

The TUI records history while it is open. Local token buckets can usually be reconstructed from rollout files after a restart, but server quota gauges cannot be recovered retroactively. To keep the remaining-quota line continuous while the TUI is closed, install the optional user-level recorder:

```bash
codex-usage-monit service install
codex-usage-monit service status
```

The installer uses a LaunchAgent on macOS, a `systemd --user` unit on Linux, and a least-privilege current-user Task Scheduler task on Windows. For online recording it registers absolute paths for both the running monitor and the Codex executable, preserves the install-time collection options and `PATH`, and starts `record --foreground`. The application does not daemonize itself. Use `--codex-bin <FILE>` before `service install` to override automatic Codex discovery; an offline recorder does not require Codex. The Windows task is isolated by user SID, has no 72-hour execution limit, may run on battery power, and restarts after failures. Registration is always explicit and can be removed without deleting history:

```bash
codex-usage-monit service uninstall
```

Run `service install` again after moving or replacing either executable, changing `--codex-home`, or changing collection options. A LaunchAgent belongs to the logged-in macOS GUI user. A systemd user unit normally follows the user's login session unless lingering is enabled. The Windows task uses an interactive user token and therefore runs while that user is logged in. On a headless host whose user session does not persist, enable the platform's supported user-service persistence or use an existing supervisor.

On systems without a supported service manager, run the recorder under tmux, Zellij, or another supervisor:

```bash
codex-usage-monit record --foreground
```

### Common options

Global options should appear before the subcommand.

| Option | Meaning |
| --- | --- |
| `--codex-home <DIR>` | Read a custom Codex data directory instead of `$CODEX_HOME` or `~/.codex`. |
| `--codex-bin <FILE>` | Use a specific Codex executable for App Server collection; service installation pins its resolved absolute path. |
| `--days <N>` | Scan rollouts from the last N days; default: `7`. |
| `--max-files <N>` | Scan at most N rollout files; default: `500`. |
| `--active-grace-minutes <N>` | Freshness window used to infer active task state; default: `5`. |
| `--offline` | Do not query the App Server; use local fallback data. |
| `--redact-content` | Replace titles with `[redacted]`, omit message previews, and use separate redacted cache and history namespaces. |
| `--no-rollout-cache` | Disable the persistent parsed-rollout cache. |
| `--theme dark|light` | Choose the TUI theme. `bright` is accepted as an alias for `light`. |
| `--startup-log <FILE>` | Write startup timing events as JSONL. |
| `--perf-log <FILE>` | Write runtime performance events as JSONL. |

Run `codex-usage-monit --help` or `codex-usage-monit <command> --help` for the complete option list.

## Interactive TUI

The **Overview** tab combines account limits with Tasks, Turns, Models, and the local token-only API-equivalent cost of model calls. Its weekly quota gauge also shows an expiry reminder when a fully known available Codex reset credit expires before the current server-defined weekly reset. **Trends** shows remaining quota, weekly local token and estimate trajectories, and 15-minute bars. **Summary** ranks project usage for the current cycle, 7 days, or 30 days and provides a collapsed project/session/subagent tree, top-project bars, and a non-cumulative stacked-area chart colored by project. The wide layout keeps the tree and ranking side by side above a full-width chart. Cycle the chart through local `1d`, `12h`, `6h`, `3h`, and `1h` buckets; it defaults to the top six projects plus `Other`, or can show all projects. Click or drag across an uncompressed plot to inspect an exact time bucket, or use Inspect with keyboard navigation. Exact inspection is disabled when a long fine-grained series must be compressed to fit the terminal. Each project's stable color is reused for its tree swatch, ranking bar, legend marker, and chart area. The chart labels complete (`C`), partial (`P`), and missing (`M`) buckets; missing project-level history is left as a gap instead of being plotted as zero. **Other** shows source health, collection statistics, diagnostics, quota windows, reset-credit details, and recorder health. **Settings** provides one place to manage display preferences and the metric columns shared by all tables.

The default scan covers the last 7 days and at most 500 rollout files. The TUI refreshes changing local rollouts incrementally and refreshes remote account state less frequently.

### Keyboard controls

| Keys | Action |
| --- | --- |
| `Tab` / `→`, `Shift+Tab` / `←` | Move between views. |
| `1`, `2`, `u`, `3`, `4` | Open Overview, Trends, Summary, Other, or Settings. |
| `c`, `7`, `m` on Summary | Select the current cycle, last 7 days, or last 30 days. |
| `K`, `e`, `a` on Summary | Rank and chart Tokens, estimated credit-rate equivalents, or API-equivalent cost. |
| `b` on Summary | Cycle the chart bucket size through `1d`, `12h`, `6h`, `3h`, and `1h`. |
| `g` on Summary | Toggle the stacked chart between Top 6 + Other and all projects. |
| `i` on Summary | Toggle exact time-bucket inspection when the series fits the plot; use `Left` / `Right`, `[` / `]`, `Home`, or `End` to navigate while active. |
| `Enter` / `Space`, `+` / `-`, `x`, `l` on Summary | Toggle, expand/collapse the selected tree node, collapse the whole tree, or toggle the optional Longx estimate. |
| `r`, `w`, `h` on compact Trends | Show Remaining, Weekly, or 15-minute charts. |
| `[`, `]`, `n` on Trends | Move the 24-hour chart window backward/forward, or return to Now. |
| `i` on Trends | Toggle Inspect mode. |
| `←` / `→`, `Home`, `End` in Inspect mode | Select the previous/next, first, or last recorded point. |
| `↑` / `↓` in Inspect mode | Move between the visible charts. |
| `5`, `w` | Select the 5-hour or weekly reset cycle. |
| `↑` / `k`, `↓` / `j`, `Home`, `End`, `PgUp`, `PgDn` | Navigate lists. |
| `Enter`, `Backspace` | Open a task's turns or return to Tasks. |
| `/` or `f` | Filter the focused Tasks or Turns list. |
| `r`, `E`, `-`, `+` | Toggle flat/tree mode, collapse/expand all, or collapse/expand one parent. |
| `a`, `d`, `s`, `c`, `[` / `]` | Filter All, Desktop, Subagent, or CLI sources, or cycle source filters. |
| `v`, `m` | Show/hide Turns or Models. |
| `l` on Overview | Toggle the optional long-context multiplier for TUI quota `~EST` values (`EST Longx`). |
| `↑` / `↓`, `Enter`, or the highlighted letter on Settings | Select or toggle a display/column preference. |
| `o` | Open the selected stopped root task in a new Zellij pane, or offer a resume command for other terminals. |
| `t` | Toggle dark/light theme. |
| `q` | Quit. `Esc` opens quit confirmation from the main view. |

Printable keys are consumed by a focused text field before global shortcuts. Mouse input is also available for controls, Tasks/Turns rows, tabs, and scrollbars. On Trends, click a chart to inspect its nearest recorded point, or hold the left mouse button and drag to scrub across points.

The `4 Settings` page has two groups. **Display** controls the theme, Turns and Models panel visibility, and `EST Longx`. **Table columns** globally control Tokens, Token share, Estimated quota, and API equivalent for the Tasks, Turns, and Models tables. Settings are persisted across TUI runs. The task/model identity and turn model/effort/message columns stay fixed; if a panel is too narrow, the layout may temporarily omit lower-priority enabled metric columns without changing the saved choices.

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
| `API EQ.` / `API.EQ5H` | Local model tokens valued at the bundled OpenAI API rates. Each Tasks and Turns row in the TUI has its own value for the selected 5-hour or weekly scope; it is not a lifetime total, shows `-` when that window is unavailable, and is independent of `EST Longx`. One-shot task/turn rows use the current 5-hour reset cycle. A range means request boundaries could be short or long context; trailing `+` marks a lower bound because unpriced samples or incomplete local rollout coverage may hide additional cost; `-` also means no usable local data or no applicable public price. |
| `~EST CR.` on Summary | Additive Codex credit-rate equivalent for the selected arbitrary time range, normalized from estimator weight units and shown in credits. It is useful for relative comparison, not an account quota percentage or subscription invoice. |
| `EFFORT` | The reasoning-effort value recorded by Codex. |
| `FAST` | The rollout used a recognized Fast tier (`serviceTier=fast` or the compatible `priority` value); attribution applies the published Fast credit multiplier. |
| `MESSAGE` | A short local preview of the turn message, up to 72 characters. |
| `SOURCE` | The recorded task source. TUI filters include All (no source filter), Desktop (including `vscode`), Subagent, and CLI. |

The estimator follows OpenAI's current [token-based Codex rate card](https://learn.chatgpt.com/docs/pricing). Following the August 21, 2026 adjustment, `gpt-5.6` (the Sol alias), `gpt-5.6-sol`, and Daybreak Blue's `daybreak-blue-latest` alias use `(100, 10, 500)` input/cached-input/output credits per 1M tokens; OpenAI documents this Sol promotional pricing as available at least through November 21, 2026. Daybreak Red's current `daybreak-red-latest` alias and `gpt-5.6-cyber` ID use `(312.5, 31.25, 1875)`, while the legacy `gpt-5.5-cyber` slug remains mapped to that row for historical rollouts. The current card also supplies the GPT-5.6 Terra/Luna, GPT-5.5, GPT-5.4, and GPT-5.4 mini rows. GPT-5.3-Codex, GPT-5.2, and the historical `gpt-5.2-codex` slug retain earlier compatibility weights but are not presented as current official card rows.

For recognized ChatGPT Fast calls the estimator applies the published [Speed](https://learn.chatgpt.com/docs/agent-configuration/speed) multiplier: `2.5x` for GPT-5.6/GPT-5.5 and `2x` for GPT-5.4. The compatible `serviceTier=priority` value found in local signed-in rollouts is treated as Fast for this attribution; this is not API Priority billing, which the official Speed page describes separately. Exact `gpt-5.3-codex-spark` calls remain outside this attribution because its credit rate is still a research preview; an unlisted or missing non-Spark model uses the corresponding GPT-5.6 Luna fallback and marks the scope partial.

The TUI's `[L]EST Longx` switch is **off by default**. When enabled, it additionally applies OpenAI's API-published long-context rule to supported models for the Codex quota `~EST` projection. OpenAI's Codex subscription credit card says context affects credits but does not publish this same per-request formula, so the switch is an optional proxy assumption, not subscription billing fact. Its setting is saved with the other TUI preferences; the recorder always stores both base and optional weights, so changing it does not require reinstalling the background service.

`API EQ.` is a separate calculation based on the current [OpenAI API pricing table](https://developers.openai.com/api/docs/pricing). It prices each locally observed model request using regular input, cached input, cache-write, and output rates; reasoning tokens are already part of output and are not added twice. The calculation applies a published API long-context price when an exact request exceeds 272K input tokens; models with one flat price keep it across their supported context. If a larger cumulative delta has unknown request boundaries, the UI shows a short/long cost range. For current structured plain ThreadSpawn subagents, the parser can recover the effective tier from provenance-gated settings snapshots only when spawn metadata explicitly records no custom role and the child model exactly matches; a newer snapshot always supersedes older state, and only here does an absent or null tier mean API `default`. `codex-auto-review` is an explicit application pricing proxy: it uses `gpt-5.6-luna` rates, honors a recorded Standard/Fast tier, treats a missing tier as Standard, keeps the observed `codex-auto-review` label, and reports `api_price_codex_auto_review_luna_proxy`. This routing assumption is not an official API model alias. Legacy or custom-role spawns, model mismatches, other missing or unknown tiers, unknown models, unavailable Fast/long rows, missing token breakdowns, and cache writes without a published rate reduce the displayed priced coverage instead of using a fallback. Per-call tool and other non-model charges are not included; model input/output tokens surrounding tool execution are still valued at model rates. This is an equivalent value at current API rates, not an API invoice or a Codex subscription charge.

GPT-Image-2.0 is not assigned one of its published rows: the official card separates image and text billing, while rollout usage does not expose enough modality information to choose reliably. If such a model name appears in an ordinary token call, it uses the partial-marked unknown-model fallback instead of pretending the image rate is exact.

Task trees start fully collapsed. A visible parent row includes the tokens, shares, estimates, and API-equivalent costs of its hidden descendants; expanding it restores the independent per-session rows.

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

### Trend fields

| Chart | Meaning |
| --- | --- |
| `Quota Remaining` | Persisted server observations of `100 - usedPercent`. Five-hour and weekly reset cycles are separate series; gaps while no recorder was running are not interpolated. |
| `Weekly Local Tokens` | A cumulative sum of eligible local token deltas inside the current server-defined weekly cycle. |
| `Weekly ~EST Usage` | The latest weekly server gauge distributed across local Codex credit-rate-weighted activity up to each point. It is a low-confidence allocation, not an independent server measurement. |
| `15m Local Tokens` | Local token deltas whose observed completion timestamps fall in UTC-aligned 15-minute buckets. |
| `15m ~EST Usage` | The same weekly low-confidence allocation split across those 15-minute credit-rate-weight buckets. |

History is stored in UTC and displayed in local time. Weekly cumulative samples use original call timestamps, so an arbitrary server reset minute is cut exactly. Summary's project breakdown is recorded prospectively and its `1h` through `1d` chart buckets are derived from the same persisted 15-minute observations; switching chart grain does not rescan rollout files or require another recorder mode. The first time the 30-day range is selected with incomplete project history, the TUI performs one local-only background scan with a 31-day lookback and an expanded file limit; the normal recorder remains on its lightweight configured lookback. A namespace-scoped marker prevents a partial scan from repeating on every launch, while incomplete coverage becomes eligible for another automatic attempt after seven days. Buckets that cannot be reconstructed remain `PARTIAL`, totals are labeled as the known lower bound, and unknown time buckets are gaps rather than zero. EST aggregates carry an estimator revision so incompatible weighting definitions are not silently mixed. The current dual-weight mapping is estimator revision 5: every new local observation stores the base Codex credit proxy and the optional API long-context extra together. With `[L]EST Longx` off, an unverifiable large aggregate does not affect completeness; with it on, the aggregate keeps its base rate and reports `long_context_usage_unknown` instead of guessing. Released revision-3 base history is preserved but cannot supply the optional multiplier until rebuilt; the briefly used development-only revision-4 single-weight history is discarded because its base and extra cannot be separated safely. Mixed estimator revisions still cannot be combined. Because the latest weekly gauge and full-cycle denominator are used, previously drawn `~EST` bars may be revised when new local calls, a new server sample, the toggle, or an estimator update changes the selected projection. A `15m ~EST` bar that straddles a weekly reset is excluded and marked partial rather than mixed across cycles.

Trends Inspect shows each selected observation's exact stored timestamp and value rather than reconstructing it from chart coordinates. For a 15-minute Trends bar, the readout shows the precise UTC-aligned bucket interval in local time. Summary Inspect instead shows the selected derived local aggregation bucket's start, interval, and value for the active `1h` through `1d` grain; it is not an original event timestamp.

## Accuracy and limitations

- **Tokens are local observations.** Task/turn/model counts are derived from monotonic counter deltas. They are exact within the scanned local data when the relevant logs are complete and counters have not reset ambiguously.
- **Account gauges are server data.** Current quota-window percentages and reset times come from the Codex App Server, or from a stale local fallback in offline/degraded mode.
- **Entity quota is always estimated.** Codex does not provide an official per-task or per-turn quota bill. `EST.Q*` projects the current ordinary `codex` gauge onto local model/service-tier Codex credit-rate weights. The optional API long-context multiplier is disabled by default; either projection can still be distorted by activity from other machines or clients.
- **API equivalent is token-only and local.** It values locally observed model calls at the bundled current API prices. It excludes per-call tool fees, container, storage, search-call, tax, regional, and negotiated-contract charges, and does not yet include usage from other machines.
- **Workspace rate-card migration matters.** The token-based card applies to most plans, but OpenAI says a small subset of Enterprise workspaces remains on the legacy per-message card. The monitor cannot infer that workspace migration state from local rollouts, so those users should treat `~EST` as not representative of their applicable billing card.
- **Partial is still usable, not complete.** A short lookback, `--max-files`, unreadable/bad lines, counter resets, stale sources, or a missing cycle boundary can mark a snapshot/window `partial`. An estimate may still be displayed.
- **Attribution is bucket-specific.** All quota buckets are displayed, but task/turn/model attribution currently uses the ordinary `codex` bucket. Exact `gpt-5.3-codex-spark` usage is excluded from the local attribution denominator.
- **Finished does not mean billable-exact.** Settled tasks can have exact locally observed token totals, while their quota estimate remains low confidence.
- **History distinguishes zero from missing.** A program outage creates a gap in server quota history. Local 15-minute buckets can be backfilled only while their rollout files remain inside the configured scan range and file cap. On upgrade, legacy 30-minute local buckets are discarded instead of being split approximately; quota and weekly cumulative history are retained, and recent local buckets are rebuilt from rollout files still in range.

See [Data capabilities and limits](docs/codex-data-capabilities.md) for formulas, rate-card fallbacks, counter handling, and detailed partial-reason semantics.

## JSON output

JSON output uses camelCase and currently reports `"schemaVersion": 2`.

| Field | Meaning |
| --- | --- |
| `asOf` | Snapshot timestamp. |
| `apiPricing` | Version, rates-as-of date, official source URL, and the `current_api_rates_model_tokens_only` basis of the bundled API price catalog. |
| `apiEquivalentCost` | Preferred current 5-hour token-only total, coverage, and partial reasons; omitted when no current 5-hour analysis exists. |
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

Token usage contains `inputTokens`, `cachedInputTokens`, `cacheWriteInputTokens`, `outputTokens`, `reasoningOutputTokens`, and `totalTokens`. Do not sum them: cached input and cache write input are subsets of input, reasoning output is part of output, and `totalTokens` is already the total.

`apiEquivalentCost` reports `minimumPicoUsd` and `maximumPicoUsd` as exact decimal strings, plus observed/priced rollout usage-sample and token counts. One non-exact sample can represent multiple requests, so `observedSamples` and `pricedSamples` are not request counts. `modelBreakdown` includes every observed model, including unpriced models excluded from the quota estimator. The top-level value and task/turn/model projections use the preferred current 5-hour window; `windowAnalyses[]` also exposes weekly values. A thread-filtered Turns response omits the all-thread top-level total while retaining per-turn costs. The amounts cover only priced model calls; inspect `partialReasons` and coverage before treating the subtotal as complete. Equal minimum and maximum values represent a single price, while differing values represent an unresolved short/long-context request-boundary range.

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
- Windows: `%LOCALAPPDATA%\codex-usage-monit\cache`

Set `CODEX_USAGE_MONIT_CACHE_DIR` to override the cache directory.

History and recorder state are user data rather than a rebuildable parse cache. They use:

- macOS: `~/Library/Application Support/codex-usage-monit`
- Linux: `$XDG_STATE_HOME/codex-usage-monit`, or `~/.local/state/codex-usage-monit`
- Windows: `%LOCALAPPDATA%\codex-usage-monit`

Set `CODEX_USAGE_MONIT_STATE_DIR` to override the state directory. History is retained for 90 days in namespaced UTC-day JSON shards. `--no-rollout-cache` does not disable history, and uninstalling the background service does not delete it.

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
Runtime logs include collection refreshes, history record/load timings and shard counts, draw aggregates, and periodic CPU/memory/I/O samples without session content.

## Documentation

- [Data capabilities and limits](docs/codex-data-capabilities.md)
- [Terminal resume behavior](docs/codex-terminal-resume.md)
- [TUI integration testing](docs/tui-integration-testing.md)
- [Changelog](CHANGELOG.md)

## License

[MIT](LICENSE)
