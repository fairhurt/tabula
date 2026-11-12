; The Windows installer, built with Inno Setup 6:
;
;   cargo build --release
;   ISCC /DAppVersion=0.2.0 installer\tabula.iss
;
; It installs for the current user (no administrator prompt), adds a Start
; menu entry and an uninstaller. Tabula's own updater runs a newer installer
; with /SILENT, which closes the running app, replaces it and starts it again.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
; The executable to package; /DExePath=... picks another build.
#ifndef ExePath
  #define ExePath "..\target\release\tabula.exe"
#endif

[Setup]
; Identifies Tabula to Windows across versions. Never change it.
AppId={{8E2B6C1A-5D0F-4B7E-9C3A-7A1D2F4E6B90}
AppName=Tabula
AppVersion={#AppVersion}
AppVerName=Tabula {#AppVersion}
AppPublisher=Tyson Fairhurst
AppPublisherURL=https://github.com/fairhurt/tabula
AppSupportURL=https://github.com/fairhurt/tabula/issues
AppUpdatesURL=https://github.com/fairhurt/tabula/releases
VersionInfoVersion={#AppVersion}
DefaultDirName={autopf}\Tabula
DefaultGroupName=Tabula
DisableProgramGroupPage=yes
DisableDirPage=auto
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\dist
OutputBaseFilename=Tabula-Setup-{#AppVersion}
SetupIconFile=..\assets\icon.ico
UninstallDisplayIcon={app}\tabula.exe
UninstallDisplayName=Tabula
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
; Agents may have `tabula mcp` running from the same file.
CloseApplications=yes
RestartApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#ExePath}"; DestDir: "{app}"; DestName: "tabula.exe"; Flags: ignoreversion
Source: "..\LICENSE-MIT"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE-APACHE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Tabula"; Filename: "{app}\tabula.exe"
Name: "{autodesktop}\Tabula"; Filename: "{app}\tabula.exe"; Tasks: desktopicon

[Run]
; After a normal install, offer to start Tabula.
Filename: "{app}\tabula.exe"; Description: "{cm:LaunchProgram,Tabula}"; Flags: nowait postinstall skipifsilent
; After an update (run silently by Tabula itself), start it again.
Filename: "{app}\tabula.exe"; Flags: nowait; Check: WizardSilent
