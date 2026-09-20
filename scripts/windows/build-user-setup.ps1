<#
.SYNOPSIS
Builds and signs a per-user setup from an already signed release executable.
.DESCRIPTION
Requires Inno Setup 6, Windows SDK SignTool, and a provisioned code-signing
certificate in CurrentUser\My. No key/password is accepted in command arguments.
Sign the application BEFORE package-release.py computes its final hash/manifest.
This script never publishes a release and never rewrites an existing manifest.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Binary,
    [Parameter(Mandatory = $true)][ValidatePattern('^[0-9A-Fa-f]{40}$')][string]$CertificateThumbprint,
    [Parameter(Mandatory = $true)][uri]$TimestampServer,
    [Parameter(Mandatory = $true)][string]$Iscc,
    [Parameter(Mandatory = $true)][string]$SignTool,
    [Parameter(Mandatory = $true)][string]$OutputDirectory
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false

if ($TimestampServer.Scheme -ne 'https') { throw 'Use an HTTPS RFC 3161 timestamp server.' }
$payload = (Resolve-Path -LiteralPath $Binary).Path
$compiler = (Resolve-Path -LiteralPath $Iscc).Path
$signer = (Resolve-Path -LiteralPath $SignTool).Path
$signature = Get-AuthenticodeSignature -LiteralPath $payload
if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Thumbprint -ne $CertificateThumbprint) {
    throw 'The payload must have a valid Authenticode signature from the selected publisher before setup is built.'
}
$identityJson = & $payload remote-agent info --sha256
if ($LASTEXITCODE -ne 0) { throw 'Could not read the signed release identity.' }
$identity = $identityJson | ConvertFrom-Json
if ($identity.product -ne 'codex-usage-monit' -or $identity.target -ne 'x86_64-pc-windows-msvc' -or
    $identity.version -notmatch '^\d+\.\d+\.\d+([+-][0-9A-Za-z.-]+)?$' -or $identity.buildId -notmatch '^[0-9a-f]{64}$') {
    throw 'The payload is not a supported Windows x64 release.'
}
New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
$output = (Resolve-Path -LiteralPath $OutputDirectory).Path
$setup = Join-Path $output 'codex-usage-monit-user-setup-x64.exe'
if (Test-Path -LiteralPath $setup) { throw 'Use an empty output directory; refusing to overwrite an existing setup.' }
& $compiler "/DPayload=$payload" "/DAppVersion=$($identity.version)" "/DOutputPath=$output" (Join-Path $PSScriptRoot 'user-setup.iss')
if ($LASTEXITCODE -ne 0) { throw 'Inno Setup compilation failed.' }
& $signer sign /sha1 $CertificateThumbprint /fd SHA256 /tr $TimestampServer.AbsoluteUri /td SHA256 $setup
if ($LASTEXITCODE -ne 0) { throw 'Setup signing failed; the unsigned output must not be published.' }
& $signer verify /pa /all $setup
if ($LASTEXITCODE -ne 0) { throw 'Setup signature verification failed.' }
$setupSignature = Get-AuthenticodeSignature -LiteralPath $setup
if ($setupSignature.Status -ne 'Valid' -or $setupSignature.SignerCertificate.Thumbprint -ne $CertificateThumbprint -or
    $null -eq $setupSignature.TimeStamperCertificate) { throw 'The setup publisher or timestamp could not be verified.' }
$hash = (Get-FileHash -LiteralPath $setup -Algorithm SHA256).Hash.ToLowerInvariant()
"$hash  $([System.IO.Path]::GetFileName($setup))" | Set-Content -LiteralPath (Join-Path $output 'user-setup-SHA256SUMS') -Encoding Ascii
Write-Output $setup
