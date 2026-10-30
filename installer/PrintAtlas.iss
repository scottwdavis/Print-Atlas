; Built by scripts/package.ps1. No source-code license is implied by this installer.
#ifndef AppVersion
  #error AppVersion must be supplied by scripts/package.ps1
#endif
#ifndef AppFileVersion
  #error AppFileVersion must be supplied by scripts/package.ps1
#endif
#ifndef SourceDir
  #error SourceDir must be supplied by scripts/package.ps1
#endif
#ifndef OutputDir
  #error OutputDir must be supplied by scripts/package.ps1
#endif

[Setup]
AppId={{17294011-CF53-46C0-8465-57E68DF2085D}
AppName=Print Atlas
AppVersion={#AppVersion}
AppVerName=Print Atlas {#AppVersion}
VersionInfoVersion={#AppFileVersion}
VersionInfoProductName=Print Atlas
VersionInfoDescription=Print Atlas Installer
DefaultDirName={localappdata}\Programs\Print Atlas
DefaultGroupName=Print Atlas
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.17763
OutputDir={#OutputDir}
OutputBaseFilename=Print-Atlas-Setup
SetupIconFile=..\icon.ico
UninstallDisplayIcon={app}\Print Atlas.exe
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes
RestartApplications=no
DisableWelcomePage=no
DisableDirPage=auto
UsePreviousAppDir=yes
UninstallDisplayName=Print Atlas

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "startmenuicon"; Description: "Create a Start menu shortcut"; GroupDescription: "Shortcuts:"
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Shortcuts:"; Flags: unchecked

[Files]
Source: "{#SourceDir}\Print Atlas.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\runtime\*"; DestDir: "{app}\runtime"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "{#SourceDir}\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\third-party-licenses\*"; DestDir: "{app}\third-party-licenses"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{userprograms}\Print Atlas"; Filename: "{app}\Print Atlas.exe"; WorkingDir: "{app}"; Tasks: startmenuicon
Name: "{userdesktop}\Print Atlas"; Filename: "{app}\Print Atlas.exe"; WorkingDir: "{app}"; Tasks: desktopicon

[Run]
Filename: "{app}\Print Atlas.exe"; Description: "Launch Print Atlas"; Flags: nowait postinstall skipifsilent

; Only installed files are uninstalled. Never delete %APPDATA%\Print Atlas,
; a chosen model-library folder, or other user-created files.
