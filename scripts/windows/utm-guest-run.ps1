# One-shot guest endpoint for scripts/macos/test-windows-utm.py.
[CmdletBinding()]
param([Parameter(Mandatory = $true)][string]$ConfigPath, [switch]$Child, [string]$Engine)
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$config = Get-Content -LiteralPath $ConfigPath -Raw | ConvertFrom-Json
$env:RUSTUP_TOOLCHAIN = $config.rustToolchain
$runRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("codex-usage-monit-" + $config.runId)
$sourceRoot = Join-Path $runRoot "source"
$logPath = Join-Path $runRoot "verify.log"
$shellContracts = $config.scope -eq "shell-contracts"
if ($Child -and $shellContracts) { $logPath = Join-Path $runRoot ("verify-" + $Engine + ".log") }
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
    $engineResult = [ordered]@{
        schemaVersion = 1
        runId = $config.runId
        sourceArchiveSha256 = $config.sourceArchiveSha256
        scope = $config.scope
        engine = $Engine
        executable = (Get-Process -Id $PID).Path
        version = $PSVersionTable.PSVersion.ToString()
        edition = $PSVersionTable.PSEdition
        processArchitecture = $env:PROCESSOR_ARCHITECTURE
        status = "failed"
        casesPassed = 0
        detail = ""
    }
    try {
        $arguments = @{ RepositoryPath = $sourceRoot; Profile = $config.profile }
        if ($config.target) { $arguments.Target = $config.target }
        if ($config.testFilter) { $arguments.TestFilter = $config.testFilter }
        if ($config.focused) {
            $arguments.SkipFormat = $true
            $arguments.SkipClippy = $true
            $arguments.SkipSmoke = $true
        }
        if ($shellContracts) {
            $versionMatches = ($Engine -eq "windows-powershell" -and $PSVersionTable.PSVersion.Major -eq 5 -and
                $PSVersionTable.PSVersion.Minor -eq 1) -or
                ($Engine -eq "powershell-7" -and $PSVersionTable.PSVersion.Major -eq 7)
            if (-not $versionMatches) {
                $engineResult.status = "blocked"
                throw "Requested $Engine but the executable runs PowerShell $($engineResult.version)."
            }
            $arguments.ScriptContractsOnly = $true
        }
        & (Join-Path $sourceRoot "scripts\windows\verify.ps1") @arguments
        $childExitCode = 0
    }
    catch {
        $engineResult.detail = $_.ToString()
        Write-Host ($_ | Out-String)
    }
    finally { Stop-Transcript | Out-Null }
    if ($shellContracts) {
        if ($childExitCode -eq 0) {
            $transcript = Get-Content -LiteralPath $logPath -Raw
            $counts = [regex]::Matches($transcript, 'Windows verification regression: (\d+) cases passed under PowerShell ([0-9.]+)\.')
            if ($counts.Count -eq 1 -and $counts[0].Groups[1].Value -eq "60" -and
                $counts[0].Groups[2].Value -eq $engineResult.version) {
                $engineResult.casesPassed = 60
                $engineResult.status = "passed"
            }
            else {
                $engineResult.detail = "The transcript did not prove all 60 contracts for this engine."
                $childExitCode = 1
            }
        }
        $engineResultPath = Join-Path $runRoot ($Engine + ".result.json")
        $engineResult | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath ($engineResultPath + ".tmp") -Encoding UTF8
        Move-Item -LiteralPath ($engineResultPath + ".tmp") -Destination $engineResultPath -Force
    }
    exit $childExitCode
}

$result = [ordered]@{
    schemaVersion = 1
    runId = $config.runId
    vm = $config.vm
    mode = $config.mode
    scope = $config.scope
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
    engines = @()
    detail = ""
}

function Invoke-VerificationChild([string]$PowerShellPath, [string]$EngineName, [DateTime]$Deadline) {
    $cancelPath = $ConfigPath -replace '\.config\.json$', '.cancel'
    $cancelled = Test-Path -LiteralPath $cancelPath -PathType Leaf
    if ($cancelled -or [DateTime]::UtcNow -ge $Deadline) {
        return @{
            status = $(if ($cancelled) { "cancelled" } else { "timed_out" })
            detail = "Verification was not started because its cancellation/deadline was already reached."
        }
    }
    $childArguments = '-NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "' + $PSCommandPath + '" -ConfigPath "' + $ConfigPath + '" -Child'
    if ($EngineName) { $childArguments += ' -Engine ' + $EngineName }
    $process = Start-Process -FilePath $PowerShellPath -ArgumentList $childArguments -PassThru -WindowStyle Hidden
    try {
        $null = $process.Handle
        while (-not $process.WaitForExit(1000)) {
            $cancelled = Test-Path -LiteralPath $cancelPath -PathType Leaf
            if ($cancelled -or [DateTime]::UtcNow -ge $Deadline) {
                & (Join-Path $env:SystemRoot "System32\taskkill.exe") /PID $process.Id /T /F | Out-Null
                $cleanupCode = $LASTEXITCODE
                return @{
                    status = $(if ($cancelled) { "cancelled" } else { "timed_out" })
                    detail = "Verification stopped; process-tree cleanup exited with code $cleanupCode."
                }
            }
        }
        $process.Refresh()
        return @{
            status = $(if ($process.ExitCode -eq 0) { "passed" } else { "failed" })
            detail = "Guest verification exited with code " + $process.ExitCode
        }
    }
    finally { $process.Dispose() }
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
        $guestDeadline = [DateTime]::UtcNow.AddSeconds([int]$config.timeoutSeconds)
        if ($shellContracts) {
            if ($config.pwshPath -notmatch '^[A-Za-z]:[\\/]' -or
                -not (Test-Path -LiteralPath $config.pwshPath -PathType Leaf)) {
                throw "PowerShell 7 is unavailable at --pwsh-path '$($config.pwshPath)'. Select an existing guest pwsh.exe; this runner does not install it."
            }
            $engines = @(
                @{ name = "windows-powershell"; path = $powershell },
                @{ name = "powershell-7"; path = $config.pwshPath }
            )
            $anyFailed = $false
            $anyBlocked = $false
            foreach ($runtimeSpec in $engines) {
                $outcome = Invoke-VerificationChild $runtimeSpec.path $runtimeSpec.name $guestDeadline
                $engineLog = Join-Path $runRoot ("verify-" + $runtimeSpec.name + ".log")
                if (Test-Path -LiteralPath $engineLog -PathType Leaf) {
                    ("PowerShell engine: " + $runtimeSpec.name) | Add-Content -LiteralPath $logPath -Encoding UTF8
                    Get-Content -LiteralPath $engineLog -Raw | Add-Content -LiteralPath $logPath -Encoding UTF8
                }
                if ($outcome.status -in @("cancelled", "timed_out")) {
                    $result.status = $outcome.status
                    $result.detail = $outcome.detail
                    break
                }
                $engineResultPath = Join-Path $runRoot ($runtimeSpec.name + ".result.json")
                $engineResult = Get-Content -LiteralPath $engineResultPath -Raw | ConvertFrom-Json
                if ($engineResult.schemaVersion -ne 1 -or $engineResult.runId -ne $config.runId -or
                    $engineResult.sourceArchiveSha256 -ne $config.sourceArchiveSha256 -or
                    $engineResult.scope -ne "shell-contracts" -or $engineResult.engine -ne $runtimeSpec.name) {
                    throw "PowerShell engine result does not match this run and source snapshot."
                }
                $result.engines += $engineResult
                if ($engineResult.status -eq "blocked") { $anyBlocked = $true }
                elseif ($outcome.status -ne "passed" -or $engineResult.status -ne "passed") { $anyFailed = $true }
            }
            if ($result.status -notin @("cancelled", "timed_out")) {
                $result.status = if ($anyFailed) { "failed" } elseif ($anyBlocked) { "blocked" } else { "passed" }
                $result.detail = "PowerShell 5.1 and 7 script contracts only; project Rust tests, lint, build, and CLI smoke were not requested."
            }
        }
        else {
            $outcome = Invoke-VerificationChild $powershell "" $guestDeadline
            $result.status = $outcome.status
            $result.detail = $outcome.detail
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
