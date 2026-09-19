# Remote node updates and recorder replacement

`remote deploy HOST` and Settings **[B] Update node** update the remote exporter
and an existing application-managed recorder together. The normal path downloads
the exact official GitHub Release on the SSH host. `remote deploy-dev HOST
--bundle-dir DIR` uses the same activation procedure with an explicitly trusted
development bundle. Local upload remains development-only; it is never a fallback
for a missing release. Older wire protocols are not translated or negotiated.

## Update sequence

1. Download into a unique staging directory, validate the manifest and binary,
   and install an immutable build/target/digest-qualified executable. The old
   executable and recorder remain usable during preparation.
2. Verify executable metadata and checksum, then probe the candidate's source
   identity, protocol, revisions, state access, rollout access and capabilities.
   Recheck the center's selected host/configuration before changing the service.
3. Invoke the verified candidate's `service upgrade --format json` over SSH.
   It imports the actual current-user service definition, rather than the SSH
   shell's defaults. Unknown or modified service definitions fail before stop.
4. Preserve collection options, Codex paths, custom history/status paths, remote
   configuration, project mappings, PATH, performance/trace logs and enablement.
   No installed service means a successful no-op. A disabled service remains
   disabled. An enabled service is restarted, including one that had crashed.
5. Persist an upgrade journal, disable future triggers, request cooperative stop
   from recorders supporting it, and verify quiescence using the existing service
   cutover gate and recorder singleton. The platform manager provides bounded
   termination for older or unresponsive recorders; persisted writes retain their
   existing atomic/WAL recovery semantics.
6. Register the same user service with the new executable's absolute path. Verify
   its exact definition/trust before allowing startup. Windows uses the existing
   least-privilege per-user Task Scheduler task. Disabled tasks are never started.
7. For enabled services, require the manager to report running and the recorder
   to publish a recent, successfully persisted history heartbeat containing the
   expected build ID, service-definition ID and a new process start time. Merely
   returning success from the service manager is insufficient. Quota collection
   or SSH diagnostics may still be reported as degraded after local persistence.
8. Switch the center's configured agent with a configuration revision check.
   For paired, sync-enabled sources, run a bounded real synchronization using the
   same history root and existing bandwidth/page limits. Unpaired or disabled
   sources are not implicitly paired, enabled or synchronized.

The managed binary is separate from a package-manager/global installation. A
recorder now points directly at the immutable managed build. No source identity
rotation, wholesale cache deletion, credential changes or elevated service
account is required. The existing Windows interactive-token task still requires
the user to remain logged in; this update does not change that persistence model.

## Recovery and success reporting

The private current-user service coordination directory holds
`recorder-upgrade.json`. It records the target build/options, prior definition
fingerprint, original enablement, phase, preparation time and last failure.
Install, uninstall and upgrade share a mutation lock. Upgrade phases are
`prepared`, `replacing`, `awaiting_heartbeat`, `complete` and `failed`.

If SSH disconnects or the updater crashes, retry the deployment or execute the
candidate's `service upgrade`. Recovery retains the original configuration even
if failed registration cleanup removed the task. It rejects a registration that
was independently changed. A crash between publishing the definition and loading
it into the service manager is recoverable only with the saved cutover intent,
an exact matching definition and proof that the manager has no active writer.
Explicit `service install`/`uninstall` supersedes the
saved upgrade after successful completion. No background updater is installed.

Preparation failures leave the old service alone. After replacement begins,
recovery proceeds forward with the retained configuration. It does **not**
automatically restore an older writer: history migration can be one-way, and
restarting an older binary could write incompatible state. Uncertain cleanup
retains the existing durable cutover blocker. Prior binaries and historical data
are retained, but their presence is not a promise of safe downgrade.

Remote service changes and center configuration cannot form a cross-machine
atomic transaction. A disconnect or center configuration conflict can leave a
successfully updated recorder while the center still selects its previous agent.
Retrying deploy converges those two states; failure never silently rewrites a
newer user configuration. Once the new agent is activated, verification-sync
failure leaves it selected and returns exit **2** with a clear partial-success
message. Continue with **Sync now**. Offline SSH hosts and bandwidth caps do not
roll back a working recorder. The initial verification is bounded and may require
continuation for large histories.

Center event logs distinguish `remote.recorder.upgrade` from
`remote.update.verify_sync`. Service readiness reports carry the target build,
enablement, recorder PID, last successful history heartbeat and any diagnostic.
`service status --format json` reads the registered service's saved options and
includes the recorder's actual build/version and process start time. Older
recorders lacking build metadata are reported as unknown, not assumed current.

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

Windows regressions cover registration argument import/quoting, configuration
preservation, disabled services, durable failure/retry, stale registration
conflicts, process-bound stop requests, build/heartbeat readiness and new-journal
WAL replay. Existing service tests continue to cover stop, trust, cutover and
cleanup behavior. Real SSH deployment and data synchronization are separate
evidence from unit tests and must report service and sync results independently.
