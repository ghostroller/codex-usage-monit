<#
.SYNOPSIS
Inspect or explicitly repair one codex-usage-monit state/config directory.
.DESCRIPTION
Run as the account that normally runs the monitor, with all monitor/recorder
processes stopped. The default is read-only. -Repair requires a new backup file
outside the target directory. Only owner and DACL change; file contents stay
intact. Links and unrelated owners are rejected before any permissions change.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Path,
    [switch]$Repair,
    [string]$BackupPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Assert-NoReparseAncestor([string]$ItemPath) {
    $cursor = $ItemPath
    while (-not [string]::IsNullOrEmpty($cursor)) {
        $item = Get-Item -LiteralPath $cursor -Force
        if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "Refusing a reparse point: $cursor"
        }
        $parent = [IO.Directory]::GetParent($cursor)
        if ($null -eq $parent) { break }
        $cursor = $parent.FullName
    }
}

$root = [IO.Path]::GetFullPath($Path).TrimEnd('\', '/')
$rootItem = Get-Item -LiteralPath $root -Force
if (-not $rootItem.PSIsContainer) { throw 'The target must be a directory.' }
Assert-NoReparseAncestor $root
$rootPrefix = $root + [IO.Path]::DirectorySeparatorChar
$protectedLocations = @(
    [IO.Path]::GetPathRoot($root), $env:USERPROFILE, $env:LOCALAPPDATA,
    $env:APPDATA, $env:SystemRoot, (Join-Path $env:USERPROFILE '.ssh')
)
foreach ($location in $protectedLocations) {
    if ([string]::IsNullOrEmpty($location)) { continue }
    $location = [IO.Path]::GetFullPath($location).TrimEnd('\', '/')
    if ($root.Equals($location, [StringComparison]::OrdinalIgnoreCase) -or
        $location.StartsWith($rootPrefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Refusing a system/profile directory or its ancestor: $root"
    }
}
foreach ($subtree in @($env:SystemRoot, (Join-Path $env:USERPROFILE '.ssh'))) {
    if ([string]::IsNullOrEmpty($subtree)) { continue }
    $prefix = [IO.Path]::GetFullPath($subtree).TrimEnd('\', '/') + '\'
    if ($root.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Refusing a system/SSH subtree: $root"
    }
}
$recognized = $rootItem.Name -eq 'codex-usage-monit'
foreach ($marker in @('source-identity.json', 'remotes.json', 'history-v1', 'tui-state.json', 'open.json')) {
    if (Test-Path -LiteralPath (Join-Path $root $marker)) { $recognized = $true }
}
if (-not $recognized) { throw 'The target has no recognizable codex-usage-monit state/config layout.' }
if ($Repair -and @(Get-Process -Name codex-usage-monit -ErrorAction SilentlyContinue).Count -gt 0) {
    throw 'Stop the monitor TUI and recorder before repairing permissions.'
}

$userSid = [Security.Principal.WindowsIdentity]::GetCurrent().User
$allowedOwners = @($userSid.Value, 'S-1-5-32-544')
$sections = [Security.AccessControl.AccessControlSections]::Owner -bor [Security.AccessControl.AccessControlSections]::Access
$pending = New-Object 'System.Collections.Generic.Stack[string]'
$entries = New-Object 'System.Collections.Generic.List[object]'
$pending.Push($root)
while ($pending.Count -gt 0) {
    $itemPath = $pending.Pop()
    $item = Get-Item -LiteralPath $itemPath -Force
    if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0 -or $item.LinkType -eq 'HardLink') {
        throw "Refusing a link: $itemPath"
    }
    $acl = Get-Acl -LiteralPath $itemPath
    $ownerSid = $acl.GetOwner([Security.Principal.SecurityIdentifier]).Value
    if ($Repair -and $ownerSid -notin $allowedOwners) {
        throw "Refusing to take ownership from $ownerSid at $itemPath"
    }
    $entry = [ordered]@{
        path = $itemPath
        directory = [bool]$item.PSIsContainer
        owner = $ownerSid
        sddl = $acl.GetSecurityDescriptorSddlForm($sections)
        sha256 = $null
    }
    if ($item.PSIsContainer) {
        foreach ($child in @(Get-ChildItem -LiteralPath $itemPath -Force)) {
            $pending.Push($child.FullName)
        }
    } elseif ($Repair) {
        $entry.sha256 = (Get-FileHash -LiteralPath $itemPath -Algorithm SHA256).Hash
    }
    $entries.Add([pscustomobject]$entry)
}

if (-not $Repair) {
    [pscustomobject]@{ root = $root; currentUserSid = $userSid.Value; entries = @($entries.ToArray()) } |
        ConvertTo-Json -Depth 5
    return
}
if ([string]::IsNullOrWhiteSpace($BackupPath)) { throw '-Repair requires -BackupPath outside the target directory.' }
$backup = [IO.Path]::GetFullPath($BackupPath)
if ($backup.Equals($root, [StringComparison]::OrdinalIgnoreCase) -or
    $backup.StartsWith($rootPrefix, [StringComparison]::OrdinalIgnoreCase)) {
    throw 'The ACL backup must be outside the target directory.'
}
Assert-NoReparseAncestor ([IO.Path]::GetDirectoryName($backup))
$record = [pscustomobject]@{
    version = 1; root = $root; userSid = $userSid.Value
    createdAtUtc = [DateTime]::UtcNow.ToString('o'); entries = @($entries.ToArray())
}
$bytes = [Text.Encoding]::UTF8.GetBytes(($record | ConvertTo-Json -Depth 5))
$stream = [IO.File]::Open($backup, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
try { $stream.Write($bytes, 0, $bytes.Length); $stream.Flush($true) } finally { $stream.Dispose() }

foreach ($entry in $entries) {
    Assert-NoReparseAncestor $entry.path
    $item = Get-Item -LiteralPath $entry.path -Force
    if ($item.LinkType -eq 'HardLink' -or [bool]$item.PSIsContainer -ne $entry.directory) {
        throw "The target changed during repair: $($entry.path). Backup: $backup"
    }
    $acl = Get-Acl -LiteralPath $entry.path
    if ($acl.GetOwner([Security.Principal.SecurityIdentifier]).Value -notin $allowedOwners) {
        throw "The owner changed during repair: $($entry.path). Backup: $backup"
    }
    $flags = if ($entry.directory) { 'OICI' } else { '' }
    $sddl = "O:$($userSid.Value)D:P(A;${flags};FA;;;$($userSid.Value))(A;${flags};FA;;;SY)(A;${flags};FA;;;BA)"
    $acl.SetSecurityDescriptorSddlForm($sddl, $sections)
    Set-Acl -LiteralPath $entry.path -AclObject $acl
}
foreach ($entry in $entries) {
    $acl = Get-Acl -LiteralPath $entry.path
    if ($acl.GetOwner([Security.Principal.SecurityIdentifier]).Value -ne $userSid.Value -or -not $acl.AreAccessRulesProtected) {
        throw "Owner/protection verification failed: $($entry.path). Backup: $backup"
    }
    foreach ($rule in $acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier])) {
        if ($rule.IdentityReference.Value -notin @($userSid.Value, 'S-1-5-18', 'S-1-5-32-544')) {
            throw "Unexpected ACL trustee at $($entry.path). Backup: $backup"
        }
    }
    if (-not $entry.directory -and (Get-FileHash -LiteralPath $entry.path -Algorithm SHA256).Hash -ne $entry.sha256) {
        throw "File contents changed during repair: $($entry.path). Backup: $backup"
    }
}
[pscustomobject]@{ repairedRoot = $root; entries = $entries.Count; owner = $userSid.Value; backup = $backup; contentsUnchanged = $true } |
    ConvertTo-Json
