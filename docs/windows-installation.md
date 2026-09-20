# Windows installation and background recording

The default installation belongs to the current Windows user. It does not need
administrator access. The optional desktop recorder is a Task Scheduler task for
that same user; it requires an interactive login. A separate, explicitly configured
machine service supports unattended servers.

The prebuilt Windows executable targets x64. Running it through Windows ARM64 x64
emulation is different from a native ARM64 release. The commands below describe
the current implementation, not evidence that an installation, reboot, or service
account has been validated on your machine. Platform evidence is tracked through
the [testing workflow](testing.md) and the
[implementation verification record](windows-lifecycle-verification-20260921.md).

## Install for your user

The download commands require a release containing this installer and the new
installation commands. Until that release is published, use this checkout's
`scripts/install.ps1` with a matching verified local release bundle. An older
release executable cannot run the new installer commands.

In Windows PowerShell 5.1 or PowerShell 7:

```powershell
Invoke-WebRequest -UseBasicParsing -Uri 'https://github.com/ghostroller/codex-usage-monit/releases/latest/download/install.ps1' -OutFile .\install.ps1
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\install.ps1
& "$env:LOCALAPPDATA\codex-usage-monit\bin\codex-usage-monit.exe" --version
```

The script downloads and verifies the release manifest and executable before
invoking the common installer. It registers the command directory in the user
PATH and records the entries it owns. It does not create a recorder by default.
Reopen the terminal application to inherit the new PATH; opening a new tab in an
existing terminal host may retain its previous environment.

Useful script options:

```powershell
# Replace X.Y.Z with the required published version.
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\install.ps1 -Version X.Y.Z

# Install a recorder for the currently logged-in user.
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\install.ps1 -Recorder on-logon

# Create the managed command entry without changing the user PATH.
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\install.ps1 -NoModifyPath

# A local release directory containing release-manifest.json and the Windows exe.
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\install.ps1 -Bundle 'D:\release-bundle'
```

For a checksum-verified executable already downloaded from a release, its public
`install` command can perform the same installation. `install --version X.Y.Z`
selects that published version; `install --bundle-dir DIR` uses a local bundle.
Only use `--adopt` (or the script's `-Adopt`) when intentionally adopting an existing
standalone command. Cargo, Scoop, Chocolatey, and other externally owned installs
remain the responsibility of their original manager.

The user installation contains:

```text
%LOCALAPPDATA%\codex-usage-monit\
  bin\codex-usage-monit.exe       stable command launcher
  versions\<version>-<sha>\      immutable business executable
  installation.json             compatible launcher selection
  install-receipt.json           user/SID/PATH/task ownership
  update-journal.json            resumable update state
```

When a desktop recorder is registered, its immutable version directory also
contains `recorder-host.exe` and `windows-components.json`. The host is extracted
from the verified main executable; it is not a second network download. Do not
manually edit the version selection, component metadata, or recovery journals.
Custom Codex, history, and configuration paths retain their existing locations.

### Build a signed User Setup

The repository includes an optional Inno Setup wrapper in
[`scripts/windows/build-user-setup.ps1`](../scripts/windows/build-user-setup.ps1)
and [`user-setup.iss`](../scripts/windows/user-setup.iss). It invokes the same Rust
installer, including the user PATH and optional on-logon recorder choices. This
build entry does not mean a signed setup is already available in published releases.

Building it requires Inno Setup 6 (`ISCC.exe`), Windows SDK `signtool.exe`, an
accessible code-signing certificate and private key in `CurrentUser\My`, and an
HTTPS RFC 3161 timestamp service. The payload must already have a valid Authenticode
signature from that certificate:

```powershell
.\scripts\windows\build-user-setup.ps1 `
    -Binary 'D:\release\codex-usage-monit-x86_64-pc-windows-msvc.exe' `
    -CertificateThumbprint '<40-hex-character certificate thumbprint>' `
    -TimestampServer 'https://your-publisher-timestamp-service.example' `
    -Iscc 'C:\Program Files (x86)\Inno Setup 6\ISCC.exe' `
    -SignTool 'C:\path\to\Windows SDK\signtool.exe' `
    -OutputDirectory 'D:\release\user-setup'
```

Sign and verify the application before generating its release manifest and
checksums: signing changes the executable bytes. The setup builder then signs and
verifies the wrapper before writing `user-setup-SHA256SUMS`; it does not modify an
existing release manifest or publish files. A real signed setup build and install
still require the provisioned tools and publisher certificate; they have not been
validated by compilation or the shell contract tests.

## Check, update, repair, and uninstall

```powershell
codex-usage-monit doctor --format json
Get-Command codex-usage-monit -All
where.exe codex-usage-monit
codex-usage-monit update
codex-usage-monit update status --format json
codex-usage-monit install repair
codex-usage-monit uninstall
```

`doctor` separates persistent PATH registration from the current process's PATH.
A PowerShell alias/function, an older `.cmd` wrapper, or a system PATH entry may
still take precedence; the shell commands above show the actual resolution.
Repair restores the owned command/PATH registration without replacing unrelated
PATH entries. It does not rewrite the complete user PATH from a process environment.

Updates use the same executor locally and over SSH. The default node update also
updates an existing recorder and verifies its fresh heartbeat. It preserves the
recorder's collection options, data locations, and enabled state. It does not
create a recorder that was never installed. Close and reopen existing TUIs after
updating. A first migration from an old executable may remain pending while that
entry is in use; inspect the update report and resume the retained transaction
after closing the named old processes. Pending or partial is not a completed
node update.

Uninstall removes owned command/PATH/uninstall registration and the owned
background task. History, configuration, and stored versions are retained. If an
in-use launcher prevents its removal, close old sessions and run `uninstall` again
from the immutable executable reported by `doctor`. A changed task definition or
unknown installation owner stops automatic removal instead of guessing ownership.

The installer owns a recorder only when it was registered with `-Recorder on-logon`
or `install --recorder on-logon` (including `install repair`). A recorder created
separately with `service install` has its own lifecycle: run `service uninstall`
before uninstalling the user command if you also want to remove that task.

Failed updates recover forward. Do not point the recorder back to an old version
after a newer writer may have migrated its data. See [application updates](remote-updates.md)
for update scopes, journals, and explicit reference-aware version pruning.

## Desktop recorder

Register while logged in as the intended user:

```powershell
codex-usage-monit service install --format json
codex-usage-monit service status --format json
codex-usage-monit service stop
codex-usage-monit service start
codex-usage-monit service restart
codex-usage-monit service repair
codex-usage-monit service uninstall
```

For online collection, Codex must be available for that user. To select a particular
Codex executable, put `--codex-bin 'C:\absolute\path\codex.exe'` before `service`.
For local history-only collection, use `codex-usage-monit --offline service install`.
Other collection options, including `--codex-home`, likewise precede `service`.

| Operation | Effect |
| --- | --- |
| `install` | Register the immutable recorder and start it with the chosen collection options. |
| `start` | Enable automatic starts and start the registered recorder now. |
| `stop` | Disable automatic starts and cooperatively stop the recorder. |
| `restart` | Restart an enabled recorder; a disabled registration stays disabled. |
| `repair` | Resume forward recovery using the current candidate, preserving recorded options and enablement. |
| `uninstall` | Remove the owned task while preserving history. |

The task is separated by user SID and runs with least privilege. Its version-bound
GUI host starts the recorder without a console, closes stdin, directs output to
the reported host log, and owns the child process tree. Battery operation, unlimited
run time, and restart-on-failure remain enabled. A stable CLI launcher is not the
task's writer selection: the task binds a particular immutable version.

The JSON status includes manager state, enablement, user SID, interactive-session
availability, heartbeat health, last task result, and the host log path. A running
task without a matching fresh heartbeat is not a healthy recorder. An enabled task
may report `waiting_for_logon` when its account lacks an interactive session.
Installation and upgrades that require an immediate start preflight that condition
before replacing the recorder. Locking the desktop or disconnecting RDP is not the
same as logging out; validate the relevant session policy on the target machine.

SSH exporter commands can run during an SSH session without a desktop recorder.
That does not make an `InteractiveToken` task usable on a server where nobody is
logged in. Use the separate machine mode for unattended recording.

## Unattended machine service

Machine service management is an explicit administrator operation and does not use
the installing administrator's user installation or collection configuration.
Choose a named local or domain service account, such as `MYPC\monit-recorder`;
built-in accounts such as LocalSystem are rejected. The chosen account needs a prepared profile, the
service-logon right, and access to its explicit data paths. Ensure its Codex auth,
DPAPI-protected material, SSH keys/agent, and remote configuration work under that
account. The installer does not copy another user's credentials or migrate history.
It does not create the account or grant its service-logon right. Existing runtime
directories must be owned by that account's SID and satisfy the private ACL
checks; merely granting write access to an administrator-owned directory is not
enough. Missing runtime directories are created under the service account's token
inside its prepared private paths.

Use a verified executable from an elevated PowerShell session. This example uses
offline collection, so no Codex executable is required. Replace the account and
paths with ones already prepared for the service account:

```powershell
$adminBinary = (Resolve-Path '.\codex-usage-monit-x86_64-pc-windows-msvc.exe').Path
$serviceAccount = "$env:COMPUTERNAME\monit-recorder"
$credential = Get-Credential -UserName $serviceAccount -Message 'Recorder service account'
$previousOutputEncoding = $OutputEncoding
try {
    # PowerShell 5.1 must send non-ASCII passwords to native stdin as UTF-8.
    $OutputEncoding = [Text.UTF8Encoding]::new($false)
    $credential.GetNetworkCredential().Password | & $adminBinary service machine install `
        --name 'CodexUsageMonitRecorderServer' `
        --account $serviceAccount --password-stdin `
        --codex-home 'C:\Users\monit-recorder\.codex' `
        --history-dir 'C:\Users\monit-recorder\AppData\Local\monit-server\history-v1' `
        --status-file 'C:\Users\monit-recorder\AppData\Local\monit-server\recorder-status.json' `
        --config-dir 'C:\Users\monit-recorder\AppData\Local\monit-server\config' `
        --offline --format json
} finally {
    $OutputEncoding = $previousOutputEncoding
    $credential = $null
}
```

Passwords are accepted through stdin, not command-line arguments or environment
variables. For online collection omit `--offline` and explicitly supply
`--codex-bin 'C:\absolute\path\codex.exe'`. Use `--environment-path` only when the
service needs a deliberately chosen executable search path; the administrator's
PATH is not copied. The Codex executable and its path must not be writable or
replaceable by unrelated accounts; installation and service startup verify these
ACLs. A service-account-owned installation is supported. Remote and project configuration can be selected explicitly
with `--remotes-config-file` and `--project-mapping-file`.

The machine store is under the Windows Program Files known folder at
`codex-usage-monit-machine\<name>`. Program files remain administrator-controlled;
the service account's state follows its configured ACLs. A privileged service does
not execute a user-writable LocalAppData launcher. User `update --scope node` does
not update this machine registration.

Installation normally enables automatic startup and waits for the recorder's fresh
heartbeat. `--disabled` registers without starting or enabling automatic startup.
SCM restarts unexpected failures after 30, 60, and 120 seconds; an orderly stop
with exit code zero does not trigger failure recovery.
Subsequent operations require the explicit service name:

```powershell
& $adminBinary service machine status --name 'CodexUsageMonitRecorderServer' --format json
& $adminBinary service machine stop --name 'CodexUsageMonitRecorderServer'
& $adminBinary service machine start --name 'CodexUsageMonitRecorderServer'
& $adminBinary service machine restart --name 'CodexUsageMonitRecorderServer'
# Invoke the verified new executable for an upgrade; retain the service account and data paths.
& $adminBinary service machine upgrade --name 'CodexUsageMonitRecorderServer'
& $adminBinary service machine uninstall --name 'CodexUsageMonitRecorderServer'
```

Machine `stop`, `start`, and `restart` change the current running state while
preserving the configured startup type. `--disabled` selects demand startup at
installation. Uninstall removes the SCM registration and preserves data and
retained machine files. Inspect any
partial result before retrying; a manager operation alone does not prove that the
recorder has the correct identity or a fresh heartbeat. Account changes, password
rotation, organization policy, and reboot-without-login behavior need native
acceptance under the intended account. Compilation or SYSTEM test fixtures do not
establish that acceptance.
