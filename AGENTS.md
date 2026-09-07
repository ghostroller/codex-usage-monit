# Repository Guidance

## Verification: local first, hosted at checkpoints

- Read [the testing workflow](docs/testing.md) before choosing a test environment. During implementation, run affected tests locally. For a completed batch of platform-sensitive changes, use the local Docker Linux and UTM Windows runners before considering hosted CI; use native macOS for macOS behavior.
- Linux: `sh scripts/test-linux-docker.sh --filter TEST_NAME` for focused work; omit the filter for the full suite. The default tests the Docker daemon's native architecture; request `--platform linux/amd64` when x64 coverage matters. `scripts/build-linux-amd64-docker.sh` builds a binary and is not a test pass.
- Windows: start with `python3 scripts/macos/test-windows-utm.py --doctor --toolchain-home 'C:\Users\user'`; substitute the actual toolchain owner. Run affected tests with `--focused --test-filter TEST_NAME`, or omit both for the full suite. Follow [Windows setup and recovery](docs/windows-testing.md) if the guest is not ready. Cross-target `cargo check` is supplementary and never counts as native Windows/ConPTY execution.
- Record the source commit, dirty snapshot identity, platform/architecture, complete command, skipped checks, result and log location. A running VM, a Docker build, or a successful `utmctl exec` exit code is not test evidence. Docker/UTM runners test isolated snapshots and record their results. Native macOS verification tests the checkout directly: keep it stable during the run and record its Git state. Do not mix an earlier successful run with later edits.
- GitHub CI is a deliberate integration checkpoint after substantial changes, before merge/release, or to cover a documented local platform blocker. Do not dispatch it for each edit, commit, push, or focused-test failure. Diagnose local failures first; a broken local VM is not permission to start repeated hosted runs.
- At a checkpoint, commit and push the intended branch, then use `python3 scripts/run-ci.py --local-results 'Linux: ...; Windows: ...; logs: ...'`. The helper refuses dirty/unpushed code and passes the exact SHA. Monitor the returned run ID and verify its `headSha`; never select an unrelated "latest" run. Re-run only for a new change, a failed check, or a concrete unresolved concern.
- Do not create tags to request ordinary tests. `v*.*.*` tags initiate release validation and publication. Pushes/PR updates do not run the full hosted suite; the scheduled dependency audit is the intentional exception. Preserve all release validation dependencies when editing workflows.

## TUI shortcut affordances

- Render every visible control with a keyboard shortcut in a btop-style label: style the exact shortcut grapheme separately with the accent color and bold weight, and render the rest with the normal control style. In a selected inverse-color button, the whole button may use the accent background; keep the shortcut distinct with underline, weight, or contrast instead of accent-on-accent text.
- Never highlight a letter that is not an active binding in the current view and focus. Show numeric or symbolic bindings as their actual key; use `↵` for Enter and `←` for back navigation.
- Keep the whole label clickable. Compute hitboxes from Unicode display width and keep their geometry stable across active, inactive, focused, compact, and light/dark states.
- Text-entry focus must consume printable keys before global shortcuts.
- New shortcut-labelled controls require render, keyboard, and mouse tests, including compact terminal coverage.
