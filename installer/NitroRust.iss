; Inno Setup 6.3+ script for NitroRust. Build: ISCC.exe installer\NitroRust.iss
; (after `cargo build --release` in rs\). Output: installer\Output\NitroRust-Setup-<version>.exe

#define AppName "NitroRust"
#define AppVersion "1.1.1"
#define AppExe "nitrorust.exe"

[Setup]
AppId={{6F1C2B7E-4E0A-4C55-9C3B-2A9E7D1B5F42}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher=deadnme
AppPublisherURL=https://github.com/deadnme/NitroRust
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
; The app itself needs administrator rights (Acer's WMI interface only answers elevated callers).
PrivilegesRequired=admin
; Windows 10 / 11, 64-bit only.
MinVersion=10.0
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=Output
OutputBaseFilename=NitroRust-Setup-{#AppVersion}
SetupIconFile=..\rs\app.ico
UninstallDisplayIcon={app}\{#AppExe}
LicenseFile=..\LICENSE
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
; The app is closed with --exit below so the fans are handed back to Auto first.
CloseApplications=no

[Tasks]
Name: "startup"; Description: "Start {#AppName} with Windows (in the tray)"; GroupDescription: "Options:"
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Options:"; Flags: unchecked

[Files]
Source: "..\rs\target\release\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#AppExe}"; Parameters: "--autostart on"; Flags: runhidden waituntilterminated; Tasks: startup; StatusMsg: "Setting up Start with Windows..."
Filename: "{app}\{#AppExe}"; Description: "Open {#AppName} now"; Flags: postinstall nowait skipifsilent runascurrentuser

[UninstallRun]
Filename: "{app}\{#AppExe}"; Parameters: "--exit"; Flags: runhidden waituntilterminated; RunOnceId: "Exit"
Filename: "{app}\{#AppExe}"; Parameters: "--autostart off"; Flags: runhidden waituntilterminated; RunOnceId: "Autostart"

[Code]
// On an upgrade, close the running copy cleanly before its exe is replaced.
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Exe: String;
  Code: Integer;
begin
  Result := '';
  Exe := ExpandConstant('{app}\{#AppExe}');
  if FileExists(Exe) then
    if Exec(Exe, '--exit', '', SW_HIDE, ewWaitUntilTerminated, Code) and (Code <> 0) then
      Result := 'NitroRust is still running. Exit it from the tray icon, then run Setup again.';
end;

function InitializeSetup(): Boolean;
begin
  Result := True;
  // Builds older than 10.0.17763 (Windows 10 1809) are untested.
  if not IsWin64 then
  begin
    MsgBox('NitroRust needs 64-bit Windows 10 or 11.', mbError, MB_OK);
    Result := False;
  end;
end;
