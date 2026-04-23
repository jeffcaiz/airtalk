; Inno Setup script for airtalk — Windows voice input.
;
; Build from CI (.github/workflows/release.yml) with:
;   set AIRTALK_VERSION=0.1.0
;   iscc installer\airtalk.iss
;
; Ships three binaries side by side: airtalk.exe (UI), airtalk-core.exe
; (subprocess spawned by the UI), airtalk-cli.exe (optional console
; launcher for terminals). The UI's `SpawnConfig::default_sibling()`
; finds airtalk-core.exe in the same directory as itself.

[Setup]
; AppName / DefaultGroupName use the display-cased "AirTalk" — this is
; what shows up in "Apps & features", the Start Menu folder, and the
; Uninstall registry entry. The install dir ({autopf}\airtalk) stays
; lowercase to match the binary name and filesystem convention.
AppName=AirTalk
AppVersion={#GetEnv('AIRTALK_VERSION')}
AppPublisher=jeffcaiz
AppPublisherURL=https://github.com/jeffcaiz/airtalk
AppSupportURL=https://github.com/jeffcaiz/airtalk/issues
DefaultDirName={autopf}\airtalk
DefaultGroupName=AirTalk
UninstallDisplayIcon={app}\airtalk.exe
OutputDir=..
OutputBaseFilename=airtalk-{#GetEnv('AIRTALK_VERSION')}-x86_64-windows-setup
SetupIconFile=..\airtalk\assets\airtalk.ico
Compression=lzma2
SolidCompression=yes
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequired=lowest
; Auto-close the running app (UI + core subprocess) before overwriting
; its files. Required for upgrade installs to succeed without a reboot.
CloseApplications=yes
RestartApplications=no

[Files]
Source: "..\target\x86_64-pc-windows-msvc\release\airtalk.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\x86_64-pc-windows-msvc\release\airtalk-core.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\x86_64-pc-windows-msvc\release\airtalk-cli.exe"; DestDir: "{app}"; Flags: ignoreversion

[Tasks]
Name: "autostart"; Description: "Start AirTalk when Windows starts"; GroupDescription: "Additional options:"; Flags: checkedonce
Name: "desktopicon"; Description: "Create a &desktop icon"; GroupDescription: "Additional options:"; Flags: unchecked

[Icons]
Name: "{group}\AirTalk"; Filename: "{app}\airtalk.exe"; IconFilename: "{app}\airtalk.exe"; Comment: "AirTalk voice input"
Name: "{group}\Uninstall AirTalk"; Filename: "{uninstallexe}"
Name: "{autodesktop}\AirTalk"; Filename: "{app}\airtalk.exe"; IconFilename: "{app}\airtalk.exe"; Tasks: desktopicon

[Registry]
; HKCU\...\Run\airtalk — the value name here MUST match VALUE_NAME in
; airtalk/src/autostart.rs, otherwise the runtime toggle and the
; installer checkbox would write to different entries.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "airtalk"; ValueData: """{app}\airtalk.exe"""; Flags: uninsdeletevalue; Tasks: autostart

[UninstallDelete]
Type: filesandordirs; Name: "{userappdata}\airtalk"

[UninstallRun]
Filename: "{cmd}"; Parameters: "/C cmdkey /delete:airtalk/asr_api_key >NUL 2>NUL"; Flags: runhidden
Filename: "{cmd}"; Parameters: "/C cmdkey /delete:airtalk/llm_api_key >NUL 2>NUL"; Flags: runhidden

[Run]
Filename: "{app}\airtalk.exe"; Description: "Launch AirTalk"; Flags: nowait postinstall skipifsilent
