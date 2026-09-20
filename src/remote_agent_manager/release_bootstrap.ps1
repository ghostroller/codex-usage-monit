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
        $base = if ($Expected.version -ceq 'latest') {
            'https://github.com/ghostroller/codex-usage-monit/releases/latest/download/'
        } else {
            "https://github.com/ghostroller/codex-usage-monit/releases/download/v$($Expected.version)/"
        }
        $manifestPath = Join-Path $directory 'manifest.json'
        Receive-ReleaseAsset ($base + 'release-manifest.json') $manifestPath 32768
        try { $manifest = [IO.File]::ReadAllText($manifestPath) | ConvertFrom-Json }
        catch { throw 'agent_release_invalid: manifest is not valid JSON' }
        $fields = @($manifest.PSObject.Properties.Name | Sort-Object)
        if (($fields -join ',') -cne 'artifacts,buildId,product,protocolVersion,schemaVersion,version' -or
            ($manifest.schemaVersion -isnot [int] -and $manifest.schemaVersion -isnot [long]) -or $manifest.schemaVersion -ne 1 -or
            $manifest.product -cne 'codex-usage-monit' -or
            $manifest.version -isnot [string] -or $manifest.version -cnotmatch '^[0-9]+\.[0-9]+\.[0-9]+(?:[-+][A-Za-z0-9.+-]+)?$' -or
            $manifest.buildId -isnot [string] -or $manifest.buildId -cnotmatch '^[a-f0-9]{64}$' -or
            ($manifest.protocolVersion -isnot [int] -and $manifest.protocolVersion -isnot [long]) -or $manifest.protocolVersion -lt 1 -or
            $manifest.artifacts -isnot [array] -or $manifest.artifacts.Count -lt 1 -or $manifest.artifacts.Count -gt 5) {
            throw 'agent_release_invalid: invalid official manifest'
        }
        $seen = @{}
        $selected = $null
        foreach ($artifact in $manifest.artifacts) {
            $names = @($artifact.PSObject.Properties.Name | Sort-Object)
            if (($names -join ',') -cne 'binarySha256,binarySize,file,sha256,size,target' -or
                $artifact.target -cnotin @('x86_64-pc-windows-msvc', 'x86_64-apple-darwin', 'aarch64-apple-darwin', 'x86_64-unknown-linux-musl', 'aarch64-unknown-linux-musl') -or
                $seen.ContainsKey($artifact.target)) {
                throw 'agent_release_invalid: invalid release artifact'
            }
            $seen[$artifact.target] = $true
            $suffix = if ($artifact.target -ceq 'x86_64-pc-windows-msvc') { '.exe' } else { '.tar.gz' }
            if ($artifact.file -cne ("codex-usage-monit-$($artifact.target)" + $suffix)) {
                throw 'agent_release_invalid: invalid artifact filename'
            }
            foreach ($key in @('size', 'binarySize')) {
                $value = $artifact.$key
                if (($value -isnot [int] -and $value -isnot [long]) -or $value -le 0 -or $value -gt 134217728) {
                    throw 'agent_release_invalid: invalid artifact size'
                }
            }
            foreach ($key in @('sha256', 'binarySha256')) {
                if ($artifact.$key -isnot [string] -or $artifact.$key -cnotmatch '^[a-f0-9]{64}$') {
                    throw 'agent_release_invalid: invalid artifact checksum'
                }
            }
            if ($artifact.target -ceq $Expected.target) { $selected = $artifact }
        }
        if ($null -eq $selected) { throw 'agent_release_mismatch: release lacks the requested platform' }
        $agent = [pscustomobject]@{schemaVersion=$manifest.schemaVersion;product=$manifest.product;
            version=$manifest.version;buildId=$manifest.buildId;protocolVersion=$manifest.protocolVersion;target=$selected.target}
        foreach ($property in $Expected.PSObject.Properties) {
            if ($property.Name -ceq 'version' -and $property.Value -ceq 'latest') { continue }
            $actual = $agent.($property.Name)
            if ($null -eq $actual -or $actual.GetType() -ne $property.Value.GetType() -or $actual -cne $property.Value) {
                throw 'agent_release_mismatch: official release does not match this center; unpublished development builds require explicit deploy-dev'
            }
        }
        if ($selected.size -ne $selected.binarySize -or $selected.sha256 -cne $selected.binarySha256) {
            throw 'agent_release_invalid: executable metadata disagrees'
        }
        $base = "https://github.com/ghostroller/codex-usage-monit/releases/download/v$($agent.version)/"
        $binary = Join-Path $directory 'agent.exe'
        Receive-ReleaseAsset ($base + $selected.file) $binary $selected.size
        $hasher = [Security.Cryptography.SHA256]::Create()
        $stream = [IO.File]::OpenRead($binary)
        try { $digest = ([BitConverter]::ToString($hasher.ComputeHash($stream))).Replace('-', '').ToLowerInvariant() }
        finally { $stream.Dispose(); $hasher.Dispose() }
        if ((Get-Item -LiteralPath $binary).Length -ne $selected.size -or $digest -cne $selected.sha256) {
            throw 'agent_checksum_mismatch: official binary differs from manifest'
        }
        $normalized = [pscustomobject]@{schemaVersion=1;agent=$agent;file=$selected.file;size=$selected.binarySize;sha256=$selected.binarySha256}
        $success = $true
        return $normalized
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
