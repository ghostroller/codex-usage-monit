[CmdletBinding()]
param([string]$RepairScript)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ([string]::IsNullOrEmpty($RepairScript)) {
    $RepairScript = Join-Path $PSScriptRoot '..\repair-state-permissions.ps1'
}
$RepairScript = [IO.Path]::GetFullPath($RepairScript)
$root = Join-Path ([IO.Path]::GetTempPath()) ('monit-acl-contracts-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $root | Out-Null
$userSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$count = 0

function New-Fixture([string]$Name) {
    $path = Join-Path $root $Name
    New-Item -ItemType Directory -Path $path | Out-Null
    $acl = Get-Acl -LiteralPath $path
    $acl.SetSecurityDescriptorSddlForm("O:${userSid}D:P(A;OICI;FA;;;${userSid})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;GR;;;WD)")
    Set-Acl -LiteralPath $path -AclObject $acl
    [IO.File]::WriteAllText((Join-Path $path 'tui-state.json'), '{"keep":"unchanged"}')
    return $path
}
function Assert([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}
function Expect-Failure([scriptblock]$Action, [string]$Contains) {
    $caught = $false
    try { & $Action | Out-Null } catch {
        $caught = $true
        Assert ($_.Exception.Message.Contains($Contains)) "Unexpected error: $_"
    }
    Assert $caught "Expected refusal containing: $Contains"
}

try {
    $fixture = New-Fixture 'inspect'
    $before = (Get-Acl -LiteralPath $fixture).Sddl
    $inspection = (& $RepairScript -Path $fixture | Out-String) | ConvertFrom-Json
    Assert ($inspection.entries.Count -eq 2) 'Inspection must include the root and file.'
    Assert ((Get-Acl -LiteralPath $fixture).Sddl -eq $before) 'Inspection changed permissions.'
    $count++

    $fixture = New-Fixture 'repair'
    $nested = Join-Path $fixture 'history-v1'
    New-Item -ItemType Directory -Path $nested | Out-Null
    [IO.File]::WriteAllText((Join-Path $nested 'history.json'), '{"history":"preserved"}')
    [IO.File]::WriteAllText((Join-Path $fixture 'source-identity.json'), '{"identity":"preserved"}')
    [IO.File]::WriteAllText((Join-Path $fixture 'source-identity.anchor'), 'anchor-preserved')
    $backup = Join-Path $root 'original-acls.json'
    $result = (& $RepairScript -Path $fixture -Repair -BackupPath $backup | Out-String) | ConvertFrom-Json
    Assert ($result.contentsUnchanged -and $result.entries -eq 6) 'Repair did not verify all original files.'
    $record = Get-Content -LiteralPath $backup -Raw | ConvertFrom-Json
    Assert ($record.entries[0].sddl.Contains(';;;WD)')) 'Backup did not preserve the original public DACL.'
    foreach ($entry in $record.entries) {
        $acl = Get-Acl -LiteralPath $entry.path
        Assert $acl.AreAccessRulesProtected "Inheritance remains enabled: $($entry.path)"
        Assert ($acl.GetOwner([Security.Principal.SecurityIdentifier]).Value -eq $userSid) 'Owner mismatch.'
        Assert ($acl.Access.Count -eq 3) 'Expected exactly user, SYSTEM and Administrators grants.'
    }
    $count++

    $fixture = New-Fixture 'backup-refusal'
    $before = (Get-Acl -LiteralPath $fixture).Sddl
    Expect-Failure { & $RepairScript -Path $fixture -Repair -BackupPath (Join-Path $fixture 'backup.json') } 'outside'
    Expect-Failure { & $RepairScript -Path $fixture -Repair -BackupPath $backup } ''
    Assert ((Get-Acl -LiteralPath $fixture).Sddl -eq $before) 'Failed backup modified permissions.'
    $count++

    Expect-Failure { & $RepairScript -Path $env:USERPROFILE -Repair -BackupPath (Join-Path $root 'profile.json') } 'system/profile'
    $fakeSystemRoot = Join-Path $root 'system'
    New-Item -ItemType Directory -Path $fakeSystemRoot | Out-Null
    $systemChild = Join-Path $fakeSystemRoot 'codex-usage-monit'
    New-Item -ItemType Directory -Path $systemChild | Out-Null
    $previousSystemRoot = $env:SystemRoot
    try {
        $env:SystemRoot = $fakeSystemRoot
        Expect-Failure { & $RepairScript -Path $systemChild } 'system/SSH subtree'
    } finally { $env:SystemRoot = $previousSystemRoot }
    $count++

    $fixture = New-Fixture 'junction-refusal'
    $outside = Join-Path $root 'outside'
    New-Item -ItemType Directory -Path $outside | Out-Null
    $outsideAcl = (Get-Acl -LiteralPath $outside).Sddl
    $junction = Join-Path $fixture 'linked'
    New-Item -ItemType Junction -Path $junction -Target $outside | Out-Null
    try {
        Expect-Failure { & $RepairScript -Path $fixture -Repair -BackupPath (Join-Path $root 'junction.json') } 'link'
        Assert ((Get-Acl -LiteralPath $outside).Sddl -eq $outsideAcl) 'Junction target was modified.'
    } finally {
        # Remove the junction itself; do not recurse into its destination.
        [IO.Directory]::Delete($junction)
    }
    $count++

    $fixture = New-Fixture 'hardlink-refusal'
    $original = Join-Path $root 'original.txt'
    [IO.File]::WriteAllText($original, 'outside data')
    $originalAcl = (Get-Acl -LiteralPath $original).Sddl
    New-Item -ItemType HardLink -Path (Join-Path $fixture 'hardlink.txt') -Target $original | Out-Null
    Expect-Failure { & $RepairScript -Path $fixture -Repair -BackupPath (Join-Path $root 'hardlink.json') } 'link'
    Assert ((Get-Acl -LiteralPath $original).Sddl -eq $originalAcl) 'Hard-link target was modified.'
    $count++
    Write-Host "PASS: $count permission-repair contracts; PowerShell $($PSVersionTable.PSVersion); user $userSid"
} finally {
    # Verify the exact, unique temporary root before recursive cleanup.
    $resolved = [IO.Path]::GetFullPath($root)
    $tempPrefix = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\') + '\'
    if (-not $resolved.StartsWith($tempPrefix, [StringComparison]::OrdinalIgnoreCase) -or
        [IO.Path]::GetFileName($resolved) -notlike 'monit-acl-contracts-*') {
        throw "Unsafe contract cleanup target: $resolved"
    }
    Remove-Item -LiteralPath $resolved -Recurse -Force
}
