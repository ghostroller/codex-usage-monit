# SSH remote usage in v0.4

One center can collect usage from explicitly selected SSH machines. Install the
monitor on both ends; the remote runs a short-lived exporter on each request.
The monitor adds no listening service, but the remote must already accept SSH
logins. It sends normalized usage, bounded session evidence and recent task
metadata, rather than copying raw rollout files. Account quota and reset credits
remain the center's account information, counted once.

## First connection

1. Install v0.4 on the center and remote. Use compatible model catalogs on both
   ends: protocol, metric revisions and the effective catalog fingerprint must
   agree. The exporter does not automatically install or update itself.
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
   `C:/Tools/codex-usage-monit.exe`; `~` expansion is Unix-shell-specific.
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

The current implementation uses remote protocol v4, history metric revision 5,
and parser cache revision 14. Protocol v4 carries an explicit
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
