# Run this focused launcher contract under both Windows PowerShell 5.1 and 7.
[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
$launcher = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..\dev.ps1')).ProviderPath
$temporaryParent = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\') + '\'
$root = Join-Path $temporaryParent ('dev-smoke-' + [guid]::NewGuid().ToString('N'))
$checkout = Join-Path $root 'checkout [test] with spaces'
$scriptDirectory = Join-Path $checkout 'scripts\windows'
$caller = Join-Path $root 'caller'
$capture = Join-Path $root 'arguments.txt'
$environmentNames = @('PATH', 'RUST_BACKTRACE', 'CUM_DEV_CAPTURE', 'CUM_DEV_EXIT')
$previousEnvironment = @{}
foreach ($name in $environmentNames) {
    $previousEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}
$completed = 0

function Assert-Launch([string]$ExpectedLogRoot, [string[]]$ExtraArguments = @()) {
    $actual = @(Get-Content -LiteralPath $capture -Encoding UTF8)
    if ($actual[0] -ne $checkout -or $actual[1] -ne 'full') {
        throw 'Cargo did not receive the repository directory and full backtrace setting.'
    }
    $runDirectory = Split-Path $actual[8] -Parent
    if ((Split-Path $runDirectory -Parent) -ne $ExpectedLogRoot) {
        throw "Unexpected log directory: $runDirectory"
    }
    $expected = @(
        'run', '--locked', '--bin', 'codex-usage-monit', '--',
        '--log-file', (Join-Path $runDirectory 'events.jsonl'), '--log-level', 'debug',
        '--trace-log', (Join-Path $runDirectory 'trace.jsonl'),
        '--perf-log', (Join-Path $runDirectory 'perf.jsonl'),
        '--startup-log', (Join-Path $runDirectory 'startup.jsonl')
    ) + $ExtraArguments
    if (($actual[2..($actual.Count - 1)] -join "`n") -cne ($expected -join "`n")) {
        throw "Native argument forwarding failed:`n$($actual -join '`n')"
    }
    if ((Get-Location).ProviderPath -ne $caller) { throw 'Launcher changed the caller directory.' }
    return $runDirectory
}

New-Item -ItemType Directory -Path $scriptDirectory, $caller -Force | Out-Null
Copy-Item -LiteralPath $launcher -Destination $scriptDirectory
$testLauncher = Join-Path $scriptDirectory 'dev.ps1'
Push-Location -LiteralPath $caller
try {
    # A native fixture verifies Windows argument quoting and real exit codes.
    # No project build, TUI, SSH connection, or user application state is needed.
    $fixture = @'
use std::{env, fs};
fn main() {
    let mut lines = vec![env::current_dir().unwrap().display().to_string(),
        env::var("RUST_BACKTRACE").unwrap_or_default()];
    lines.extend(env::args().skip(1));
    fs::write(env::var("CUM_DEV_CAPTURE").unwrap(), lines.join("\n")).unwrap();
    println!("native stdout remains visible");
    eprintln!("native stderr remains visible");
    std::process::exit(env::var("CUM_DEV_EXIT").unwrap().parse().unwrap());
}
'@
    $source = Join-Path $root 'fixture.rs'
    $fixture | Set-Content -LiteralPath $source -Encoding UTF8
    & rustc --crate-name dev_launcher_fixture $source -o (Join-Path $root 'cargo.exe')
    if ($LASTEXITCODE -ne 0) { throw "Compile launcher fixture failed: $LASTEXITCODE" }
    $env:PATH = $root + ';' + $env:PATH
    $env:CUM_DEV_CAPTURE = $capture
    $env:CUM_DEV_EXIT = '0'

    [Environment]::SetEnvironmentVariable('RUST_BACKTRACE', $null, 'Process')
    & $testLauncher
    if ($LASTEXITCODE -ne 0) { throw 'Default launch failed.' }
    $firstRun = Assert-Launch (Join-Path $checkout '.codex-usage-monit\logs\dev')
    if ($null -ne [Environment]::GetEnvironmentVariable('RUST_BACKTRACE', 'Process')) {
        throw 'Launcher did not remove its temporary backtrace setting.'
    }
    $completed++

    $env:RUST_BACKTRACE = '0'
    & $testLauncher
    $secondRun = Assert-Launch (Join-Path $checkout '.codex-usage-monit\logs\dev')
    if ($firstRun -eq $secondRun -or $env:RUST_BACKTRACE -ne '0') {
        throw 'Repeated launch reused logs or overwrote the backtrace setting.'
    }
    $completed++

    $extra = @('--offline', '--codex-home', 'a path [with brackets]', '--days', '14')
    & $testLauncher -LogRoot 'relative logs' -TuiArgs $extra
    $null = Assert-Launch (Join-Path $caller 'relative logs') $extra
    $completed++

    foreach ($preference in @($false, $true)) {
        $PSNativeCommandUseErrorActionPreference = $preference
        $env:CUM_DEV_EXIT = '17'
        & $testLauncher
        if ($LASTEXITCODE -ne 17 -or $env:RUST_BACKTRACE -ne '0' -or
            $PSNativeCommandUseErrorActionPreference -ne $preference) {
            throw 'Native failure lost its exit code or changed caller preferences.'
        }
        $null = Assert-Launch (Join-Path $checkout '.codex-usage-monit\logs\dev')
        $completed++
    }

    $PSNativeCommandUseErrorActionPreference = $false
    $shell = (Get-Process -Id $PID).Path
    & $shell -NoLogo -NoProfile -ExecutionPolicy Bypass -File $testLauncher
    if ($LASTEXITCODE -ne 17) { throw 'The -File entry point lost the native exit code.' }
    $null = Assert-Launch (Join-Path $checkout '.codex-usage-monit\logs\dev')
    $completed++

    foreach ($flag in @('--log-file', '--log-level', '--trace-log', '--perf-log', '--startup-log')) {
        foreach ($argument in @($flag, ($flag + '=override'))) {
            Remove-Item -LiteralPath $capture
            $rejected = $false
            try { & $testLauncher -TuiArgs @($argument, 'override') }
            catch { $rejected = $_.Exception.Message.Contains('development launcher manages') }
            if (-not $rejected -or (Test-Path -LiteralPath $capture)) {
                throw "Launcher did not reject its conflicting logging argument: $argument"
            }
            # Recreate only the fixture marker for the next rejection check.
            Set-Content -LiteralPath $capture -Value ''
            $completed++
        }
    }
    Write-Host "Windows development launcher: $completed cases passed under PowerShell $($PSVersionTable.PSVersion)."
}
finally {
    Pop-Location
    foreach ($name in $environmentNames) {
        [Environment]::SetEnvironmentVariable($name, $previousEnvironment[$name], 'Process')
    }
    $resolvedRoot = [IO.Path]::GetFullPath($root)
    if ($resolvedRoot.StartsWith($temporaryParent, [StringComparison]::OrdinalIgnoreCase) -and
        (Split-Path $resolvedRoot -Leaf) -like 'dev-smoke-*') {
        Remove-Item -LiteralPath $resolvedRoot -Recurse -Force
    }
}
exit 0
