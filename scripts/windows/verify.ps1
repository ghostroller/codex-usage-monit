<#
.SYNOPSIS
Runs the repository's Windows verification pipeline.

.DESCRIPTION
This is the single Windows entry point used both by GitHub Actions and by a
native Windows development machine.  It deliberately uses Cargo's locked
dependency graph, verifies formatting and linting, runs all test targets, and
then exercises the real executable against the deterministic offline fixture.

When testing a repository exposed through a VM shared folder, pass
-CargoTargetDir with a guest-local directory. That keeps Windows artifacts and
file locks out of the host checkout.
#>

[CmdletBinding()]
param(
    [string]$RepositoryPath = (Join-Path $PSScriptRoot "..\.."),

    [ValidateSet("debug", "release")]
    [string]$Profile = "debug",

    [string]$Target,

    [string]$CargoTargetDir,

    [string]$TestFilter,

    [switch]$SkipFormat,

    [switch]$SkipClippy,

    [switch]$SkipTests,

    [switch]$SkipSmoke
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Assert-NativeSuccess {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Description,

        [int[]]$AcceptedExitCodes = @(0)
    )

    if ($LASTEXITCODE -notin $AcceptedExitCodes) {
        throw "$Description failed with exit code $LASTEXITCODE."
    }
}

function Invoke-Cargo {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Description,

        [Parameter(Mandatory = $true)]
        [string[]]$Arguments
    )

    Write-Host "==> $Description"
    & cargo @Arguments
    Assert-NativeSuccess $Description
}

function Get-VisualStudioInstallationPaths {
    param(
        [Parameter(Mandatory = $true)]
        [string]$VsWhere,

        [switch]$Latest
    )

    $vsWhereExitCode = $null
    for ($attempt = 1; $attempt -le 3; $attempt++) {
        $arguments = @("-products", "*", "-property", "installationPath")
        if ($Latest) {
            $arguments = @("-latest") + $arguments
        }
        $installationPaths = @(& $VsWhere @arguments)
        $vsWhereExitCode = $LASTEXITCODE
        if ($vsWhereExitCode -eq 0) {
            return $installationPaths
        }
        if ($attempt -lt 3) {
            Start-Sleep -Milliseconds 500
        }
    }

    throw "Locate Visual Studio Build Tools failed with exit code $vsWhereExitCode after 3 attempts."
}

function Get-TargetArchitecture {
    param([string]$RustTarget)

    if ($RustTarget -like "aarch64-*") {
        return "arm64"
    }

    if ($RustTarget -like "x86_64-*") {
        return "x64"
    }

    if ($RustTarget -like "i686-*") {
        return "x86"
    }

    $processArchitecture = if ([string]::IsNullOrWhiteSpace($env:PROCESSOR_ARCHITEW6432)) {
        $env:PROCESSOR_ARCHITECTURE
    }
    else {
        $env:PROCESSOR_ARCHITEW6432
    }
    if ($processArchitecture -eq "ARM64") {
        return "arm64"
    }

    return "x64"
}

function ConvertTo-VisualStudioArchitecture {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Architecture
    )

    if ($Architecture -eq "x64") {
        return "amd64"
    }

    return $Architecture
}

function Get-VisualStudioHostArchitecture {
    $processArchitecture = if ([string]::IsNullOrWhiteSpace($env:PROCESSOR_ARCHITEW6432)) {
        $env:PROCESSOR_ARCHITECTURE
    }
    else {
        $env:PROCESSOR_ARCHITEW6432
    }

    if ($processArchitecture -eq "ARM64") {
        return "arm64"
    }

    return "amd64"
}

function Import-VisualStudioDeveloperEnvironment {
    param([string]$RustTarget)

    if (-not [string]::IsNullOrWhiteSpace($env:VSCMD_VER) -and [string]::IsNullOrWhiteSpace($RustTarget)) {
        return
    }

    $programFilesX86 = ${env:ProgramFiles(x86)}
    if ([string]::IsNullOrWhiteSpace($programFilesX86)) {
        return
    }

    $vsWhere = Join-Path $programFilesX86 "Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path -LiteralPath $vsWhere -PathType Leaf)) {
        return
    }

    $installationPath = @(Get-VisualStudioInstallationPaths $vsWhere -Latest) | Select-Object -First 1
    if ($null -eq $installationPath -or [string]::IsNullOrWhiteSpace($installationPath)) {
        return
    }
    $installationPath = $installationPath.Trim()

    $vsDevCmd = Join-Path $installationPath "Common7\Tools\VsDevCmd.bat"
    if (-not (Test-Path -LiteralPath $vsDevCmd -PathType Leaf)) {
        return
    }

    $architecture = ConvertTo-VisualStudioArchitecture (Get-TargetArchitecture $RustTarget)
    $hostArchitecture = Get-VisualStudioHostArchitecture
    $command = 'call "' + $vsDevCmd + '" -arch=' + $architecture + ' -host_arch=' + $hostArchitecture + ' >nul && set'
    $environmentLines = & $env:ComSpec /d /s /c $command
    if ($LASTEXITCODE -ne 0 -and $hostArchitecture -eq "arm64") {
        # Older VS installations may not expose an ARM64-hosted developer shell.
        # The x64-hosted tools still work under Windows-on-Arm emulation.
        $command = 'call "' + $vsDevCmd + '" -arch=' + $architecture + ' -host_arch=amd64 >nul && set'
        $environmentLines = & $env:ComSpec /d /s /c $command
    }
    Assert-NativeSuccess "Load Visual Studio developer environment"

    foreach ($line in $environmentLines) {
        if ($line -match "^([^=]+)=(.*)$") {
            Set-Item -Path ("Env:" + $matches[1]) -Value $matches[2]
        }
    }
}

function Invoke-SmokeTest {
    param(
        [Parameter(Mandatory = $true)]
        [string]$RepositoryRoot,

        [Parameter(Mandatory = $true)]
        [string]$TargetRoot,

        [Parameter(Mandatory = $true)]
        [string]$BuildProfile,

        [string]$RustTarget
    )

    $binaryDirectory = Join-Path $TargetRoot $BuildProfile
    if (-not [string]::IsNullOrWhiteSpace($RustTarget)) {
        $binaryDirectory = Join-Path (Join-Path $TargetRoot $RustTarget) $BuildProfile
    }
    $binary = Join-Path $binaryDirectory "codex-usage-monit.exe"
    if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) {
        throw "Expected Windows binary was not produced: $binary"
    }

    $temporaryRoot = Join-Path ([System.IO.Path]::GetTempPath()) (
        "codex-usage-monit-windows-smoke-" + [guid]::NewGuid().ToString("N")
    )
    $environmentNames = @(
        "CODEX_USAGE_MONIT_STATE_DIR",
        "CODEX_USAGE_MONIT_CONFIG_DIR",
        "CODEX_USAGE_MONIT_CACHE_DIR"
    )
    $previousEnvironment = @{}

    New-Item -ItemType Directory -Path $temporaryRoot -Force | Out-Null
    try {
        foreach ($name in $environmentNames) {
            $previousEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, "Process")
        }
        $env:CODEX_USAGE_MONIT_STATE_DIR = Join-Path $temporaryRoot "state"
        $env:CODEX_USAGE_MONIT_CONFIG_DIR = Join-Path $temporaryRoot "config"
        $env:CODEX_USAGE_MONIT_CACHE_DIR = Join-Path $temporaryRoot "cache"

        Write-Host "==> Smoke-test version output"
        $version = & $binary --version
        Assert-NativeSuccess "Run Windows binary with --version"
        if ([string]::IsNullOrWhiteSpace(($version -join "`n"))) {
            throw "The Windows binary returned an empty version string."
        }

        $fixtureHome = Join-Path $RepositoryRoot "tests\fixtures\codex-home\normal"
        Write-Host "==> Smoke-test offline JSON snapshot"
        $snapshot = & $binary `
            --codex-home $fixtureHome `
            --days 3650 `
            --offline `
            --no-rollout-cache `
            snapshot `
            --format json `
            --compact
        # `--offline` deliberately omits the App Server account data, so the
        # CLI reports usable-but-partial output with exit code 2. Both 0 and 2
        # are valid here; malformed JSON or a missing partial marker still
        # fails below.
        Assert-NativeSuccess "Run Windows binary against the offline fixture" -AcceptedExitCodes @(0, 2)

        $snapshotText = $snapshot -join "`n"
        if ([string]::IsNullOrWhiteSpace($snapshotText)) {
            throw "The Windows binary returned an empty snapshot."
        }
        $parsedSnapshot = $snapshotText | ConvertFrom-Json
        if ($null -eq $parsedSnapshot) {
            throw "The Windows binary did not return a JSON snapshot."
        }
        if ($null -eq $parsedSnapshot.PSObject.Properties["partial"] -or -not $parsedSnapshot.partial) {
            throw "The offline Windows smoke snapshot must be explicitly marked partial."
        }
    }
    finally {
        foreach ($name in $environmentNames) {
            if ($null -eq $previousEnvironment[$name]) {
                Remove-Item -Path ("Env:" + $name) -ErrorAction SilentlyContinue
            }
            else {
                Set-Item -Path ("Env:" + $name) -Value $previousEnvironment[$name]
            }
        }
        Remove-Item -LiteralPath $temporaryRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}

if ($env:OS -ne "Windows_NT") {
    throw "This verification script must run on Windows."
}

if ($null -eq (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw "cargo was not found. Run scripts\\windows\\bootstrap.ps1 first."
}

$repositoryRoot = (Resolve-Path -LiteralPath $RepositoryPath).Path
if (-not (Test-Path -LiteralPath (Join-Path $repositoryRoot "Cargo.toml") -PathType Leaf)) {
    throw "RepositoryPath does not contain Cargo.toml: $repositoryRoot"
}
$originalCargoTargetDir = [Environment]::GetEnvironmentVariable("CARGO_TARGET_DIR", "Process")
if (-not [string]::IsNullOrWhiteSpace($CargoTargetDir)) {
    $env:CARGO_TARGET_DIR = [System.IO.Path]::GetFullPath($CargoTargetDir)
}

Push-Location $repositoryRoot
try {
    Import-VisualStudioDeveloperEnvironment $Target

    $rustcVersion = & rustc -vV
    Assert-NativeSuccess "Read Rust host target"
    $hostTriple = ($rustcVersion | Where-Object { $_ -like "host: *" } | Select-Object -First 1) -replace "^host:\s*", ""
    Write-Host "Windows Rust host: $hostTriple"
    if (-not [string]::IsNullOrWhiteSpace($Target)) {
        Write-Host "Requested Rust target: $Target"
    }

    if (-not $SkipFormat) {
        Invoke-Cargo "Check Rust formatting" @("fmt", "--all", "--", "--check")
    }

    if (-not $SkipClippy) {
        $clippyArguments = @("clippy", "--locked", "--all-targets", "--", "-D", "warnings")
        if (-not [string]::IsNullOrWhiteSpace($Target)) {
            $clippyArguments = @("clippy", "--locked", "--target", $Target, "--all-targets", "--", "-D", "warnings")
        }
        Invoke-Cargo "Run Clippy" $clippyArguments
    }

    if (-not $SkipTests) {
        $testArguments = @("test", "--locked", "--all-targets")
        if ($Profile -eq "release") {
            $testArguments += "--release"
        }
        if (-not [string]::IsNullOrWhiteSpace($Target)) {
            $testArguments = @("test", "--locked", "--target", $Target, "--all-targets")
            if ($Profile -eq "release") {
                $testArguments += "--release"
            }
        }
        if (-not [string]::IsNullOrWhiteSpace($TestFilter)) {
            $testArguments += $TestFilter
        }
        Invoke-Cargo "Run Rust tests" $testArguments
    }

    if (-not $SkipSmoke) {
        $buildArguments = @("build", "--locked", "--bin", "codex-usage-monit")
        if ($Profile -eq "release") {
            $buildArguments += "--release"
        }
        if (-not [string]::IsNullOrWhiteSpace($Target)) {
            $buildArguments = @("build", "--locked", "--target", $Target, "--bin", "codex-usage-monit")
            if ($Profile -eq "release") {
                $buildArguments += "--release"
            }
        }
        Invoke-Cargo "Build Windows CLI for smoke test" $buildArguments

        $targetRoot = if ([string]::IsNullOrWhiteSpace($env:CARGO_TARGET_DIR)) {
            Join-Path $repositoryRoot "target"
        }
        else {
            [System.IO.Path]::GetFullPath($env:CARGO_TARGET_DIR)
        }
        Invoke-SmokeTest $repositoryRoot $targetRoot $Profile $Target
    }
}
finally {
    Pop-Location
    if ($null -eq $originalCargoTargetDir) {
        Remove-Item -Path "Env:CARGO_TARGET_DIR" -ErrorAction SilentlyContinue
    }
    else {
        Set-Item -Path "Env:CARGO_TARGET_DIR" -Value $originalCargoTargetDir
    }
}
