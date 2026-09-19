# SSH remote usage in v0.4

One center can collect usage from explicitly selected SSH machines. Install the
monitor on both ends; the remote runs a short-lived exporter on each request.
The monitor adds no listening service, but the remote must already accept SSH
logins. It sends normalized usage, bounded session evidence and recent task
metadata, rather than copying raw rollout files. Recorded quota observations are
also synchronized, with explicit same-account confirmation required before they
contribute to the center's quota history. Reset credits and the live account
snapshot remain the center's account information, counted once.

## First connection

### Version policy during rapid iteration

**We intentionally do not implement compatibility with older data protocols.**
There is no protocol downgrade, old payload translation, or fallback that silently
omits quota history. The project is evolving quickly: update the remote agent to
the center's matching build when their protocols differ. A package version such
as `0.4.0` alone does not establish compatibility. `remote-agent info` is a small,
stable JSON bootstrap command independent of the usage wire protocol; it reports
the package version, source build ID, target and exact data protocol version.
The build ID hashes normalized Rust sources, the embedded release bootstrap
scripts, `Cargo.toml`, `Cargo.lock` and `build.rs`, so unpublished edits have a
different identity while the same source
on different platforms has the same identity. Different build IDs are displayed;
ordinary sync still requires exact protocol and data revisions/catalog agreement.
Deploy always selects the exact center source build and target platform.

An older executable that predates `info` is reported as **unknown/missing bootstrap
metadata**, never guessed to support a particular protocol. An explicit OS probe
can still install its replacement. This installation bootstrap is not support for
its old data protocol. A legacy protocol rejection is classified as
`compatibility` / `agent_version_mismatch`, not as an SSH network failure.

### Inspect and deploy a matching agent

```sh
codex-usage-monit remote inspect local-mac
codex-usage-monit remote deploy local-mac
codex-usage-monit remote test local-mac
```

In TUI **Settings**, select a configured host and press **[B] Update node** (or
click the label). **[C] Test** displays the agent version/build when available and
checks state, rollouts, source identity and data revisions. An unpaired host can
be deployed; pairing and enabling automatic sync remain explicit operations.

**The normal deployment path downloads the official Release on the remote host.**
The center probes the OS/architecture and sends a small embedded bootstrap script
over SSH. That script fetches the manifest and binary from the fixed repository
`https://github.com/ghostroller/codex-usage-monit/releases/download/v<VERSION>/`.
It checks the manifest's exact source build, version, protocol, platform, bounded
size and SHA-256 before executing the candidate installer. Downloads and redirects
require HTTPS. There is no `latest`, older-release or local-upload fallback.
The installer creates an immutable private copy below the SSH login's working directory:
`.codex-usage-monit-agents/<source-build-prefix>/<target>/<binary-digest-prefix>/`.
Directory keys use 32 build-ID and 16 binary-digest hex characters to keep Windows
launch paths short. Verification still checks the full build ID and SHA-256. Windows
uses an `.exe` and native backslash paths. SSH retains strict host-key checking
and batch authentication. The remote login must have a stable writable working
directory (normally its home directory) and HTTPS access to GitHub release assets.
Windows remotes need x64, Windows PowerShell 5.1 or later, `curl.exe`, and a cmd.exe
or PowerShell SSH login shell. Unix remotes need `uname`, Python 3.8 or later and
`curl`. The remote needs neither project source nor a compiler. The center does
not download or upload a local executable in this mode.

The installed binary must return the expected build, target, protocol and full
checksum, then pass the normal data probe with the existing source identity pin.
The verified candidate next upgrades any existing application-managed recorder,
preserving its registration options and enabled state. An absent recorder is not
created; a disabled recorder is not started. Enabled recorders must publish a new,
build-verified history heartbeat before the center switches `agentExecutable`
with a configuration revision check. Paired, sync-enabled sources then perform a
bounded real sync; incomplete verification returns exit 2 and keeps the new agent
selected for retry. No old protocol fallback or manual cache deletion is needed.

Preparation failures retain the previous service/configuration. A later failure
can leave the remote service updated while center activation is pending; retry
deploy to converge them. The private upgrade journal preserves service options
through failed registration cleanup and interrupted SSH connections. Recovery
proceeds forward, never blindly restarting an older writer after data migration.
Previous managed builds remain on disk; an explicit old-path selection is not a
guarantee that a downgrade can read the current state. See [remote update and
recovery](remote-updates.md) for the complete sequence and failure semantics.

Deployment preserves the source identity and global/package-manager executable;
the upgraded recorder points at the new immutable managed build.
A custom launcher that sets `CODEX_HOME` or state paths
must have those same settings in the SSH login environment before using a managed
agent; otherwise verification fails and leaves its configured launcher in place.

`remote deploy` and TUI **[B] Update node** ignore local bundles,
`CODEX_USAGE_MONIT_AGENT_DIR`, adjacent `agents` directories and the center's own
executable. `remote deploy --bundle-dir ...` is rejected. A development build with
unpublished edits must not silently install an older official binary, even when
both binaries report the same package version.

Release CI generates `.agent` (Windows: `.agent.exe`) and `.agent.json` assets
with `scripts/package-agent.py` on each native build runner. The manifest contains
schema version, agent identity, the fixed binary filename, byte size and SHA-256;
it is generated from the binary's bootstrap metadata, not handwritten. Formal
deployment requires these assets in the matching version's Release. Older
Releases without them are not converted or used as a fallback.

The trust root is the fixed official GitHub repository, its release process and
HTTPS. **A build ID and a hash are not publisher signatures:** a replaced binary
and replaced manifest can agree with each other. This implementation does not
verify an independent publisher signature or GitHub artifact attestation and
cannot protect against a compromised center, remote login, publisher account or
release workflow. Pulling directly on the remote avoids accepting an arbitrary
local executable through the normal update interface.

Useful diagnostics:

| Code | Meaning / next step |
| --- | --- |
| `agent_release_unavailable` | The required tag or agent asset returned HTTP 404; use a center build whose matching agent has been published. |
| `agent_release_mismatch` | The Release exists but its build/protocol/platform differs; a development center requires a matching development bundle or a matching published center. |
| `agent_release_invalid` | The Release manifest or asset metadata is malformed; repair the published artifacts. |
| `agent_release_download_failed` | Remote HTTPS/curl failed; check the remote's network, curl and TLS configuration. |
| `agent_checksum_mismatch` | Downloaded bytes differ from the manifest; the candidate is not executed. |
| `agent_release_prepare_failed` | Remote bootstrap failed, for example because Python/PowerShell is unavailable; inspect the accompanying SSH diagnostic. |

### Explicit development-only local upload

**An unpublished cross-platform development build needs a matching binary.** A
Windows executable cannot run on macOS. Build the same source snapshot for each
required target, then run this on that binary's native platform, and only for
locally trusted builds:

```sh
cargo build --release --locked
python3 scripts/package-agent.py target/release/codex-usage-monit --output-dir dist/agents
```

Windows developers can also [cross-compile an Apple Silicon executable locally](windows-macos-cross.md).
That workflow builds the binary on Windows; the native packaging step above is
still required to inspect it and create its deployment manifest.

For Windows use `python scripts/package-agent.py target/release/codex-usage-monit.exe
--output-dir dist/agents`. Copy the generated binary and `.agent.json` together to
the center's bundle directory, then explicitly opt into the development CLI:

```powershell
cargo run -- remote deploy-dev local-mac --bundle-dir 'D:\AgentBundles\current'
```

This is a temporary development mechanism, not the normal updater. It prints a
trust warning, requires an explicit directory, verifies the full metadata and
hash, uploads through system SCP, then uses the same installation/readiness and
configuration activation checks. It is unavailable in the TUI and is never used
automatically after Release failure. The operator is authorizing execution of
the supplied file; neither packaging nor its checksum proves that it is benign.
SCP and SSH must refer to the same remote working directory. Cleanup removes only
the operation's random `.codex-usage-monit-upload-*` staging file.

Missing or mismatched development artifacts report `agent_artifact_missing` or
`agent_artifact_mismatch` before upload. Changing the package version alone does
not make an older binary acceptable. Both deployment modes have a five-minute
installation budget, bounded subprocess output and process-tree cancellation;
the final data probe has its own transport bounds. No hosted build, remote
compilation or git pull is started implicitly.

### Configure a source

1. Install the monitor on the center. Install it on the remote, or use **Deploy
   agent** below to install a matching managed exporter. Use matching model catalogs on both
   ends: protocol, metric revisions and the effective catalog fingerprint must
   agree. Automatic sync never installs software; deployment is an explicit action.
2. Configure one system OpenSSH alias and verify the remote host key through your
   normal SSH setup. The application uses batch authentication and strict host
   key checking; it cannot answer password, key-passphrase or host-key prompts.
3. Leave `Agent exe` at `codex-usage-monit` for automatic setup, or specify
   `--agent-executable /absolute/path/codex-usage-monit` when adding it.
   Test and Pair first try the SSH login's **non-interactive** PATH. If a Unix
   shell reports that this default command is missing, they probe `~/.local/bin`,
   `~/.cargo/bin`, `/opt/homebrew/bin`, then `/usr/local/bin`. A valid protocol
   response saves the working executable path for subsequent syncs. Explicit
   custom paths are used exactly as entered. Discovery does not explicitly load
   shell startup files.
   This is an executable token, not a shell command: spaces, quoting and extra
   arguments are rejected. Windows can use a path such as
   `C:\Tools\codex-usage-monit.exe`; `~` expansion is Unix-shell-specific.
4. The remote login must be able to read its Codex home and write the monitor's
   own state. A custom Codex home or environment can be supplied by a small
   remote launcher script with a safe executable path. Keep launcher output off
   stdout because stdout carries the binary protocol.

Remote title/message previews are redacted by default. The center and each
remote in its history profile must use the same redaction setting. This safe
starting sequence keeps redaction enabled:

```sh
codex-usage-monit --redact-content remote add buildbox --ssh-host dev-server
codex-usage-monit --redact-content remote pair buildbox
codex-usage-monit --redact-content remote test buildbox
codex-usage-monit --redact-content remote sync buildbox
codex-usage-monit --redact-content summary --source all --range 30d
codex-usage-monit --redact-content
```

`add` does not connect. Pairing pins both the remote node ID and its generation;
it does not enable automatic synchronization. `test` checks readiness without
syncing usage. A paired, disabled host still accepts an explicit `sync`.
If a sync reports `continuation` or `bootstrap-restarted` (exit 2), repeat the
same command until the aggregate is complete. Each round is bounded.

Continue using `--redact-content` for this center's TUI, reports and recorder.
If previews are wanted instead, explicitly set
`remote edit buildbox --redact-content false` and consistently run the center
without the flag. Stop collectors using the previous profile before switching;
the profile lease prevents incompatible concurrent writers. Redaction is a
collection policy, not a command to erase previously stored history.

If your recorder uses a custom history location, pass that exact directory to
remote commands (including Pair/Test recovery, Unpair/Remove and Source actions)
and reports, for example:

```sh
codex-usage-monit --redact-content remote --history-dir /srv/monit/history-v1 sync buildbox
codex-usage-monit --redact-content summary --history-dir /srv/monit/history-v1 --source all
```

The TUI and service commands select their default state root instead of taking
`--history-dir`. For `/srv/monit/history-v1`, set
`CODEX_USAGE_MONIT_STATE_DIR=/srv/monit` consistently when starting the TUI or
running `service install`, `status`, or `uninstall`. The override is the parent
of `history-v1`, not that directory itself. Likewise, keep `--codex-home` (or
`CODEX_HOME`) consistent across the center's commands; it identifies the local
history profile.

## Automatic collection and the TUI

Enable both switches only for machines that should be polled:

```sh
codex-usage-monit remote enable buildbox
codex-usage-monit remote config --auto-sync true
codex-usage-monit --redact-content record
```

The recorder performs automatic synchronization. To keep it running in the
background, install/reinstall the user service with the same collection options,
including `--redact-content`; see the README's recorder setup. The TUI works
independently for viewing and manual operations, but merely opening it does not
start the recorder. The default scheduler uses one worker, polling active hosts
about every 60 seconds and idle hosts about every 300 seconds, with backoff on
failure. Both automatic switches are off for a newly added configuration.

Settings → Remote sources provides host creation/editing, pairing, readiness
tests, manual sync and enable/disable controls. For a new host,
Save → Test → Pair → Sync now completes
manual setup. Test can save an automatically discovered executable; it does not
pair the host or enable automatic sync. Manual failures show bounded, terminal-safe
CLI/SSH details in Remote sources and Other → Diagnostics for the current TUI
session. These details are not written into trace or sync-health records.
Project mapping provides explicit
merge/split actions. Git evidence suggests mappings; it never silently merges
projects. Other shows per-host aggregate status, session-fact attention,
bandwidth pauses and SSH process-cleanup pauses.

## Synchronize quota remaining history

Keep a recorder (or a collecting TUI) running on the always-on machine. The SSH
exporter reads its existing local quota recordings; it cannot reconstruct samples
from periods when no monitor was recording, and does not call the account API.
The exporter, recorder and TUI must use the same Codex home and state root.

After pairing and synchronizing a source, select it in Settings → Remote sources
and press **O**, labelled **Same account quota: off**, to confirm that its retained
quota observations belong to the same account as the center. Press O again to
stop merging. The corresponding CLI commands are:

```sh
codex-usage-monit remote source merge-quota NODE_ID
codex-usage-monit remote source separate-quota NODE_ID
```

Use the same redaction and state-directory options as other remote commands.
The default is off. This is a user confirmation, not automatic account detection:
historical samples have no account identifier. Do not enable it for a different
account or a source that switched accounts during its retained history. Use
separate history profiles/state roots when recording different accounts.

Raw observations stay source-owned even when merging is off. Only locally
recorded account shards are exported, so two machines can synchronize each
other without forwarding imported observations back and forth. Excluding a
source from aggregates also excludes its quota contribution. Detaching/unpairing
a source clears its same-account confirmation. Disabling automatic sync merely
stops polling; it does not remove retained history.

The merged curve unions observed periods within the retained 35-day window.
For each limit and duration, reset times within two minutes of the earliest
reset in a cluster are treated as clock drift; clustering does not chain across
successive near matches. Each five-minute sampling slot uses the newest actual
observation. Equal timestamps use the higher used percentage (then lower
remaining percentage) as a deterministic tie-break. Percentages are never added
or averaged. Separate reset cycles and gaps remain separate; no samples are
invented for recorder outages. The original observations remain available for
reprojection when a source is excluded.

Quota remains an account-wide history in Trends, independent of its token source
selector. The token and EST charts use the selected usage sources: All combines
included local and remote usage, while Local or an exact remote selects that
usage source. Replica reconciliation runs before All is aggregated; insufficient
evidence and unavailable estimation inputs remain marked partial/unknown.

Trace logs include `history.v2.quota_merge` with local, remote and merged point
counts and the number of confirmed sources. Remote delta statistics include
`quotaChangesEmitted`; `remote.quota.commit` records committed day-change and
point counts. Quota recordings alone do not move an idle host to the active
polling interval. An unreadable remote quota store produces
`quota_history_unavailable`; usage export can still succeed.

## Read and manage retained usage

- Summary and Trends support `--source all`, `--source local`, and a full
  `node-…` ID. Their TUI source selectors use the same meanings. Obtain IDs from
  `remote list --format json` or `remote source list`.
- Overview combines all included sources independently of the Summary/Trends
  selector. Remote rows are read-only. Usage comes from source-aware history;
  bounded live metadata provides recent status and becomes stale after 15 minutes.
- Copies and continued sessions use digest/event evidence for deduplication.
  Insufficient or conflicting evidence remains explicitly partial; a successful
  aggregate exchange does not imply every session-fact follow-up succeeded.
- `remote disable ID` stops automatic synchronization for that host. Retained
  usage remains available, and explicit manual sync still works.
- `remote source exclude NODE_ID` removes a source from All and replica
  reconciliation while preserving exact-source inspection and synchronization.
  `include` restores participation.
- `remote remove ID` retains detached history and excludes it from All by
  default; `remove --keep-included` preserves its existing inclusion policy.
  `unpair ID` disables sync and clears the identity pin while preserving the
  source's existing inclusion policy. Use `source exclude` to hide its retained
  usage from All. Purge is a separate, irreversible operation restricted to
  detached sources.

## Limits and recovery

The normal aggregate window is 31 days with overlap; the exporter retains a
bounded 35-day domain. Long first scans can take time even though network output
is paged. A manual round allows at most four aggregate pages, approximately
16 MiB and 60 seconds per SSH exchange. Automatic rounds use one page,
approximately 4 MiB and 30 seconds per exchange. The aggregate network loop has
a five-minute absolute deadline. Session facts have their own cursor and separate limits; the
whole command can take longer when it also scans local data or follows up facts.
No-change aggregate responses stay compact.

The rolling 24-hour budget is **per configured source**, including conservative
SSH/ProxyJump overhead. Automatic bulk work pauses at 150 MiB; ordinary sync
traffic pauses at 250 MiB. Manual `remote sync ID --ignore-budget` is an explicit
one-invocation override. Pair/Test remain available for manual diagnosis; budget
pauses can also permit infrequent bounded automatic readiness probes. The ledger
is a conservative admission estimate, not an OS network-byte counter.

An unreachable remote does not erase retained history or stop local collection.
An incomplete rollout scan can publish new lower-bound aggregates and monotonic
updates to already-partial keys. It cannot replace a more complete key, lower
any retained metric/group, or delete an omitted key. Exact session-fact export
still requires a complete scan, so aggregate completion is not proof of complete
usage coverage or replica reconciliation.
For `facts=attention`, inspect Other and retry after addressing its category;
already committed aggregate pages remain usable. A process-cleanup pause stops
automatic connections for that exact host across restarts. A successful explicit
Test/Sync, or an intentional edit to that host, clears it. Inspect custom SSH
wrappers/ProxyCommand first when cleanup repeatedly fails.

Moved or deleted project directories can prevent a historical session from
establishing a source-scoped project identity. Its aggregate usage can remain
visible while exact fact reconciliation reports attention. The monitor does not
invent project evidence to force deduplication.

If identity changes, verify the intended remote, then explicitly unpair and pair
again. Do not copy another machine's monitor state directory as an installation
method: the persistent identity belongs to that source. A version/catalog
mismatch needs compatible installations, not repeated sync attempts.

## Rebuilding incompatible derived data

The current implementation uses remote protocol v5, history metric revision 5,
and parser cache revision 14. Protocol v5 adds quota-day journal changes to the
bounded, replayable aggregate protocol. Upgrade both endpoints before syncing.
Existing v4 cursors use a separate binding; the first v5 sync bootstraps a new
generation and keeps the old visible history until publication. Quota support
does not require deleting existing account observations or usage history.
The protocol also carries an explicit
`unclassifiedTokens` component: input + output + unclassified must equal total.
Both endpoints must use this implementation before synchronizing.

When discarding incompatible development aggregates is acceptable, stop the
recorder and other monitor writers on both endpoints, back up their state and
installed executables, and move the affected derived source history, remote
ingest cursors, exporter journals/facts and parser caches outside the active
state roots. Clear the affected summary-backfill marker as well. Do not copy
old cursors or remote live projections into the rebuilt store.

Preserve raw Codex rollout logs, monitor source identity/anchor, remote pairing
and automatic-sync settings, project mappings, and account quota observations.
Quota samples and usage whose raw logs are no longer retained cannot be
reconstructed by rescanning. Do not erase the entire state directory as a cache.

Install matching binaries, restart/reinstall the recorder with its existing
profile and collection options, and run a local `summary --range 30d` to trigger
the bounded backfill. Test the paired remote, then perform manual sync rounds
until the aggregate bootstrap completes. Keep the existing content-redaction
policy consistent throughout; rebuilding must not silently enable previews or
automatic synchronization. Retain the backup until both local and remote
queries have been checked.

## Upgrading from v0.3

Back up the monitor's state before upgrading. Stop old foreground collectors and
replace/reinstall existing recorder services with the new binary before the
source-aware history cutover. Service/cutover checks prevent an old writer from
silently corrupting the new history. The v1→v2 switch is one-way; an older binary
does not understand v0.4's remote history. Existing SSH aliases are never imported
or enabled automatically.

For development verification, see [local-first testing](testing.md). The real
Unix SSH loopback test is `python3 scripts/test-ssh-loopback.py --binary PATH`;
it requires OpenSSH client/server tools, uses temporary fixtures and keys, and
does not contact a configured production host.
