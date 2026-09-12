<#
.SYNOPSIS
Starts the development TUI with all application diagnostic streams enabled.

.DESCRIPTION
Uses cargo run (the dev profile) from the repository root. Each launch gets a
unique directory under .codex-usage-monit/logs/dev, which Git ignores. Standard
input/output/error stay attached to the terminal; build errors and panic
backtraces appear there. The application writes its own JSONL diagnostic files.
Requires the same Rust/MSVC development tools as ordinary cargo run.

.PARAMETER LogRoot
Parent directory for per-run logs. Relative paths resolve from the caller's
working directory. Directories are created by the application's private logger.

.PARAMETER TuiArgs
Additional application arguments, interpreted from the repository root. Pass an
array, for example -TuiArgs @('--offline', '--days', '14'). Logging switches are
managed by this script and cannot be supplied again.

.EXAMPLE
& .\scripts\windows\dev.ps1

.EXAMPLE
& .\scripts\windows\dev.ps1 -LogRoot 'D:\TUI logs' -TuiArgs @('--offline')
#>
[CmdletBinding()]
param(
    [string]$LogRoot,
    [string[]]$TuiArgs = @()
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false

$repositoryRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..\..')).ProviderPath
$cargo = (Get-Command cargo -CommandType Application -ErrorAction Stop | Select-Object -First 1).Source
foreach ($argument in $TuiArgs) {
    if ($argument -match '^--(log-file|log-level|trace-log|perf-log|startup-log)(=|$)') {
        throw "The development launcher manages $($argument.Split('=')[0]); use -LogRoot to choose the log location."
    }
}

if ([string]::IsNullOrWhiteSpace($LogRoot)) {
    $LogRoot = Join-Path $repositoryRoot '.codex-usage-monit\logs\dev'
}
$logRootPath = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($LogRoot)
$runId = [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssfffZ') + "-$PID-" + [guid]::NewGuid().ToString('N').Substring(0, 8)
$runDirectory = Join-Path $logRootPath $runId
$cargoArguments = @(
    'run', '--locked', '--bin', 'codex-usage-monit', '--',
    '--log-file', (Join-Path $runDirectory 'events.jsonl'), '--log-level', 'debug',
    '--trace-log', (Join-Path $runDirectory 'trace.jsonl'),
    '--perf-log', (Join-Path $runDirectory 'perf.jsonl'),
    '--startup-log', (Join-Path $runDirectory 'startup.jsonl')
) + $TuiArgs

$previousBacktrace = [Environment]::GetEnvironmentVariable('RUST_BACKTRACE', 'Process')
$exitCode = 1
Push-Location -LiteralPath $repositoryRoot
try {
    $env:RUST_BACKTRACE = 'full'
    Write-Host "Development TUI logs: $runDirectory"
    Write-Host 'Events: debug; operation trace, performance, and startup timing: enabled.'
    # Do not pipe or capture this invocation: the TUI needs its console handles.
    & $cargo @cargoArguments
    $exitCode = $LASTEXITCODE
}
finally {
    [Environment]::SetEnvironmentVariable('RUST_BACKTRACE', $previousBacktrace, 'Process')
    Pop-Location
    Write-Host "Development TUI exited. Log location: $runDirectory"
}
exit $exitCode
