# Changelog

All notable changes to this project are documented in this file.

## Unreleased

### Added

- Added token-only API-equivalent model-call costs for the current 5-hour and weekly Codex reset cycles. The calculation uses a versioned OpenAI API price catalog, preserves Standard/Fast and short/long-context request boundaries, prices regular input, cached input, cache writes, and output separately, and reports unpriced coverage instead of applying a fallback to unknown models or tiers.
- Added exact fixed-point pico-USD values to snapshot JSON and API-equivalent totals/ranges to the Models summary, per-model rows, task/turn window output, and selected-turn detail. Totals include per-model coverage and distinguish rollout usage samples from request counts. Tool-call, container, storage, search-call, tax, regional, and contract-specific charges are intentionally excluded.

### Changed

- Clarified the Overview long-context control as `[L]EST Long×`: it changes only the optional Codex quota estimate projection. API-equivalent cost always follows the API pricing catalog and is unaffected by this display preference.

### Compatibility

- Bumped snapshot JSON to schema version 2 and added `apiPricing`, per-window `apiEquivalentCost`, and per-entity fixed-point cost fields. Pico-USD values serialize as decimal strings so large totals remain exact in JavaScript clients.

## [0.2.9] - 2026-08-27

### Changed

- Added an opt-in `[L]Long×` TUI switch for API-style long-context weighting. It is off by default because the Codex subscription credit card does not publish the API's exact per-request formula. When enabled, verified requests with more than 272K input tokens use the API-published 2x input/cached-input and 1.5x output multipliers for supported model profiles; unverifiable large cumulative deltas retain the base rate and report `long_context_usage_unknown` instead of being guessed.
- Recorder history now stores the base Codex credit proxy and the optional API long-context extra together. Overview attribution and Trends `~EST` charts switch between those paired projections immediately, without recollecting data or changing the recorder service configuration. The saved TUI preference defaults to off on first use.
- Preserved `cache_write_input_tokens` from rollout counters as an input subset for exact deltas and request-boundary checks. The Codex credit proxy does not add a separate cache-write charge because the published Codex credit card has no cache-write row.

### Fixed

- On Windows, automatic Codex CLI discovery now skips the non-launchable Desktop packaged resource, prefers a runnable CLI from `PATH` or the installed standalone fallback, and reports an actionable error when neither is available.
- Prioritized the ordinary `codex` 5h and weekly quota gauges ahead of auxiliary rate-limit buckets while keeping reset-credit reminder placement aligned with the reordered gauges.

### Internal

- Normalized Windows executable-path assertions so release verification does not depend on verbatim temporary-path formatting.

### Compatibility

- Bumped the rollout parser cache revision to 4, the estimator revision to 5, the history metric revision to 3, and the TUI state version to 3. Released estimator-revision-3 base history is retained but cannot provide the optional multiplier until rebuilt; development-only revision-4 single-projection history is discarded because its base and API extra cannot be separated safely. Other incompatible revisions remain isolated, and mixed-revision `~EST` stays unavailable/partial.
- Existing recorder-service processes should be restarted after upgrading so resident writers use parser revision 4 and the dual-projection history schema. Switching `[L]Long×` afterward does not require reinstalling or restarting the service.

## [0.2.8] - 2026-08-26

### Changed

- Aligned Codex credit weighting with OpenAI's [August 21, 2026 token-based rate card](https://learn.chatgpt.com/docs/pricing): GPT-5.6 Sol and Daybreak Blue now use 100/10/500 input/cached-input/output credits per 1M tokens, while the unchanged Terra, Luna, GPT-5.5, GPT-5.4, GPT-5.4 mini, and Daybreak Red rows retain their published rates. Sol's promotional pricing is currently documented as available at least through November 21, 2026.
- Added the current `daybreak-blue-latest` and `daybreak-red-latest` aliases plus the `gpt-5.6-cyber` model ID. The legacy `gpt-5.5-cyber` slug remains mapped to the Daybreak Red row, and GPT-5.3-Codex/GPT-5.2 mappings remain only for historical rollout compatibility rather than being described as current official rate-card rows.

### Fixed

- Restored Windows account-quota collection by preferring the installed standalone Codex CLI over the non-launchable Desktop packaged resource, using the current default stdio App Server transport, and allowing additional CLI cold-start time.
- Treated an unavailable optional `account/usage/read` RPC as protocol-compatible, bounded external diagnostics, and kept a stable actionable App Server/CLI warning between the session panels and Models instead of letting transient refresh state consume the session area.

### Compatibility

- Bumped the estimator revision to 3 for the updated Sol weight and Daybreak mappings. Rebuildable points inside the configured rollout scan range may replace older revisions when their unweighted evidence is no worse; unrebuildable revision-1/2 points remain stored, and mixed-revision windows keep `~EST` unavailable and partial instead of combining incompatible weights.

## [0.2.7] - 2026-08-18

### Fixed

- Restored turn message previews for Codex rollout files that emit user prompts as `response_item` records, while excluding injected AGENTS, environment, internal-context, and plugin envelopes.
- Counted exact first samples after resumed rollout token-counter epochs when `total_token_usage` and `last_token_usage` prove the reset boundary, instead of reporting and dropping them as ambiguous.

## [0.2.6] - 2026-08-10

### Changed

- Deferred the Codex App Server account refresh until after the TUI's first frame, while keeping the initial local rollout snapshot and history load synchronous.
- Added bounded 5-second and 10-second retries for missing, partial, stale, or failed reset-credit refreshes before returning to the normal 45-second account interval.

### Fixed

- Preserved recently fetched reset-credit details across transient omitted or `null` responses for at most five minutes, without extending their original observation time or retaining expired details.
- Avoided persisting stale online quota-window samples while the deferred account refresh is incomplete; offline history collection continues to retain its local fallback observations.

## [0.2.5] - 2026-08-10

### Changed

- Replaced the API-dollar short-context weighting proxy with OpenAI's token-based Codex credit rate card for the `gpt-5.6` Sol alias, GPT-5.6 Sol/Terra/Luna, GPT-5.5, GPT-5.5 Cyber, GPT-5.4, GPT-5.4 mini, GPT-5.3-Codex, GPT-5.2, and the historical `gpt-5.2-codex` slug.
- Applied the published Fast credit multipliers to both `serviceTier=fast` and `serviceTier=priority`, retained GPT-5.6 Luna as the explicit fallback for unknown non-Spark models, and continued excluding GPT-5.3-Codex-Spark while its rate remains a research preview.
- Renamed user-facing estimate explanations from price-weighted to Codex credit-rate-weighted; `~EST` remains a low-confidence allocation of the account gauge rather than server per-task accounting.

### Compatibility

- Bumped the estimator revision to 2. Overlapping local buckets and weekly points are rebuilt from rollout calls still in the configured scan range, and revision-aware upsert replaces older points when unweighted evidence is no worse. Older points that cannot be rebuilt remain isolated; mixed-revision windows keep `~EST` unavailable and partial instead of silently combining mappings.
- Changed the attribution method identifier from `current_codex_gauge_short_context_price_weighted_proxy` to `current_codex_gauge_credit_rate_weighted_proxy`.
- A small subset of Enterprise workspaces still uses OpenAI's legacy per-message rate card. The monitor cannot detect that migration state from local rollout data, so revision-2 estimates do not represent the applicable legacy card for those workspaces.

## [0.2.4] - 2026-07-31

### Added

- Added an interactive Trends inspector with keyboard navigation and mouse click/drag selection. Readouts preserve exact sampled values and timestamps, and 15-minute bars show their precise bucket interval.

### Changed

- Increased local token and `~EST` bar resolution from 30-minute to UTC-aligned 15-minute buckets while keeping weekly cumulative sampling at 30 minutes.
- Added exact as-of values to Quota and Weekly line charts while leaving point-in-time values off the 15-minute bar charts.

### Compatibility

- Bumped history shards to format and metric revision 2. Legacy quota and weekly cumulative history are retained, while indivisible 30-minute local buckets are discarded and recent 15-minute buckets are rebuilt from rollout files still in the configured scan range.
- Bumped performance-log schema to version 4 and renamed the history volume field from `halfHourBuckets` to `localBuckets`.
- Existing recorder-service installations must be reinstalled after upgrading so the resident process restarts with history format 2 support.

## [0.2.3] - 2026-07-29

### Added

- Added native 64-bit Windows CI and direct GitHub Release distribution as a checksummed `x86_64-pc-windows-msvc` executable.
- Added PowerShell-safe copied resume commands and Windows `codex.cmd`/`PATHEXT` command discovery.
- Added persistent 90-day quota, local-token, and estimated-usage history with weekly trajectories and 30-minute bars in the new Trends view.
- Added an optional foreground recorder and per-user launchd, systemd, or Windows Task Scheduler service so quota history can continue while the TUI is closed.

### Changed

- Collapsed every task-tree node by default while preserving explicit expansion choices across refreshes.
- Reduced steady-state refresh overhead by caching rollout discovery, batching TUI history writes, retaining live staged data, and expanding performance diagnostics.

### Fixed

- Used `%USERPROFILE%\.codex` when Windows does not provide `HOME`, and made persisted TUI state replacement work on Windows.
- Kept the latest Trends window and recorder-health age tied to wall-clock time, and included both weekly cycles when a 30-minute estimate view crosses a reset.
- Preserved distinct history namespaces for non-UTF-8 Codex home paths.
- Made recorder health account for custom collection intervals, preserved the install-time `PATH` on Windows, and unloaded launchd services even when their plist was already missing.
- Reported native Windows process CPU, memory, peak-memory, and generic I/O counters in performance logs; renamed the cross-platform process counters to `ioReadBytes` and `ioWrittenBytes` under performance-log schema v2.
- Preserved all loaded Quota Remaining reset cycles so resets appear as observed jumps while genuine recorder gaps remain disconnected.

## [0.2.2] - 2026-07-26

### Added

- Added an exact local expiry reminder inside the Overview weekly quota gauge when the earliest complete, current, available Codex reset credit expires before the ordinary Codex weekly reset.

## [0.2.1] - 2026-07-18

### Added

- Added opt-in `--perf-log <FILE>` JSONL diagnostics for refresh stages, redraws, event wakeups, process CPU, memory footprint, page-ins, and disk I/O.

### Changed

- Parsed append-only rollout tails and incrementally reduced only affected threads, while safely falling back to a full parse for rewrites, truncation, replacement, or unstable files.
- Skipped materialization and snapshot derivation when rollout data and freshness state are unchanged.
- Replaced fixed-rate TUI drawing with dirty, event-driven redraws and ignored passive mouse motion.
- Compacted cached rollout events and collapsed consecutive foreign token baselines to reduce steady-state memory use.

### Internal

- Added cache fast-path, incremental-equivalence, redraw-state, mouse-capture, and performance-log coverage.
- Expanded refresh metrics with cache size, parsed-line, tail/full parse, incremental-reduce, and per-stage timing counters.

### Compatibility

- Snapshot JSON remains schema version 1. The rollout parser cache revision is bumped, so the first run rebuilds parsed rollout cache entries once before returning to the incremental path.

## [0.2.0] - 2026-07-17

### Added

- Renamed the Data Health view to Other and added a unified Resets table for quota windows and reset credits.
- Display reset-credit status, grant time, exact local reset time, provenance, stale state, missing details, and backend or viewport truncation.
- Added reset-credit details to Limits text and JSON output while preserving the distinction between `credits: null` and `credits: []`.

### Changed

- Simplified estimated-quota confidence presentation: entity rows now use `~` or `-`, while method, external-activity risk, and partial reasons live in the scope summary.
- Moved the task-status legend under the Tasks panel so it remains visible when Turns is collapsed.
- Preserved reset-credit details across failed refreshes as stale data without reading `auth.json` or calling the reset-credit consume API.

### Fixed

- Avoided double-counting rollout copies when an active session already covers the matching archived rollout.
- Rejected non-finite quota percentages so invalid windows cannot enter snapshots or estimates.
- Kept valid quota windows, reset-credit counts, and valid credit rows when optional reset-credit details are malformed.

### Internal

- Split large TUI, output, rollout, and session-launch test modules into focused helpers and test files.
- Expanded coverage for reset-credit parsing, cache compatibility, narrow terminals, keyboard and mouse behavior, and release installation.

### Compatibility

- JSON schema remains version 1. Existing confidence and preferred five-hour fields keep their prior meaning; reset-credit fields are additive and backward compatible.

[Unreleased]: https://github.com/ghostroller/codex-usage-monit/compare/v0.2.9...HEAD
[0.2.9]: https://github.com/ghostroller/codex-usage-monit/compare/v0.2.8...v0.2.9
[0.2.8]: https://github.com/ghostroller/codex-usage-monit/compare/v0.2.7...v0.2.8
[0.2.7]: https://github.com/ghostroller/codex-usage-monit/compare/v0.2.6...v0.2.7
[0.2.6]: https://github.com/ghostroller/codex-usage-monit/compare/v0.2.5...v0.2.6
[0.2.5]: https://github.com/ghostroller/codex-usage-monit/compare/v0.2.4...v0.2.5
[0.2.4]: https://github.com/ghostroller/codex-usage-monit/compare/v0.2.3...v0.2.4
[0.2.3]: https://github.com/ghostroller/codex-usage-monit/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/ghostroller/codex-usage-monit/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/ghostroller/codex-usage-monit/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/ghostroller/codex-usage-monit/compare/v0.1.1...v0.2.0
