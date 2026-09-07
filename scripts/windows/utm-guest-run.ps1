# One-shot guest endpoint for scripts/macos/test-windows-utm.py.
[CmdletBinding()]
param([Parameter(Mandatory = $true)][string]$ConfigPath, [switch]$Child)
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$config = Get-Content -LiteralPath $ConfigPath -Raw | ConvertFrom-Json
$env:RUSTUP_TOOLCHAIN = $config.rustToolchain
$runRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("codex-usage-monit-" + $config.runId)
$sourceRoot = Join-Path $runRoot "source"
$logPath = Join-Path $runRoot "verify.log"
$resultPath = $ConfigPath -replace '\.config\.json$', '.result.json'
New-Item -ItemType Directory -Path $runRoot -Force | Out-Null

# Guest Agent commonly runs as SYSTEM. Reuse only an explicitly selected Rust
# installation; keep the execution identity and all build/state directories local
# to this process's account. Never rewrite USERPROFILE or a user's PATH.
if (-not [string]::IsNullOrWhiteSpace($config.toolchainHome)) {
    $env:CARGO_HOME = Join-Path $config.toolchainHome ".cargo"
    $env:RUSTUP_HOME = Join-Path $config.toolchainHome ".rustup"
    $env:Path = (Join-Path $env:CARGO_HOME "bin") + ";" + $env:Path
}
$gitDirectory = Join-Path $env:ProgramFiles "Git\cmd"
if (Test-Path -LiteralPath $gitDirectory -PathType Container) {
    $env:Path = "$gitDirectory;$env:Path"
}

if ($Child) {
    Start-Transcript -LiteralPath $logPath -Force | Out-Null
    $childExitCode = 1
    try {
        $arguments = @{ RepositoryPath = $sourceRoot; Profile = $config.profile }
        if ($config.target) { $arguments.Target = $config.target }
        if ($config.testFilter) { $arguments.TestFilter = $config.testFilter }
        if ($config.focused) {
            $arguments.SkipFormat = $true
            $arguments.SkipClippy = $true
            $arguments.SkipSmoke = $true
        }
        & (Join-Path $sourceRoot "scripts\windows\verify.ps1") @arguments
        $childExitCode = 0
    }
    catch { Write-Host ($_ | Out-String) }
    finally { Stop-Transcript | Out-Null }
    exit $childExitCode
}

$result = [ordered]@{
    schemaVersion = 1
    runId = $config.runId
    vm = $config.vm
    mode = $config.mode
    status = "blocked"
    sourceRevision = $config.sourceRevision
    sourceDirty = $config.sourceDirty
    sourceArchiveSha256 = $config.sourceArchiveSha256
    user = [Environment]::UserName
    processArchitecture = $env:PROCESSOR_ARCHITECTURE
    nativeArchitecture = [Environment]::GetEnvironmentVariable("PROCESSOR_ARCHITECTURE", "Machine")
    rustHost = ""
    rustVersion = ""
    effectiveTarget = ""
    profile = $config.profile
    target = $config.target
    testFilter = $config.testFilter
    focused = $config.focused
    logPath = $logPath
    startedAt = [DateTime]::UtcNow.ToString("o")
    commands = @()
    detail = ""
}
$locationChanged = $false
try {
    if ($config.mode -ne "doctor") {
        $archivePath = $ConfigPath -replace '\.config\.json$', '.zip'
        $archiveHash = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($archiveHash -ne $config.sourceArchiveSha256) { throw "Source archive hash mismatch." }
        Expand-Archive -LiteralPath $archivePath -DestinationPath $sourceRoot
        Push-Location $sourceRoot
        $locationChanged = $true
    }
    $result.commands = @(Get-Command cargo,rustup,rustc,git -ErrorAction SilentlyContinue | Select-Object Name,Source)
    $missing = @(@("cargo", "rustup", "rustc", "git") | Where-Object { $null -eq (Get-Command $_ -ErrorAction SilentlyContinue) })
    if ($missing.Count -eq 0) {
        $rustVersion = @(& rustc -vV)
        if ($LASTEXITCODE -ne 0) { throw "rustc exists but the selected Rust toolchain cannot run." }
        $result.rustHost = ($rustVersion | Where-Object { $_ -like "host: *" } | Select-Object -First 1) -replace "^host:\s*", ""
        $result.rustVersion = $rustVersion -join "`n"
        $result.effectiveTarget = if ($config.target) { $config.target } else { $result.rustHost }
        if ([string]::IsNullOrWhiteSpace($result.rustHost)) { throw "rustc did not report a host target." }
    }
    if ($config.mode -eq "doctor") {
        $result.status = if ($missing.Count -eq 0) { "ready" } else { "blocked" }
        $result.detail = if ($missing.Count -eq 0) { "Guest execution and result-file round trip succeeded." } else { "Guest account lacks: " + ($missing -join ", ") + ". Run bootstrap in the guest or select an existing --toolchain-home." }
    }
    else {
        if ($missing.Count -gt 0) { throw ("Guest account lacks: " + ($missing -join ", ")) }
        $powershell = Join-Path $PSHOME "powershell.exe"
        $childArguments = '-NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "' + $PSCommandPath + '" -ConfigPath "' + $ConfigPath + '" -Child'
        $process = Start-Process -FilePath $powershell -ArgumentList $childArguments -PassThru -WindowStyle Hidden
        $null = $process.Handle
        $guestDeadline = [DateTime]::UtcNow.AddSeconds([int]$config.timeoutSeconds)
        $cancelPath = $ConfigPath -replace '\.config\.json$', '.cancel'
        $cancelled = $false
        while (-not $process.WaitForExit(1000)) {
            $cancelled = Test-Path -LiteralPath $cancelPath -PathType Leaf
            if ($cancelled -or [DateTime]::UtcNow -ge $guestDeadline) {
                & (Join-Path $env:SystemRoot "System32\taskkill.exe") /PID $process.Id /T /F | Out-Null
                $cleanupCode = $LASTEXITCODE
                $result.status = if ($cancelled) { "cancelled" } else { "timed_out" }
                $result.detail = "Verification stopped; process-tree cleanup exited with code $cleanupCode."
                break
            }
        }
        if ($result.status -eq "blocked") {
            $process.Refresh()
            $result.status = if ($process.ExitCode -eq 0) { "passed" } else { "failed" }
            $result.detail = "Guest verification exited with code " + $process.ExitCode
        }
    }
}
catch { $result.detail = $_.ToString() }
finally {
    if ($locationChanged) { Pop-Location }
    $result.finishedAt = [DateTime]::UtcNow.ToString("o")
    $temporaryResult = $resultPath + ".tmp"
    $result | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $temporaryResult -Encoding UTF8
    Move-Item -LiteralPath $temporaryResult -Destination $resultPath -Force
}
