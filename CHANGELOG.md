# Changelog

All notable changes to this project are documented in this file.

## Unreleased

### Added

- Added native 64-bit Windows CI and direct GitHub Release distribution as a checksummed `x86_64-pc-windows-msvc` executable.
- Added PowerShell-safe copied resume commands and Windows `codex.cmd`/`PATHEXT` command discovery.

### Fixed

- Used `%USERPROFILE%\.codex` when Windows does not provide `HOME`, and made persisted TUI state replacement work on Windows.

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

[0.2.2]: https://github.com/ghostroller/codex-usage-monit/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/ghostroller/codex-usage-monit/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/ghostroller/codex-usage-monit/compare/v0.1.1...v0.2.0
