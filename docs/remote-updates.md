# Application updates and recorder replacement

Local `update`, the shell installer, and remote deployment use the same updater
on the target machine. SSH adds platform discovery, download/upload transport,
and activation of the center's configured exporter. CLI, TUI, recorder and
exporter are modes of one complete executable, not separate distributions.

## Choose the scope

| Scope | Exporter / prepared application | Existing recorder | Managed CLI entry |
| --- | --- | --- | --- |
| `sync` | Install matching executable | Upgrade, preserving configuration | Keep current selection |
| `node` | Install matching executable | Upgrade, preserving configuration | Select the same version |

Scope applies to the current local user or SSH login, not every user on the
machine. No installed recorder means a successful no-op for that component;
a disabled recorder remains disabled. Already-open TUI processes keep using
their original executable until restarted.

```sh
# Local: latest official release; node scope is the default.
codex-usage-monit update
codex-usage-monit update --version X.Y.Z --scope node
codex-usage-monit update status --format json

# Explicit migration of an existing manually installed CLI.
codex-usage-monit update --install-dir "$HOME/.local/bin" --adopt

# Explicitly trusted development bundle instead of an official download.
codex-usage-monit update --bundle-dir /path/to/bundle --scope node

# Remote: exact center version/build; sync scope is the default.
codex-usage-monit remote deploy buildbox --scope sync
codex-usage-monit remote deploy buildbox --scope node
codex-usage-monit remote deploy buildbox --scope node --adopt
codex-usage-monit remote deploy-dev buildbox --bundle-dir /path/to/bundle --scope node
```

Local `--version` accepts `latest` or a release version, optionally prefixed with
`v`. `--bundle-dir` is mutually exclusive with `--version`. Local updates support
`--format json`; `update status` inspects installation ownership, managed versions,
the selected CLI entry, PATH resolution and the last update journal without
applying an update. `--install-dir` and `--adopt` require `node` scope.

Settings **[B] Update node** opens a dialog for the selected host. **[S] Sync
components** preserves its CLI; **[N] Node application** includes the CLI.
**[A] Adopt manual CLI** explicitly authorizes migration of a standalone existing
entry for that operation. **[↵] Update** starts the operation and **[←] Back**
cancels it. The TUI remembers scope per host, starting with `sync` for an unknown
preference; it never remembers adoption permission. The dialog shows the known
agent path and the center's target version/build. CLI ownership and PATH are
checked on the target; the dialog does not invent a remote CLI version.

An unmanaged ordinary file requires explicit adoption. Cargo, Homebrew, Scoop
and Chocolatey locations are rejected even with `--adopt`; update those entries
through their original manager or choose a separate application-owned directory.
Symlinks, special files, or externally modified registered launchers are not
overwritten. The shell installer requests migration of a previous manual entry
at its explicitly selected destination, then invokes this same node updater.

The updater uses an explicit local `--install-dir` first, otherwise the already
registered CLI entry. On first registration it checks the target process's PATH
and offers to adopt that existing CLI, including custom directories such as
`~/bin`. Without `--adopt`, an existing unmanaged entry stops preparation before
the recorder changes. Only when PATH has no CLI does it choose the default
user installation directory. SSH's PATH may differ from an interactive shell;
the result always reports the selected entry and any PATH conflict.

## Files and release assets

The managed application root is:

| Platform | Root |
| --- | --- |
| macOS | `~/Library/Application Support/codex-usage-monit/` |
| Linux | `$XDG_DATA_HOME/codex-usage-monit/`, or `~/.local/share/codex-usage-monit/` |
| Windows | `%LOCALAPPDATA%\codex-usage-monit\` |

```text
<application root>/
├── versions/
│   └── <version>-<full binary SHA256>/
│       ├── codex-usage-monit[.exe]
│       └── build.json
├── installation.json       # registered stable CLI and selected version
├── update-journal.json     # complete-node recovery state
├── update.lock
└── adopted-cli/            # preserved original manual CLI, when adopted
```

The default Unix CLI entry is `~/.local/bin/codex-usage-monit`; Windows uses
`<application root>\bin\codex-usage-monit.exe`. It is a stable executable
launcher that reads the selected version from `installation.json`, **not a
symlink**. Later updates atomically change this selection. On Unix the launcher
replaces its process with the selected version; on Windows it waits for that
version and returns its exit status. A running Windows launcher need not be
replaced on every update. First adoption of a running old Windows executable
may still require closing its CLI/TUI processes.

The recorder always points directly at an immutable version's absolute path,
so changing the command entry alone cannot alter the service's next start.
New deployments use this root instead of `.codex-usage-monit-agents/`. Existing
configured legacy paths stay usable until explicitly updated. Old versions and
legacy directories are retained; automatic reference-aware cleanup is separate
work. **Configuration and history paths are not migrated by this update**, and
custom data roots, source identities and remote pins are preserved.

New Release packaging publishes one standard `.tar.gz` per Unix platform, one
Windows `.exe`, one `release-manifest.json`, `SHA256SUMS`, and `install.sh`.
The shared manifest records version, build ID, protocol, target, archive size/hash
and executable size/hash. Local and remote acquisition consume the same assets;
there is no extra `.agent` executable or per-platform `.agent.json` publication.
Already-published Releases retain their original assets for their existing
clients. The new updater requires the shared manifest and does not silently
fall back to an old Release layout. Development bundles use the same manifest
and platform archives/executable, with explicit local trust.

## Update sequence

1. Download to a unique staging directory, validate the manifest and archive,
   then validate the extracted executable's full checksum and identity. Install
   an immutable version. The old recorder continues during preparation. For SSH
   deployment, verify the source pin, data revisions and readiness, then recheck
   the selected host's configuration revision.
2. Run the candidate's target-machine updater. Acquire the application mutation
   lock, validate CLI ownership when requested, and persist the selected scope,
   target and CLI plan in `update-journal.json` before activation.
3. Call the existing recorder upgrade mechanism. Import the actual current-user
   service definition, preserving collection options, Codex paths, custom
   history/status paths, remote configuration, PATH, logs and enabled state.
   Unknown or modified definitions fail before the old recorder is stopped.
4. Persist the service upgrade journal, disable automatic triggers and request
   cooperative stop. The platform manager provides bounded termination for old
   or unresponsive recorders; the cutover gate and singleton establish that the
   old recorder has stopped writing before changing registration.
5. Register the new immutable absolute executable path, verify the exact service
   definition and trust, then start only an enabled service. Windows retains the
   existing least-privilege current-user Task Scheduler model.
6. Require both a running manager status and a fresh, successfully persisted
   history heartbeat with the expected build ID, service-definition ID and new
   process start time. A manager command's exit status alone is insufficient.
7. For `node`, publish or validate the stable CLI and atomically select the same
   version. Report the entry and this process's PATH resolution. The caller's
   interactive shell may have a different PATH or cached command lookup; check
   `codex-usage-monit -V` there and reopen the shell or refresh its command cache
   if needed. `update` itself does not edit shell profiles; the shell installer
   retains its explicit PATH setup behavior.
8. For SSH deployment, switch the center's `agentExecutable` using its revision
   check. Paired, sync-enabled sources then run a bounded real sync with existing
   history and bandwidth/page limits. Unpaired or disabled sources are not
   implicitly paired, enabled or synchronized.

No background updater is installed. The Windows service still requires the user
to remain logged in; updating does not change service persistence policy.

## Recovery and component results

`update-journal.json` records the application target, scope, CLI plan and recorder
and CLI results. Its phases include `prepared`, `upgrading_recorder`,
`activating_cli`, `complete` and `failed`. An unfinished update must be resumed
with the same target and scope; competing updates cannot overwrite its intent.
The existing private service coordination directory separately retains
`recorder-upgrade.json`, which preserves the original registration and enablement
through service replacement. Service install, uninstall and upgrade continue to
share their own mutation lock.

Retry the same update/deployment after fixing the reported cause. For a local
update requested as `latest`, pin the reported version when resuming so a newer
publication does not select a different target. Recovery continues forward:
preparation leaves the old service alone, but a failure after service replacement
must not blindly start an old writer against possibly migrated history. Retaining
an old executable is not a guarantee of safe downgrade. The updater rejects a
lower version than the registered CLI or recorder. Same-version builds with a
different build ID require an explicit development bundle.

After updating an existing recorder, the retained service journal uses schema 2
and records `minimumUpdaterVersion`. The new reader accepts older schema 1
journals and upgrades them; the published 0.5 updater rejects schema 2 before
stopping the service. This prevents its normal remote deployment path from
undoing the version protection. Explicit service reinstallation/uninstallation
or manually removing the journal resets that boundary. Different centers can
retain separate agent references, but one user's recorder and CLI are shared
resources.

Reports distinguish installed exporter, recorder and CLI outcomes. For example,
a recorder can be ready while CLI activation fails, and CLI success can still
include a PATH-shadowing diagnostic. Local JSON reports and `update status`
retain component diagnostics. Remote partial results return exit **2** and must
not be described as complete merely because an executable was downloaded.
A configuration conflict or lost SSH connection can leave the remote updated
while the center still selects the previous agent. Retry the same deployment;
newer user configuration is never silently overwritten.

Once center activation succeeds, verification-sync failure keeps the new agent
selected and returns exit **2**. Continue with **Sync now**. Offline hosts and
bandwidth caps do not roll back a working recorder; large histories may require
more than one bounded verification round. Center event logs distinguish
`remote.recorder.upgrade` from `remote.update.verify_sync`.
`service status --format json` reports the recorder's actual build/version and
process start time using the registered options. Missing legacy metadata is
reported as unknown, not assumed current.

## Cache generations across updates

Live revisions belong to a source, data revision tuple, redaction profile and
exporter journal generation. A cursorless bootstrap advertises no known live
revision. A new journal establishes its baseline from a full snapshot; revisions
must remain monotonic within one journal. A stale bootstrap may not publish after
another history generation has become active.

Pre-upgrade live caches remain readable but do not advertise a known baseline.
An already saved full-snapshot WAL page can be replayed into its new journal even
when its counter is numerically smaller. A legacy revision-only WAL commits its
historical records without refreshing the unbound live cache; the next exchange
requests a full replacement. Receive timestamps are preserved during replay.
Old active historical data remains available until the new bootstrap completes.
This is local persisted-state recovery, not support for an old network protocol.

## Verification scope

The relevant Windows regressions exercise registration argument import/quoting, configuration
preservation, disabled services, durable failure/retry, stale registration
conflicts, process-bound stop requests, build/heartbeat readiness and new-journal
WAL replay. Existing service tests continue to cover stop, trust, cutover and
cleanup behavior. Real SSH deployment and data synchronization are separate
evidence from unit tests and must report service and sync results independently.
