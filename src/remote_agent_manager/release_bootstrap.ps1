# Prepare an official release on the SSH host. Never execute the downloaded file.
function Receive-ReleaseAsset {
    param([string]$Url, [string]$Destination, [long]$Maximum)
    # Windows PowerShell turns native stderr into ErrorRecords. Preserve curl's
    # exit/status output instead of terminating on its first diagnostic line.
    $ErrorActionPreference = 'Continue'
    $curlArgs = @('--fail', '--silent', '--show-error', '--location',
        '--proto', '=https', '--proto-redir', '=https', '--connect-timeout', '10',
        '--max-time', '120', '--max-filesize', "$Maximum", '--write-out', '%{http_code}',
        '--output', $Destination, $Url)
    $http = & curl.exe @curlArgs 2>&1
    if ($LASTEXITCODE -ne 0) {
        $kind = if (($http | Out-String) -match '(?m)^404\s*$') { 'agent_release_unavailable' } else { 'agent_release_download_failed' }
        throw "${kind}: $Url"
    }
    $ErrorActionPreference = 'Stop'
    if ((Get-Item -LiteralPath $Destination).Length -gt $Maximum) {
        throw 'agent_release_invalid: downloaded file exceeds size limit'
    }
}

function Invoke-ReleasePreparation {
    param($Expected, [string]$Stage)
    $ErrorActionPreference = 'Stop'
    if ($Stage -cnotmatch '^\.codex-usage-monit-release-[a-f0-9]{32}$' -or
        $Expected.version -cnotmatch '^[A-Za-z0-9.+-]{1,80}$' -or
        $Expected.target -cne 'x86_64-pc-windows-msvc') {
        throw 'agent_release_invalid: invalid release request'
    }
    $directory = Join-Path (Get-Location).Path $Stage
    if (Test-Path -LiteralPath $directory) { throw 'agent_release_invalid: staging directory already exists' }
    $sid = [Security.Principal.WindowsIdentity]::GetCurrent().User
    $acl = New-Object Security.AccessControl.DirectorySecurity
    $acl.SetOwner($sid)
    $acl.SetAccessRuleProtection($true, $false)
    $rule = New-Object Security.AccessControl.FileSystemAccessRule($sid, 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow')
    $acl.AddAccessRule($rule)
    if ($PSVersionTable.PSEdition -eq 'Desktop') {
        [IO.Directory]::CreateDirectory($directory, $acl) | Out-Null
    } else {
        [IO.FileSystemAclExtensions]::Create([IO.DirectoryInfo]::new($directory), $acl)
    }
    $success = $false
    try {
        $stem = "codex-usage-monit-$($Expected.target).agent"
        $base = "https://github.com/ghostroller/codex-usage-monit/releases/download/v$($Expected.version)/"
        $manifestPath = Join-Path $directory 'manifest.json'
        Receive-ReleaseAsset ($base + $stem + '.json') $manifestPath 32768
        try { $manifest = [IO.File]::ReadAllText($manifestPath) | ConvertFrom-Json }
        catch { throw 'agent_release_invalid: manifest is not valid JSON' }
        $fields = @($manifest.PSObject.Properties.Name | Sort-Object)
        if (($fields -join ',') -cne 'agent,file,schemaVersion,sha256,size' -or
            ($manifest.schemaVersion -isnot [int] -and $manifest.schemaVersion -isnot [long]) -or $manifest.schemaVersion -ne 1 -or
            $manifest.file -cne ($stem + '.exe') -or
            ($manifest.size -isnot [int] -and $manifest.size -isnot [long]) -or
            $manifest.size -le 0 -or $manifest.size -gt 134217728 -or
            $manifest.sha256 -isnot [string] -or $manifest.sha256 -cnotmatch '^[a-f0-9]{64}$') {
            throw 'agent_release_invalid: invalid official manifest'
        }
        foreach ($property in $Expected.PSObject.Properties) {
            $actual = $manifest.agent.($property.Name)
            if ($null -eq $actual -or $actual.GetType() -ne $property.Value.GetType() -or $actual -cne $property.Value) {
                throw 'agent_release_mismatch: official release does not match this center; unpublished development builds require explicit deploy-dev'
            }
        }
        $binary = Join-Path $directory 'agent.exe'
        Receive-ReleaseAsset ($base + $manifest.file) $binary $manifest.size
        $hasher = [Security.Cryptography.SHA256]::Create()
        $stream = [IO.File]::OpenRead($binary)
        try { $digest = ([BitConverter]::ToString($hasher.ComputeHash($stream))).Replace('-', '').ToLowerInvariant() }
        finally { $stream.Dispose(); $hasher.Dispose() }
        if ((Get-Item -LiteralPath $binary).Length -ne $manifest.size -or $digest -cne $manifest.sha256) {
            throw 'agent_checksum_mismatch: official binary differs from manifest'
        }
        $success = $true
        return $manifest
    } finally {
        if (-not $success) {
            foreach ($name in @('agent.exe', 'manifest.json')) {
                $file = Join-Path $directory $name
                if (Test-Path -LiteralPath $file) { Remove-Item -LiteralPath $file -Force -ErrorAction SilentlyContinue }
            }
            try { [IO.Directory]::Delete($directory, $false) } catch {}
        }
    }
}
