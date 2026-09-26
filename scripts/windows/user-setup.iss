; User Setup is a thin UI over the same Rust installer. It does not own a
; second PATH entry, file tree, task or uninstall registration.
#ifndef Payload
  #error Payload is required
#endif
#ifndef AppVersion
  #error AppVersion is required
#endif
#ifndef OutputPath
  #error OutputPath is required
#endif

[Setup]
AppId=codex-usage-monit-user-bootstrap
AppName=codex-usage-monit
AppVersion={#AppVersion}
AppPublisher=ghostroller
AppPublisherURL=https://github.com/ghostroller/codex-usage-monit
DefaultDirName={localappdata}\codex-usage-monit
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
CreateAppDir=no
DisableDirPage=yes
DisableProgramGroupPage=yes
Uninstallable=no
OutputDir={#OutputPath}
OutputBaseFilename=codex-usage-monit-user-setup-x64
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
CloseApplications=no
RestartApplications=no
SetupLogging=yes

[Tasks]
Name: path; Description: "Add the command to my user PATH"; Flags: checkedonce
Name: recorder; Description: "Run the recorder while I am logged in"; Flags: unchecked

[Files]
Source: "{#Payload}"; DestName: "codex-usage-monit.exe"; Flags: dontcopy

[Code]
var
  InstallExitCode: Integer;

function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Parameters: String;
begin
  Result := '';
  ExtractTemporaryFile('codex-usage-monit.exe');
  Parameters := 'install --current-binary --version "{#AppVersion}"';
  if not WizardIsTaskSelected('path') then
    Parameters := Parameters + ' --no-modify-path';
  if WizardIsTaskSelected('recorder') then
    Parameters := Parameters + ' --recorder on-logon';
  if not Exec(ExpandConstant('{tmp}\codex-usage-monit.exe'), Parameters,
      ExpandConstant('{tmp}'), SW_HIDE, ewWaitUntilTerminated, InstallExitCode) then
    Result := 'Could not start installation. See the setup log.'
  else if InstallExitCode <> 0 then
    Result := 'Installation needs attention (exit ' + IntToStr(InstallExitCode) +
      '). Run codex-usage-monit doctor, then install repair. Your data is preserved.';
end;

function GetCustomSetupExitCode: Integer;
begin
  Result := InstallExitCode;
end;
