# Windows state directory permissions

`local-state/permission-denied` accompanied by a source identity DACL diagnostic
means the monitor rejected its local state/config permissions. These directories
are separate from `%USERPROFILE%\.ssh`. By default, both are under
`%LOCALAPPDATA%\codex-usage-monit`. `CODEX_USAGE_MONIT_STATE_DIR` / `XDG_STATE_HOME`
and `CODEX_USAGE_MONIT_CONFIG_DIR` / `XDG_CONFIG_HOME` can select other locations;
inspect the path in the diagnostic and repair each affected application directory.

The monitor permits access by its current Windows user, SYSTEM and built-in
Administrators. An extra allow entry, even read-only or inherited, fails this
policy. Ownership must match the running user, or the process's default
Administrators owner. Running elevated does not bypass the ACL check.

New application directories are created with an explicit current-user owner and
a protected DACL granting full control to those three principals. Child files
inherit those grants. Existing directories are never silently re-permissioned.
This avoids inheriting unrelated groups from a parent directory during first use.

## Repair an existing installation

Close every monitor TUI and stop its recorder first. Run the following from this
repository in PowerShell **as the Windows account that normally runs the monitor**.
If that account cannot change an Administrators-owned directory, use an elevated
PowerShell under the same account. Do not use another administrator account.

First inspect the directory (no writes):

```powershell
$statePath = Join-Path $env:LOCALAPPDATA 'codex-usage-monit'
& .\scripts\windows\repair-state-permissions.ps1 -Path $statePath
```

Then explicitly repair it, giving a new backup filename outside that directory:

```powershell
$backupPath = Join-Path $PWD ('monit-acl-backup-' + [Guid]::NewGuid().ToString('N') + '.json')
& .\scripts\windows\repair-state-permissions.ps1 -Path $statePath -Repair -BackupPath $backupPath
```

The script saves original owner/DACL information and SHA-256 hashes before any
ACL mutation. It sets the owner to the current user and protects every state
directory/file from parent ACL inheritance, retaining only user/SYSTEM/admin
grants. It verifies permissions and unchanged file contents afterwards. It
refuses profile/system roots, unrecognized application directories, links
(including junctions and hard links), unrelated owners, and an existing or
in-tree backup filename. The application must remain stopped throughout repair.

Keep the backup if a repair is interrupted; it identifies every original path
and ACL. Re-running with a new backup filename is supported after resolving the
reported problem. Do not delete or regenerate `source-identity.json` or
`source-identity.anchor` to fix permissions: their identity continuity matters
to local history and remote pairing. Only application directory ACLs are repaired;
the parent profile and SSH configuration are not edited.

Restart the monitor and check Diagnostics and Remote Sources. If a diagnostic
still names an untrusted SID, inspect the reported path: custom config and state
roots may be different directories.

## Regression verification

The native Windows Rust tests filtered by `windows_private_directory` create a
parent directory with an inheritable Everyone grant, then exercise source
identity, UI/Open config, cache, remotes, concurrent creation, diagnostics and
junction rejection. They also ensure existing public state is not changed.

Run the repair contracts once in Windows PowerShell 5.1 and once in PowerShell 7:

```powershell
powershell.exe -NoProfile -File .\scripts\windows\tests\repair-state-permissions.ps1
pwsh.exe -NoProfile -File .\scripts\windows\tests\repair-state-permissions.ps1
```

The normal full Windows verification pipeline includes the six repair contracts
in its current shell. A test process needs a private temporary directory, as the
general suite intentionally uses already-private fixtures. Record the actual
account, elevation, architecture and temporary-directory setup; a SYSTEM or
elevated run does not establish standard-user execution.
