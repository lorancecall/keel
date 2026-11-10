; Inno Setup script - build:  .\build.ps1  (cargo build --release, cargo test, then ISCC.exe installer\keel.iss)
#define AppVersion "0.1.45"

[Setup]
AppId={{444DB70A-6E16-45C8-8959-7F2B3EA77C67}
AppName=Keel
AppVersion={#AppVersion}
AppPublisher=lorancecall
; The service runs only from here (admin-only folder); `keel service-install` refuses other paths.
DefaultDirName={autopf}\Keel
DisableDirPage=yes
DefaultGroupName=Keel
DisableProgramGroupPage=yes
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\dist
OutputBaseFilename=keel-setup-{#AppVersion}
Compression=lzma2
SolidCompression=yes
ChangesEnvironment=yes
UninstallDisplayName=Keel
WizardStyle=modern
SetupIconFile=..\assets\keel.ico
UninstallDisplayIcon={app}\keel-gui.exe

[Tasks]
Name: addtopath; Description: "Add the keel command to PATH"; Flags: checkedonce
Name: desktopicon; Description: "Create a desktop shortcut"; Flags: unchecked

[Files]
Source: "..\target\release\keel.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\release\keel-tray.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\release\keel-gui.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
; The Start-menu shortcut carries the AppUserModelID, so the tray's notifications show as "Keel".
Name: "{group}\Keel"; Filename: "{app}\keel-gui.exe"; AppUserModelID: "lorancecall.Keel"
Name: "{group}\Keel Tray"; Filename: "{app}\keel-tray.exe"
Name: "{autodesktop}\Keel"; Filename: "{app}\keel-gui.exe"; Tasks: desktopicon
Name: "{commonstartup}\Keel Tray"; Filename: "{app}\keel-tray.exe"
Name: "{group}\Keel Command Prompt (run as administrator)"; Filename: "{cmd}"; Parameters: "/k ""cd /d ""{app}"" && keel.exe status"""; WorkingDir: "{app}"
Name: "{group}\Open Service Log"; Filename: "notepad.exe"; Parameters: """{commonappdata}\keel\service.log"""
Name: "{group}\Uninstall Keel"; Filename: "{uninstallexe}"

[Run]
Filename: "{app}\keel.exe"; Parameters: "service-install"; Flags: runhidden waituntilterminated; StatusMsg: "Installing the service..."
Filename: "{app}\keel-tray.exe"; Description: "Start the Keel tray"; Flags: nowait postinstall runasoriginaluser
Filename: "{app}\keel-gui.exe"; Description: "Open Keel"; Flags: nowait postinstall runasoriginaluser unchecked

[UninstallRun]
Filename: "{sys}\taskkill.exe"; Parameters: "/F /IM keel-tray.exe"; Flags: runhidden waituntilterminated; RunOnceId: "KillTray"
Filename: "{sys}\taskkill.exe"; Parameters: "/F /IM keel-gui.exe"; Flags: runhidden waituntilterminated; RunOnceId: "KillGui"
Filename: "{app}\keel.exe"; Parameters: "egpu legion-fix off"; Flags: runhidden waituntilterminated; RunOnceId: "LegionFixOff"
; Puts back everything the service changed (on-demand families, NVIDIA services, network), then removes it.
Filename: "{app}\keel.exe"; Parameters: "service-uninstall"; Flags: runhidden waituntilterminated; RunOnceId: "RemoveService"

[UninstallDelete]
Type: files; Name: "{app}\keel-icon.ico"
Type: filesandordirs; Name: "{commonappdata}\keel\driver-cache"

[Code]
const EnvKey = 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment';

function PrepareToInstall(var NeedsRestart: Boolean): String;
var rc: Integer;
begin
  { upgrade: release the running exes (the service keeps its state; families stay off) }
  Exec(ExpandConstant('{sys}\sc.exe'), 'stop Keel', '', SW_HIDE, ewWaitUntilTerminated, rc);
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM keel-tray.exe', '', SW_HIDE, ewWaitUntilTerminated, rc);
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM keel-gui.exe', '', SW_HIDE, ewWaitUntilTerminated, rc);
  Sleep(2000);
  Result := '';
end;

procedure AddToPath();
var p: String;
begin
  if not RegQueryStringValue(HKLM, EnvKey, 'Path', p) then p := '';
  if Pos(';' + Uppercase(ExpandConstant('{app}')) + ';', ';' + Uppercase(p) + ';') = 0 then
    RegWriteExpandStringValue(HKLM, EnvKey, 'Path', p + ';' + ExpandConstant('{app}'));
end;

procedure RemoveFromPath();
var p, app: String; i: Integer;
begin
  if not RegQueryStringValue(HKLM, EnvKey, 'Path', p) then exit;
  app := ';' + ExpandConstant('{app}');
  i := Pos(Uppercase(app), Uppercase(p));
  if i > 0 then begin
    Delete(p, i, Length(app));
    RegWriteExpandStringValue(HKLM, EnvKey, 'Path', p);
  end;
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if (CurStep = ssPostInstall) and WizardIsTaskSelected('addtopath') then AddToPath();
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then RemoveFromPath();
end;
