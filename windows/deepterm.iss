#define MyAppId "{{A1B2C3D4-E5F6-7890-ABCD-EF1234567890}"
#define MyAppName "DeepTerm"
#define MyAppVersion "0.1.0"
#define MyAppPublisher "DeepTerm"
#define MyAppExeName "deepterm.exe"

[Setup]
AppId={#MyAppId}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
DefaultDirName={autopf}\{#MyAppName}
DisableProgramGroupPage=yes
OutputDir=..\target\release\bundle
OutputBaseFilename=DeepTerm-{#MyAppVersion}-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
SetupIconFile=..\assets\icon.ico
UninstallDisplayIcon={app}\{#MyAppExeName}
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.17763

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "..\target\release\{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\assets\*"; DestDir: "{app}\assets"; Flags: ignoreversion recursesubdirs
; Windows Terminal ConPTY（MIT）：系统 conhost 会丢 APC，TUI 的 Kitty 图形
; 协议必须靠它透传。alacritty 从 exe 目录加载 conpty.dll，必须与 exe 同目录；
; conpty.dll 会拉起同目录的 OpenConsole.exe，两个文件成对出现（见 README.md）。
Source: "..\windows\conpty\conpty.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\windows\conpty\OpenConsole.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\windows\conpty\LICENSE"; DestDir: "{app}\licenses"; Flags: ignoreversion

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#StringChange(MyAppName, '&', '&&')}}"; Flags: nowait postinstall skipifsilent
