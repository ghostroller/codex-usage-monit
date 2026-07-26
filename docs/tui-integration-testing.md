# TUI integration testing

The TUI test suite uses deterministic Codex-shaped fixtures and exercises the
same rendering and input paths as the application. It deliberately avoids a
developer's real Codex home, cache, configuration, and UI state.

Run the complete verification suite with:

```bash
cargo test --locked --all-targets
```

## Test layers

- `src/tui/tests/testkit.rs` provides a `TestBackend` harness that loads a
  fixed `Snapshot`, renders at an explicit terminal size and theme, sends
  keyboard or mouse input, and serializes the resulting frame.
- `src/tui/tests/integration_scenarios.rs` checks semantic layout snapshots,
  shortcut styling, full-label hitboxes, keyboard/mouse state parity, text
  input precedence, resizing, and SVG gallery generation.
- `tests/tui_data_integration.rs` launches the real binary against an isolated
  fixture Codex home. A second case supplies a child-process-only mock
  `codex app-server`, so local rollout and server quota data are tested
  together without changing the user's `PATH`.
- `tests/tui_pty.rs` runs the real TUI in a pseudo-terminal and verifies raw
  keyboard input, SGR mouse input, terminal resize, rendered styles, and clean
  exit. This test is Unix-only.

The source fixtures live under `tests/fixtures`. Their timestamps, identifiers,
limits, task hierarchy, model metadata, Unicode text, and partial-data
diagnostics are fixed so output does not depend on the clock, timezone, network,
or machine-local Codex state.

## Reviewing layout output

Every test run writes the current visual review set to:

```text
target/tui-gallery/
```

The SVG files cover wide and compact sizes, dark and light themes, empty and
partial data, filters, hierarchy state, diagnostics, and modal state. They are
generated from the same semantic frame representation used by the snapshot
assertions. CI uploads the directory as the `tui-layout-gallery` artifact even
when another verification step fails.

Semantic baselines are stored in `src/tui/tests/snapshots`. When an intentional
layout change makes a snapshot fail:

```bash
cargo install cargo-insta
cargo insta test --locked --lib
cargo insta review
cargo test --locked --all-targets
```

Review text, styles, shortcut bindings, and hitbox rectangles together. Do not
accept a baseline solely because the character rows look plausible: a changed
style or one-column hitbox drift can break keyboard/mouse affordances without
making the text obviously wrong.

## Adding a control or layout

For each new shortcut-labelled control, add:

1. a `ControlId` entry with the exact active binding;
2. a keyboard transition assertion;
3. clicks at the start, middle, and end of the visible label;
4. shortcut-accent assertions in every focus where the binding is active;
5. compact-terminal coverage when the control is visible there;
6. a semantic snapshot or gallery scenario when the layout materially changes.

If printable input is accepted in the same view, also assert that text-entry
focus consumes the shortcut character before global bindings.
