[CmdletBinding()]
param([string]$RepairScript)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ([string]::IsNullOrEmpty($RepairScript)) {
    $RepairScript = Join-Path $PSScriptRoot '..\repair-state-permissions.ps1'
}
$RepairScript = [IO.Path]::GetFullPath($RepairScript)
$temporaryParent = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\', '/')
$systemDirectory = [IO.Path]::GetFullPath($env:SystemRoot).TrimEnd('\', '/')
# SYSTEM's GetTempPath can ignore TEMP/TMP and select Windows\SystemTemp.
# Keep the production refusal intact; only our unique test fixtures move.
if ($temporaryParent.Equals($systemDirectory, [StringComparison]::OrdinalIgnoreCase) -or
    $temporaryParent.StartsWith($systemDirectory + '\', [StringComparison]::OrdinalIgnoreCase)) {
    $temporaryParent = [Environment]::GetFolderPath('CommonApplicationData')
}
if ([string]::IsNullOrWhiteSpace($temporaryParent)) { throw 'No safe test temporary parent is available.' }
$temporaryParent = [IO.Path]::GetFullPath($temporaryParent).TrimEnd('\', '/')
$root = Join-Path $temporaryParent ('monit-acl-contracts-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $root | Out-Null
$userSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$rootAcl = Get-Acl -LiteralPath $root
$rootAcl.SetSecurityDescriptorSddlForm("O:${userSid}D:P(A;OICI;FA;;;${userSid})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)")
Set-Acl -LiteralPath $root -AclObject $rootAcl
Write-Host "Permission-repair fixture root: $root; user $userSid"
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
        $expectedSids = @($userSid, 'S-1-5-18', 'S-1-5-32-544' | Sort-Object -Unique)
        $rules = @($acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier]))
        $actualSids = @($rules | ForEach-Object { $_.IdentityReference.Value } | Sort-Object -Unique)
        Assert (($actualSids -join ',') -eq ($expectedSids -join ',')) 'Unexpected ACL trustee set.'
        foreach ($rule in $rules) {
            Assert ($rule.AccessControlType -eq 'Allow' -and -not $rule.IsInherited -and
                $rule.FileSystemRights -eq [Security.AccessControl.FileSystemRights]::FullControl) 'Expected explicit full-control grants.'
        }
    }
    $count++

    $fixture = New-Fixture 'relative [test]'
    $processDirectory = Join-Path $root 'process-cwd'
    New-Item -ItemType Directory -Path $processDirectory | Out-Null
    $previousProcessDirectory = [Environment]::CurrentDirectory
    Push-Location -LiteralPath $root
    try {
        [Environment]::CurrentDirectory = $processDirectory
        $inspection = (& $RepairScript -Path '.\relative [test]' | Out-String) | ConvertFrom-Json
        Assert ($inspection.root -eq $fixture) 'Relative inspection ignored the PowerShell location.'
        $count++
        $relativeBackup = Join-Path $root 'relative-backup.json'
        $result = (& $RepairScript -Path '.\relative [test]' -Repair -BackupPath '.\relative-backup.json' | Out-String) | ConvertFrom-Json
        Assert ($result.repairedRoot -eq $fixture -and $result.backup -eq $relativeBackup -and
            $result.contentsUnchanged -and (Test-Path -LiteralPath $relativeBackup)) 'Relative repair or backup used the process directory.'
        Assert (-not (Test-Path -LiteralPath (Join-Path $processDirectory 'relative-backup.json'))) 'Backup appeared in the wrong directory.'
        $count++
    } finally {
        [Environment]::CurrentDirectory = $previousProcessDirectory
        Pop-Location
    }

    Expect-Failure { & $RepairScript -Path 'Env:\' } 'FileSystem provider'
    Expect-Failure { & $RepairScript -Path $fixture -Repair -BackupPath 'Env:\CODEX_REPAIR_BACKUP' } 'FileSystem provider'
    $count++
    Expect-Failure { & $RepairScript -Path ([IO.Path]::GetPathRoot($root)) } 'system/profile'
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
    if (-not ([IO.Path]::GetDirectoryName($resolved)).Equals($temporaryParent, [StringComparison]::OrdinalIgnoreCase) -or
        [IO.Path]::GetFileName($resolved) -notlike 'monit-acl-contracts-*' -or
        ((Get-Item -LiteralPath $resolved -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "Unsafe contract cleanup target: $resolved"
    }
    Remove-Item -LiteralPath $resolved -Recurse -Force
}
