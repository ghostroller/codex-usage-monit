# Changelog

All notable changes to this project are documented in this file.

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

[0.2.0]: https://github.com/ghostroller/codex-usage-monit/compare/v0.1.1...v0.2.0
