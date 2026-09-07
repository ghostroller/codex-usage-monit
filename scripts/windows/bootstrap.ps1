<#
.SYNOPSIS
Installs the prerequisites for the local Windows verification pipeline.

.DESCRIPTION
Run this once from a Windows 11 development VM. It installs Git and the
Microsoft C++ Build Tools workload required by Rust's MSVC targets, installs
rustup when it is missing, installs the repository-pinned Rust toolchain and
then invokes the same verification script used by GitHub Actions.

The default Cargo target directory is intentionally guest-local. This makes it
safe to run against a repository mounted from macOS through UTM SPICE WebDAV.
#>

[CmdletBinding()]
param(
    [string]$RepositoryPath = (Join-Path $PSScriptRoot "..\.."),

    [string]$RustToolchain = "1.97.0",

    [string]$CargoTargetDir,

    [string]$CargoBuildDir,

    [switch]$SkipBuildTools,

    [switch]$SkipRustup,

    [switch]$SkipVerification
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

function Invoke-WingetInstall {
    param(
        [Parameter(Mandatory = $true)]
        [string]$PackageId,

        [string[]]$AdditionalArguments = @()
    )

    if ($null -eq (Get-Command winget -ErrorAction SilentlyContinue)) {
        throw "Installing $PackageId requires winget. Install Microsoft App Installer, then retry."
    }
    Write-Host "==> Install $PackageId"
    & winget install `
        --id $PackageId `
        --exact `
        --silent `
        --accept-package-agreements `
        --accept-source-agreements `
        @AdditionalArguments
    Assert-NativeSuccess "Install $PackageId" @(0, 3010)
    if ($LASTEXITCODE -eq 3010) {
        Write-Warning "Windows reports that a restart is required before the installed tools are used."
    }
}

function Add-PathDirectory {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Directory
    )

    if ((Test-Path -LiteralPath $Directory -PathType Container) -and -not (($env:Path -split ";") -contains $Directory)) {
        $env:Path = "$Directory;$env:Path"
    }
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

function Test-VisualCppTools {
    $processArchitecture = if ([string]::IsNullOrWhiteSpace($env:PROCESSOR_ARCHITEW6432)) {
        $env:PROCESSOR_ARCHITECTURE
    }
    else {
        $env:PROCESSOR_ARCHITEW6432
    }
    $targetArchitecture = if ($processArchitecture -eq "ARM64") { "arm64" } else { "x64" }

    $programFilesX86 = ${env:ProgramFiles(x86)}
    if ([string]::IsNullOrWhiteSpace($programFilesX86)) {
        return $false
    }

    $vsWhere = Join-Path $programFilesX86 "Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path -LiteralPath $vsWhere -PathType Leaf)) {
        return $false
    }

    $installationPaths = @(Get-VisualStudioInstallationPaths $vsWhere)
    foreach ($installationPath in $installationPaths) {
        if ($null -eq $installationPath) {
            continue
        }

        $toolsetRoot = Join-Path $installationPath.Trim() "VC\Tools\MSVC"
        if (-not (Test-Path -LiteralPath $toolsetRoot -PathType Container)) {
            continue
        }

        foreach ($toolset in Get-ChildItem -LiteralPath $toolsetRoot -Directory) {
            foreach ($hostArchitecture in @("HostARM64", "Hostx64", "Hostx86")) {
                $linker = Join-Path $toolset.FullName ("bin\{0}\{1}\link.exe" -f $hostArchitecture, $targetArchitecture)
                if (Test-Path -LiteralPath $linker -PathType Leaf) {
                    return $true
                }
            }
        }
    }

    return $false
}

if ($env:OS -ne "Windows_NT") {
    throw "This bootstrap script must run on Windows."
}

$repositoryRoot = (Resolve-Path -LiteralPath $RepositoryPath).Path
if (-not (Test-Path -LiteralPath (Join-Path $repositoryRoot "Cargo.toml") -PathType Leaf)) {
    throw "RepositoryPath does not contain Cargo.toml: $repositoryRoot"
}
if ([string]::IsNullOrWhiteSpace($CargoTargetDir)) {
    $localAppData = [Environment]::GetFolderPath("LocalApplicationData")
    $CargoTargetDir = Join-Path $localAppData "codex-usage-monit\cargo-target"
}

# Repeated bootstrap runs must discover tools installed by an earlier process
# before deciding an installer is needed.
Add-PathDirectory (Join-Path $env:ProgramFiles "Git\cmd")
Add-PathDirectory (Join-Path $env:USERPROFILE ".cargo\bin")
if ($null -eq (Get-Command git -ErrorAction SilentlyContinue)) {
    Invoke-WingetInstall "Git.Git"
}

# winget changes the persisted PATH, but this process retains its original
# environment. Git's standard machine-wide location lets this initial run
# execute repository tests without requiring a new shell.
$gitBin = Join-Path $env:ProgramFiles "Git\cmd"
Add-PathDirectory $gitBin
if ($null -eq (Get-Command git -ErrorAction SilentlyContinue)) {
    throw "git was not found after installation. Open a new PowerShell window, then run this script again."
}

if (-not $SkipBuildTools) {
    if (-not (Test-VisualCppTools)) {
        Invoke-WingetInstall "Microsoft.VisualStudio.2022.BuildTools" @(
            "--override",
            "--wait --quiet --norestart --add Microsoft.VisualStudio.Workload.VCTools --add Microsoft.VisualStudio.Component.VC.Tools.ARM64 --add Microsoft.VisualStudio.Component.VC.Tools.x86.x64 --includeRecommended"
        )
    }
    if (-not (Test-VisualCppTools)) {
        throw "The Visual C++ Build Tools workload is still unavailable. Restart Windows if prompted, then run this script again."
    }
}

if (-not $SkipRustup -and $null -eq (Get-Command rustup -ErrorAction SilentlyContinue)) {
    Invoke-WingetInstall "Rustlang.Rustup"
}

$cargoBin = Join-Path $env:USERPROFILE ".cargo\bin"
Add-PathDirectory $cargoBin

if ($null -eq (Get-Command rustup -ErrorAction SilentlyContinue)) {
    throw "rustup was not found after installation. Open a new PowerShell window, then run this script again."
}

Write-Host "==> Install Rust $RustToolchain with CI components"
& rustup toolchain install $RustToolchain --profile minimal --component rustfmt --component clippy
Assert-NativeSuccess "Install Rust toolchain"

if (-not $SkipVerification) {
    $verifyScript = Join-Path $PSScriptRoot "verify.ps1"
    & $verifyScript -RepositoryPath $repositoryRoot -CargoTargetDir $CargoTargetDir -CargoBuildDir $CargoBuildDir
}
