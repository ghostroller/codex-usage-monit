# Runtime regression for verify.ps1 under Windows PowerShell and PowerShell 7.
# The normal Windows verification pipeline runs this before its Rust tests.
[CmdletBinding()]
param(
    [string]$VerificationScript,
    [string[]]$CaseFilter = @()
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$PSNativeCommandUseErrorActionPreference = $false
if ([string]::IsNullOrWhiteSpace($VerificationScript)) {
    $VerificationScript = Join-Path $PSScriptRoot '..\verify.ps1'
}
$verification = (Resolve-Path -LiteralPath $VerificationScript).Path
$repository = Split-Path (Split-Path (Split-Path $verification -Parent) -Parent) -Parent
$root = Join-Path ([IO.Path]::GetTempPath()) ("verify-smoke-" + [guid]::NewGuid().ToString("N"))
$shell = (Get-Process -Id $PID).Path
$completed = 0

function Quote-PowerShellLiteral([string]$Value) {
    return "'" + $Value.Replace("'", "''") + "'"
}

function Invoke-RegressionProcess([string]$Wrapper, [string]$Mode) {
    $info = New-Object Diagnostics.ProcessStartInfo
    $info.FileName = $shell
    $info.UseShellExecute = $false
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    if ($Mode -eq "github") {
        # Match the runner's pwsh -Command invocation, including dot-sourcing
        # the generated wrapper; -File alone does not reproduce this failure.
        $info.Arguments = '-NoLogo -NoProfile -NonInteractive -Command ". ' +
            (Quote-PowerShellLiteral $Wrapper) + '"'
    }
    else {
        $info.Arguments = '-NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "' + $Wrapper + '"'
    }
    $process = [Diagnostics.Process]::Start($info)
    try {
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit(30000)) {
            $process.Kill()
            throw "Windows verification regression child timed out ($Mode)."
        }
        return @{
            ExitCode = $process.ExitCode
            Output = $stdout.GetAwaiter().GetResult() + $stderr.GetAwaiter().GetResult()
        }
    }
    finally {
        $process.Dispose()
    }
}

New-Item -ItemType Directory -Force -Path (Join-Path $root "target\debug") | Out-Null
try {
    # Only this newly created fixture is made private. The verification entry
    # point must diagnose unsuitable caller TEMP permissions, not repair them.
    $sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    $acl = Get-Acl -LiteralPath $root
    $acl.SetSecurityDescriptorSddlForm("O:${sid}D:P(A;OICI;FA;;;${sid})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)")
    Set-Acl -LiteralPath $root -AclObject $acl
    $testTemp = Join-Path $root 'temp'
    New-Item -ItemType Directory -Path $testTemp | Out-Null
    $nestedTemp = Join-Path $root 'repository\temp'
    New-Item -ItemType Directory -Path $nestedTemp -Force | Out-Null
    New-Item -ItemType Directory -Path (Join-Path $root 'repository\.git') | Out-Null
    $sharedTemp = Join-Path $root 'shared-temp'
    New-Item -ItemType Directory -Path $sharedTemp | Out-Null
    $sharedAcl = Get-Acl -LiteralPath $sharedTemp
    $sharedAcl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
        [Security.Principal.SecurityIdentifier]::new('S-1-1-0'), 'ReadAndExecute',
        'ContainerInherit,ObjectInherit', 'None', 'Allow'))
    Set-Acl -LiteralPath $sharedTemp -AclObject $sharedAcl
    # A real native executable exercises PowerShell's native exit handling.
    # Cargo/rustc are stubbed only inside each child verification invocation;
    # compiling this small fixture uses the normal configured Windows toolchain.
    $fixture = @'
use std::env;
fn main() {
    let case = env::var("CUM_VERIFY_SMOKE_CASE").unwrap();
    if case == "skip-smoke" {
        eprintln!("the skipped smoke binary must never run");
        std::process::exit(19);
    }
    if env::args().any(|arg| arg == "--version") {
        println!("verification fixture 1.0");
        return;
    }
    match case.as_str() {
        "native-failure" => {
            eprintln!("native smoke failure diagnostic");
            std::process::exit(1);
        }
        "invalid-json" => println!("not valid JSON"),
        "missing-partial" => println!(r#"{{"tasks":[{{}}]}}"#),
        "false-partial" => println!(r#"{{"partial":false,"tasks":[{{}}]}}"#),
        "string-partial" => println!(r#"{{"partial":"false","tasks":[{{}}]}}"#),
        "empty-tasks" => println!(r#"{{"partial":true,"tasks":[]}}"#),
        _ => println!(r#"{{"partial":true,"tasks":[{{}}]}}"#),
    }
    std::process::exit(if case == "valid-zero" { 0 } else { 2 });
}
'@
    $source = Join-Path $root "fixture.rs"
    $fixture | Set-Content -LiteralPath $source -Encoding UTF8
    & rustc --crate-name verify_smoke_fixture $source -o (Join-Path $root "target\debug\codex-usage-monit.exe")
    if ($LASTEXITCODE -ne 0) { throw "Compile Windows verification fixture failed with exit code $LASTEXITCODE." }

    $cases = @(
        @{ Name = "valid-partial"; Success = $true },
        @{ Name = "valid-zero"; Success = $true },
        @{ Name = "skip-smoke"; Success = $true },
        @{ Name = "native-failure"; Success = $false; Diagnostic = "native smoke failure diagnostic" },
        @{ Name = "invalid-json"; Success = $false; Diagnostic = "invalid snapshot JSON" },
        @{ Name = "missing-partial"; Success = $false; Diagnostic = "explicitly marked partial" },
        @{ Name = "false-partial"; Success = $false; Diagnostic = "explicitly marked partial" },
        @{ Name = "string-partial"; Success = $false; Diagnostic = "explicitly marked partial" },
        @{ Name = "empty-tasks"; Success = $false; Diagnostic = "must contain fixture tasks" },
        @{ Name = "cargo-failure"; Success = $false; Diagnostic = "exit code 17" },
        @{ Name = "temp-missing"; Success = $false; Diagnostic = "Test TEMP must be an existing directory" },
        @{ Name = "temp-in-repository"; Success = $false; Diagnostic = "Test TEMP must be outside any Git checkout" },
        @{ Name = "temp-shared"; Success = $false; Diagnostic = "Test TEMP fixtures inherit access for other identities" }
    )
    if ($CaseFilter.Count -gt 0) {
        foreach ($name in $CaseFilter) {
            if ($name -notin $cases.Name) { throw "Unknown verification contract case: $name" }
        }
        $cases = @($cases | Where-Object { $_.Name -in $CaseFilter })
    }
    foreach ($nativePreference in @('$false', '$true')) {
        foreach ($mode in @("github", "file", "utm")) {
            foreach ($case in $cases) {
                $selectedTemp = switch ($case.Name) {
                    'temp-missing' { Join-Path $root 'missing' }
                    'temp-in-repository' { $nestedTemp }
                    'temp-shared' { $sharedTemp }
                    default { $testTemp }
                }
                $invoke = "& " + (Quote-PowerShellLiteral $verification) +
                    " -CargoTargetDir " + (Quote-PowerShellLiteral (Join-Path $root "target")) +
                    " -CargoBuildDir " + (Quote-PowerShellLiteral (Join-Path $root "build")) +
                    " -SkipFormat -SkipClippy -SkipTests"
                if ($case.Name -ne 'valid-zero') {
                    $invoke += ' -RepositoryPath ' + (Quote-PowerShellLiteral $repository)
                    $invoke += ' -TestTempDir ' + (Quote-PowerShellLiteral $selectedTemp)
                }
                if ($case.Name -eq "skip-smoke") { $invoke += " -SkipSmoke" }
                $assertEnvironment = @'
foreach ($name in @('TEMP', 'TMP', 'PSModulePath')) {
    if ([Environment]::GetEnvironmentVariable($name, 'Process') -cne $originalEnvironment[$name]) {
        throw "Verification did not restore $name."
    }
}
Write-Host 'VERIFICATION_ENVIRONMENT_RESTORED'
'@
                $invoke = "try {`n" + $invoke + "`n} finally {`n" + $assertEnvironment + "`n}"
                $assertPreferences = @'
if ($PSNativeCommandUseErrorActionPreference -ne $originalPreference) {
    throw 'Verification changed the caller native-error preference.'
}
if ($LASTEXITCODE -ne 0) { throw "Successful verification leaked LASTEXITCODE=$LASTEXITCODE" }
'@
                $wrapper = @'
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = NATIVE_PREFERENCE
$originalPreference = $PSNativeCommandUseErrorActionPreference
INHERITED_TEMP
$originalEnvironment = @{}
foreach ($name in @('TEMP', 'TMP', 'PSModulePath')) {
    $originalEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}
$global:LASTEXITCODE = 37
$env:CUM_VERIFY_SMOKE_CASE = 'FIXTURE_CASE'
$env:VSCMD_VER = 'verification-regression'
function cargo {
    $global:LASTEXITCODE = if ($env:CUM_VERIFY_SMOKE_CASE -eq 'cargo-failure') { 17 } else { 0 }
}
function rustc { 'host: verification-fixture'; $global:LASTEXITCODE = 0 }
'@.Replace("NATIVE_PREFERENCE", $nativePreference).Replace("FIXTURE_CASE", $case.Name)
                $inheritedTemp = if ($case.Name -eq 'valid-zero') {
                    '$env:TEMP = ' + (Quote-PowerShellLiteral $selectedTemp) + "`n" + '$env:TMP = $env:TEMP'
                } else { '' }
                $wrapper = $wrapper.Replace('INHERITED_TEMP', $inheritedTemp)
                if ($mode -eq "utm") {
                    # The UTM supervisor assigns zero only after the script
                    # returns without an exception. Failures must still throw.
                    $wrapper += "`n" + '$childExitCode = 1' + "`ntry {`n" + $invoke + "`n" +
                        $assertPreferences + "`n" + '$childExitCode = 0' + "`n} catch { Write-Host " +
                        '($_ | Out-String)' + " }`n" + 'exit $childExitCode'
                }
                else {
                    $wrapper += "`n" + $invoke + "`n" + $assertPreferences
                    if ($mode -eq "github") {
                        $wrapper += "`n" + 'if ((Test-Path -LiteralPath variable:\LASTEXITCODE)) { exit $LASTEXITCODE }'
                    }
                }
                $wrapperPath = Join-Path $root "wrapper.ps1"
                $wrapper | Set-Content -LiteralPath $wrapperPath -Encoding UTF8
                $result = Invoke-RegressionProcess $wrapperPath $mode
                if (-not $result.Output.Contains('VERIFICATION_ENVIRONMENT_RESTORED')) {
                    throw "Verification did not confirm environment restoration for $($case.Name) ($mode):`n$($result.Output)"
                }
                if ($case.Success) {
                    if ($result.ExitCode -ne 0) {
                        throw "Expected successful $($case.Name) ($mode, $nativePreference), exit=$($result.ExitCode):`n$($result.Output)"
                    }
                }
                elseif ($result.ExitCode -eq 0 -or
                    -not $result.Output.Contains($case.Diagnostic) -or
                    -not $result.Output.Contains("Windows verification failed:")) {
                    throw "Expected diagnostic failure for $($case.Name) ($mode, $nativePreference), exit=$($result.ExitCode):`n$($result.Output)"
                }
                if (Test-Path -LiteralPath $selectedTemp -PathType Container) {
                    $probes = @(Get-ChildItem -LiteralPath $selectedTemp -Filter 'monit-temp-probe-*' -Force)
                    if ($probes.Count -ne 0) {
                        throw "Temporary-environment preflight leaked a probe for $($case.Name)."
                    }
                }
                $completed++
            }
        }
    }
    Write-Host "Windows verification regression: $completed cases passed under PowerShell $($PSVersionTable.PSVersion)."
}
finally {
    Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue
}
