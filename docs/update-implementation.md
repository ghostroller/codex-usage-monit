# Unified application updates — implementation checklist

The local command and the remote TUI invoke the same updater on the target
machine. SSH is a transport/bootstrap layer, not a second activation engine.
The application remains one complete executable for CLI, TUI, exporter and
recorder use.

## Scope and acceptance

- [x] Share one target-machine update executor for local and SSH invocation.
- [x] Offer `sync` (exporter and an existing recorder) and `node` (also the
  current user's managed CLI entry) scopes, with explicit TUI labels.
- [x] Preserve the existing sync scope for old host configurations; save a
  user's selected scope per host and show it before updating.
- [x] Identify installation ownership and require explicit adoption of an
  existing unmanaged CLI, including a custom entry selected through PATH; never
  silently overwrite a package-managed program.
- [x] Use one version store under the platform application root and a stable
  CLI entry; keep Windows updates possible while prior processes are running.
- [x] Journal the complete node update, serialize mutations, preserve service
  settings/enablement, verify the new recorder heartbeat, and resume failures
  forward. No implicit old-writer rollback after data migration.
- [x] Report exporter, recorder and CLI outcomes separately, including PATH
  shadowing, restart requirements and partial sync success.
- [x] Keep exact center/build matching for remote deployment and reject an
  incompatible/shared-state downgrade instead of silently switching versions.
  Persist a v2 recorder journal with a minimum updater version so the published
  0.5 updater also refuses before stopping an upgraded service.
- [x] Configure publication of standard platform archives/executables and one release manifest;
  eliminate duplicate `.agent` binaries in future releases while retaining
  already-published release assets.
- [x] Share release selection and checksum/identity checks across local and
  remote acquisition; keep trusted development bundles explicit.
- [x] Migrate active references away from legacy agent directories without
  deleting unknown references. Provide bounded, reference-aware version cleanup.
- [x] Update installer, CLI commands, TUI help, English and Chinese docs.
- [x] Add regression coverage for download/archive failures, ownership/PATH conflicts, scopes,
  service recovery/concurrency, old installation migration and Windows entry
  behavior, plus shortcut rendering/keyboard/mouse/compact layouts.
- [ ] Run affected tests during implementation and the relevant native macOS,
  Docker Linux and UTM Windows suites on the settled source; record exact source
  identities, commands, exclusions, results and logs before hosted CI.
  Native macOS and Docker Linux full suites passed; Windows has partial native
  evidence, but its final UTM run is blocked by guest resource exhaustion.
  Hosted CI remains pending after API failures and an automatic-review refusal;
  the user chose to keep the current results and stop further CI attempts.
- [x] Update this Mac's CLI only after the implementation and verification are
  ready; confirm the actual shell entry and recorder identity.

## Data directory follow-up

- [ ] Design and independently validate migration into `config/` and grouped
  `state/` subdirectories. The executable update preserves existing custom and
  default configuration/history paths; it must not combine a directory move
  with service replacement or rotate the source identity.

## Implementation evidence

Started from clean commit `39c703310e96c8cf3471e3db115b9e21aba2b81e` on
macOS ARM64. No release/tag publication is implied by this implementation.

Commands, snapshot identities, results, exclusions and local log locations are
recorded in [the verification record](update-verification-20260920.md).
