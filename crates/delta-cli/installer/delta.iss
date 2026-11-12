; DELTA per-user installer template (M3 S-25/S-26/S-29/S-34). Rendered by
; `delta-cli package-build`, which passes every identity value with /D. Keep this file ASCII:
; the Chinese wizard text comes from Inno Setup's ChineseSimplified.isl.

#ifndef Identity
  #error Identity must be defined by delta-cli package-build
#endif
#ifndef DesktopOnUpdate
  #error DesktopOnUpdate must be defined by delta-cli package-build (1 for dev-like identities)
#endif
#if Ver < EncodeVer(7, 1, 0) || Ver >= EncodeVer(7, 2, 0)
  #error DELTA installers require Inno Setup 7.1.x (S-25)
#endif
#define AppIdValue "{{" + AppGuid + "}"

[Setup]
AppId={#AppIdValue}
AppName={#DisplayName}
AppVersion={#Version}
AppVerName={#DisplayName} {#Version}
AppPublisher=DELTA Contributors
AppCopyright=Copyright (C) DELTA Contributors. Apache-2.0.
VersionInfoVersion={#NumericVersion}
VersionInfoProductName={#DisplayName}
VersionInfoProductTextVersion={#Version}
VersionInfoDescription={#DisplayName} Setup
DefaultDirName={localappdata}\Programs\{#Identity}
UsePreviousAppDir=no
DisableDirPage=yes
DisableProgramGroupPage=yes
DefaultGroupName={#DisplayName}
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; The running app holds this mutex. [Code] asks it to save and exit first; nothing is killed.
AppMutex={#MutexName}
SetupMutex={#Identity}-setup
CloseApplications=no
RestartApplications=no
SetupIconFile={#IconFile}
UninstallDisplayIcon={app}\{#DesktopExe}
UninstallDisplayName={#DisplayName}
OutputDir={#OutputDir}
OutputBaseFilename={#OutputBase}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ShowLanguageDialog=no
SetupLogging=yes

[Languages]
; delta-*.isl add the wizard text Inno does not ship (the Start-menu shortcut task).
Name: "chinesesimplified"; MessagesFile: "compiler:Languages\ChineseSimplified.isl,delta-zh-CN.isl"
Name: "english"; MessagesFile: "compiler:Default.isl,delta-en.isl"

; Shortcuts. In the wizard both are optional: checked on a first install, otherwise as the
; shortcuts are now. Silent installs decide in [Code] (WantStartMenuIcon / WantDesktopIcon):
; a first install makes both; an over-install or update keeps the ones there and makes no new
; one, except that dev-like identities make the desktop shortcut again as a sign of the update.
[Tasks]
Name: "startmenuicon"; Description: "{cm:CreateStartMenuIcon}"; GroupDescription: "{cm:AdditionalIcons}"
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"

[Files]
Source: "{#SourceDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs
; The new CLI also runs from {tmp} to migrate an M2 portable installation before files are copied.
Source: "{#SourceDir}\{#CliExe}"; DestDir: "{tmp}"; DestName: "delta-migrate-helper.exe"; Flags: dontcopy

[Icons]
Name: "{autoprograms}\{#DisplayName}"; Filename: "{app}\{#DesktopExe}"; AppUserModelID: "{#Aumid}"; Check: WantStartMenuIcon
Name: "{autodesktop}\{#DisplayName}"; Filename: "{app}\{#DesktopExe}"; AppUserModelID: "{#Aumid}"; Check: WantDesktopIcon

[Run]
; Silent updates restart the app with its previous arguments (hex-encoded JSON list).
Filename: "{app}\{#DesktopExe}"; Parameters: "--restore-args {param:DELTAARGS|}"; Flags: nowait; Check: ShouldRestart
Filename: "{app}\{#DesktopExe}"; Description: "{cm:LaunchProgram,{#DisplayName}}"; Flags: nowait postinstall skipifsilent

[Code]
const
  QuitWaitMs = 35000;
  UninstallKeyPrefix = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\';
#if DesktopOnUpdate == "1"
  RecreateDesktopIconOnUpdate = True;
#else
  RecreateDesktopIconOnUpdate = False;
#endif

var
  { No uninstall entry of this AppId when Setup started: nothing of this identity is installed. }
  FirstInstall: Boolean;
  TaskDefaultsApplied: Boolean;

function StartMenuIconPath: String;
begin
  Result := ExpandConstant('{autoprograms}\{#DisplayName}.lnk');
end;

function DesktopIconPath: String;
begin
  Result := ExpandConstant('{autodesktop}\{#DisplayName}.lnk');
end;

function WantStartMenuIcon: Boolean;
begin
  if WizardSilent then
    Result := FirstInstall or FileExists(StartMenuIconPath)
  else
    Result := WizardIsTaskSelected('startmenuicon');
end;

function WantDesktopIcon: Boolean;
begin
  if WizardSilent then
    Result := FirstInstall or RecreateDesktopIconOnUpdate or FileExists(DesktopIconPath)
  else
    Result := WizardIsTaskSelected('desktopicon');
end;

{ Over an installation the wizard's boxes start as the shortcuts are now, not as Inno remembers
  the last choice: a shortcut the user deleted starts unchecked. }
procedure CurPageChanged(CurPageID: Integer);
begin
  if (CurPageID <> wpSelectTasks) or FirstInstall or TaskDefaultsApplied then
    exit;
  TaskDefaultsApplied := True;
  if FileExists(StartMenuIconPath) then
    WizardSelectTasks('startmenuicon')
  else
    WizardSelectTasks('!startmenuicon');
  if FileExists(DesktopIconPath) then
    WizardSelectTasks('desktopicon')
  else
    WizardSelectTasks('!desktopicon');
end;

{ A box cleared in the wizard removes that shortcut of an earlier installation. }
procedure CurStepChanged(CurStep: TSetupStep);
begin
  if (CurStep <> ssPostInstall) or WizardSilent then
    exit;
  if not WizardIsTaskSelected('startmenuicon') and FileExists(StartMenuIconPath) then
    DeleteFile(StartMenuIconPath);
  if not WizardIsTaskSelected('desktopicon') and FileExists(DesktopIconPath) then
    DeleteFile(DesktopIconPath);
end;

function ShouldRestart: Boolean;
begin
  Result := (ExpandConstant('{param:DELTARESTART|0}') = '1') and
    (ExpandConstant('{param:DELTAARGS|}') <> '');
end;

function WaitForAppExit(TimeoutMs: Integer): Boolean;
var
  Waited: Integer;
begin
  Waited := 0;
  while CheckForMutexes('{#MutexName}') and (Waited < TimeoutMs) do
  begin
    Sleep(200);
    Waited := Waited + 200;
  end;
  Result := not CheckForMutexes('{#MutexName}');
end;

{ S-29: never force-close. Ask the running app (through the installed CLI and the control
  pipe) to save and exit; if it is busy or does not exit, abort without changing anything. }
function InitializeSetup(): Boolean;
var
  Cli: String;
  ResultCode: Integer;
begin
  Result := True;
  FirstInstall := not RegValueExists(HKCU,
    UninstallKeyPrefix + '{' + '{#AppGuid}' + '}_is1', 'UninstallString');
  Log(Format('DELTA: first install of {#Identity}: %d', [Ord(FirstInstall)]));
  if not CheckForMutexes('{#MutexName}') then
    exit;
  Log('DELTA: {#Identity} is running; requesting quit-for-update');
  Cli := ExpandConstant('{localappdata}\Programs\{#Identity}\{#CliExe}');
  if FileExists(Cli) then
  begin
    if ExecAndLogOutput(Cli, 'control quit-for-update --identity {#Identity} --timeout-secs 30',
      '', SW_HIDE, ewWaitUntilTerminated, ResultCode, nil) then
      Log(Format('DELTA: quit-for-update exit code %d', [ResultCode]))
    else
      Log('DELTA: the installed control command could not be started');
  end
  else
    Log('DELTA: no installed control command; waiting for the application to exit');
  if WaitForAppExit(QuitWaitMs) then
  begin
    Log('DELTA: application saved and exited');
    exit;
  end;
  Log('DELTA: application is still running (busy, unsaved or not responding); installation aborted, nothing changed');
  if not WizardSilent then
    MsgBox('{#DisplayName} is still running or busy. Save your work, close it and run Setup again.',
      mbError, MB_OK);
  Result := False;
end;

{ S-34: remove an M2 portable installation's verified files before copying; modified and
  unknown files are kept and listed in the setup log by the helper. }
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Helper: String;
  ResultCode: Integer;
begin
  Result := '';
  if not FileExists(ExpandConstant('{app}\install-manifest.json')) then
    exit;
  Log('DELTA: M2 portable installation found; migrating');
  ExtractTemporaryFile('delta-migrate-helper.exe');
  Helper := ExpandConstant('{tmp}\delta-migrate-helper.exe');
  if not ExecAndLogOutput(Helper, '__migrate-portable --identity {#Identity} --install-dir "' +
    ExpandConstant('{app}') + '"', '', SW_HIDE, ewWaitUntilTerminated, ResultCode, nil)
    or (ResultCode <> 0) then
    Result := 'The previous portable installation could not be migrated; nothing was changed. See the setup log.';
end;
