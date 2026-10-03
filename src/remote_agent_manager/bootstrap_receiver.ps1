# Fixed, visible SSH receiver. stdin contains data; it is never evaluated here.
# No dollar expansion, double quotes or backticks: this command also passes
# unchanged through a PowerShell SSH default shell.
Set-Variable -Name ErrorActionPreference -Value Stop;
Set-Variable -Name CodexBootstrapOwned -Value ([bool]0);
Set-Variable -Name CodexBootstrapWriter -Value ([object]::new());
try {
    if (Test-Path -LiteralPath './__STAGE__') { throw 'agent_bootstrap_stage_exists' };
    New-Object Security.AccessControl.DirectorySecurity | Tee-Object -Variable CodexBootstrapAcl | ForEach-Object -MemberName SetSecurityDescriptorSddlForm -ArgumentList ([string]::Format('O:{0}G:{0}D:P(A;OICI;FA;;;{0})',[Security.Principal.WindowsIdentity]::GetCurrent().User.Value));
    if ((Get-Variable PSVersionTable -ValueOnly).PSEdition -eq 'Desktop') {
        [void][IO.Directory]::CreateDirectory([IO.Path]::GetFullPath('./__STAGE__'),(Get-Variable CodexBootstrapAcl -ValueOnly));
        Set-Variable -Name CodexBootstrapActualAcl -Value ([IO.DirectoryInfo]::new('./__STAGE__').GetAccessControl());
    } else {
        [void][IO.FileSystemAclExtensions]::Create([IO.DirectoryInfo]::new('./__STAGE__'),(Get-Variable CodexBootstrapAcl -ValueOnly));
        Set-Variable -Name CodexBootstrapActualAcl -Value ([IO.FileSystemAclExtensions]::GetAccessControl([IO.DirectoryInfo]::new('./__STAGE__')));
    };
    if (([IO.File]::GetAttributes('./__STAGE__') -band [IO.FileAttributes]::ReparsePoint) -ne 0 -or (Get-Variable CodexBootstrapActualAcl -ValueOnly).GetSecurityDescriptorSddlForm([Security.AccessControl.AccessControlSections]'Owner,Access') -cne (Get-Variable CodexBootstrapAcl -ValueOnly).GetSecurityDescriptorSddlForm([Security.AccessControl.AccessControlSections]'Owner,Access')) { throw 'agent_bootstrap_stage_not_private' };
    Set-Variable -Name CodexBootstrapOwned -Value ([bool]1);
    Set-Variable -Name CodexBootstrapReader -Value ([IO.BinaryReader]::new([Console]::OpenStandardInput()));
    try { Set-Variable -Name CodexBootstrapBytes -Value ((Get-Variable CodexBootstrapReader -ValueOnly).ReadBytes(__LIMIT__)) }
    finally { (Get-Variable CodexBootstrapReader -ValueOnly).Dispose() };
    if ((Get-Variable CodexBootstrapBytes -ValueOnly).Length -ge __LIMIT__) { throw 'agent_bootstrap_input_too_large' };
    Set-Variable -Name CodexBootstrapHasher -Value ([Security.Cryptography.SHA256]::Create());
    try {
        if ([BitConverter]::ToString((Get-Variable CodexBootstrapHasher -ValueOnly).ComputeHash((Get-Variable CodexBootstrapBytes -ValueOnly))).Replace('-','').ToLowerInvariant() -cne '__SHA256__') { throw 'agent_bootstrap_checksum_mismatch' };
    } finally { (Get-Variable CodexBootstrapHasher -ValueOnly).Dispose() };
    Set-Variable -Name CodexBootstrapWriter -Value ([IO.File]::Open('./__STAGE__/bootstrap.ps1',[IO.FileMode]::CreateNew,[IO.FileAccess]::Write,[IO.FileShare]::None));
    try {
        (Get-Variable CodexBootstrapWriter -ValueOnly).Write([byte[]](239,187,191),0,3);
        (Get-Variable CodexBootstrapWriter -ValueOnly).Write((Get-Variable CodexBootstrapBytes -ValueOnly),0,(Get-Variable CodexBootstrapBytes -ValueOnly).Length);
    }
    finally { (Get-Variable CodexBootstrapWriter -ValueOnly).Dispose() };
    exit 0;
} catch {
    [Console]::Error.WriteLine((Get-Variable _ -ValueOnly).Exception.Message);
    if (Get-Variable CodexBootstrapOwned -ValueOnly) {
        try {
            if ((Get-Variable CodexBootstrapWriter -ValueOnly) -is [IO.FileStream]) { [IO.File]::Delete('./__STAGE__/bootstrap.ps1') };
            [IO.Directory]::Delete([IO.Path]::GetFullPath('./__STAGE__'),[bool]0);
        } catch { [Console]::Error.WriteLine('agent_bootstrap_cleanup_failed') };
    };
    exit 1;
}
