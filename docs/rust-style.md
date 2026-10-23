# Rust style and review policy

This repository adopts the general guidance in the [Google Rust Style
Guide](https://google.github.io/styleguide/rustguide.html), reviewed on
2026-10-23, with the application-specific scope below. Repository behavior,
privacy guarantees, and the [testing workflow](testing.md) remain authoritative.

## Scope

`codex-usage-monit` is a Cargo CLI/TUI application. Its library target also
supports the executable and integration tests; `pub` alone does not establish
a supported third-party SDK. Chromium/Fuchsia-specific build layouts, thread
facilities, and test frameworks do not apply. Keep the pinned stable toolchain.

For new or substantially changed code:

- Use explicit production imports, grouped by source module. Keep allowed
  inline-test and enum-local glob imports. Qualify external helper functions
  when their origin would otherwise be unclear.
- Keep module entry points focused on wiring and exports. Move responsibilities
  incrementally, preserving callers and behavior rather than splitting by size.
- Name policy choices and grouping-key fields. Prefer borrowing when ownership
  is unnecessary; retain shared ownership when workers actually share state.
- Document non-obvious public contracts: side effects, failure reporting,
  preconditions, and panic conditions. Avoid comments that only repeat a name.
- Give each unsafe operation a concrete `SAFETY:` explanation and document why
  native code is needed. Unsafe functions require a caller safety contract.
- Treat poisoned business state as unusable. Do not recover history or cache
  contents with `PoisonError::into_inner`; stop the affected operation and expose
  a diagnostic. Restart or reconstruct from trusted inputs before resuming.

## Project-specific recovery contract

A TUI worker panic can interrupt updates to staged history or rollout caches.
The current session must not publish or retry work using those objects. It may
retain an already displayed snapshot, but must explain that refresh has stopped.
Exit cleanup must not flush a poisoned history store or panic while reporting
the original failure. Catching a worker panic is a last-resort reporting boundary,
not permission to continue using the interrupted state.

Git evidence caches commit explicitly after successful export preparation;
destructors do not publish pending evidence. Project-registration failure keeps
the existing partial-result diagnostic. Process-local profile and recorder
ownership registries reject new authorization after poison; dropping an existing
guard still releases its operating-system lock without recovering the registry.

Review all owners of a changed lock together, including initial loading,
background refresh, backfill, and shutdown. Existing diagnostic and notification
locks are separate owners, not evidence that recovery is safe for business data.
Any future changes to those owners need their own invariant review.

Windows SCM code owns raw handles, security descriptors, and process attribute
buffers. Its safety review must account for allocator/free pairing, successful
initialization, pointer bounds and alignment, handle ownership, and the lifetime
of every buffer borrowed by a native call. Comments are evidence to review,
not a substitute for fixing an invalid precondition.

## Enforcement and migration

The existing local verification entry points enforce `cargo fmt --all --
--check` and `cargo clippy --locked --all-targets -- -D warnings`. Default
rustfmt output remains the formatting baseline; import granularity is reviewed
manually where the stable formatter cannot enforce it.

Enable additional lints in a module only after auditing and fixing that module.
The Windows SCM module family enables `clippy::undocumented_unsafe_blocks`.
Do not enable all restriction/pedantic lints across the crate or suppress a new
lint globally to hide existing debt. Introduce `missing_docs` module by module
after documenting meaningful API contracts. Explain any narrow suppression at
its use site.

The first maintenance batch covers this policy, TUI poison handling, and the
Windows SCM safety audit. Named collection strategies/grouping keys, broader
API documentation/import cleanup, and TUI decomposition remain separate batches;
do not fold them into a recovery fix.

## Verification

Follow [local-first verification](testing.md) and any machine-specific setup.
Use actual held-lock panics for poison regressions and inspect durable history
after shutdown. Run affected tests during implementation, then the relevant
native suite once the batch is stable. Comment-only safety documentation needs
contract review and lint checks; behavior changes also need native regressions.

Record the source commit, dirty snapshot identity, platform, toolchain, complete
commands, skipped checks, outcomes, and logs. A focused pass is not a full suite.
This policy does not authorize hosted CI, release tags, or publication.
