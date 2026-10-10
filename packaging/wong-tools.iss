; Wong Tools 安装程序（Inno Setup 6）。
; 调用方式：仓库根运行 packaging\build.ps1，它会先构建再调 ISCC，
; exe 和输出目录都经 /D 传绝对路径进来，不猜相对基准。
; 下面只是手调 ISCC 时的回落（假设在仓库根下调），平时以传参为准。

#ifndef MyAppVersion
#define MyAppVersion "0.1.0"
#endif
#ifndef ExePath
#define ExePath "dist\windows\wong-tools.exe"
#endif
#ifndef OutDir
#define OutDir "dist"
#endif
; 中文向导文案跟仓库走（Inno 自带目录里不一定有，见 build.ps1 传参）。
#ifndef LangIsl
#define LangIsl "packaging\ChineseSimplified.isl"
#endif
; 应用图标跟仓库走（见 build.ps1 传参）：安装包图标、卸载项图标、
; 快捷方式图标都用它。exe 本体内嵌图标另需 winres，本版快捷方式
; 指外部 ico（随包安装，不怕丢）。
#ifndef IconIco
#define IconIco "assets\icons\wong-tools.ico"
#endif

[Setup]
; 升级认这个 Id，改了它老用户会装出第二份，永远别动。
AppId=WongTools
AppName=Wong Tools
AppVersion={#MyAppVersion}
AppPublisher=Wong Tools
DefaultDirName={autopf}\Wong Tools
DefaultGroupName=Wong Tools
OutputDir={#OutDir}
OutputBaseFilename=WongTools-Setup-{#MyAppVersion}
Compression=lzma2
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; 覆盖安装时让旧进程先退出，不然 exe 占着写不进去。
CloseApplications=yes
RestartApplications=no
UninstallDisplayName=Wong Tools
SetupIconFile={#IconIco}
UninstallDisplayIcon={app}\wong-tools.ico
; 签名发布流程见 https://gpui-kit.com/docs/packaging/ ：先签 exe 再打 ZIP
; 和安装包，打完再签安装包（卸载器要走 Inno 签名卸载器流程）。

[Languages]
Name: "chinesesimp"; MessagesFile: "{#LangIsl}"

[Tasks]
Name: "desktopicon"; Description: "创建桌面快捷方式"; GroupDescription: "附加选项:"; Flags: unchecked

[Files]
; 单 exe 程序：字体/主题/文案全编进二进制了，运行时没有外部文件要带。
; ico 随包安装：快捷方式图标指它（见下）。
Source: "{#ExePath}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#IconIco}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\Wong Tools"; Filename: "{app}\wong-tools.exe"; IconFilename: "{app}\wong-tools.ico"
Name: "{autodesktop}\Wong Tools"; Filename: "{app}\wong-tools.exe"; Tasks: desktopicon; IconFilename: "{app}\wong-tools.ico"

[Run]
Filename: "{app}\wong-tools.exe"; Description: "启动 Wong Tools"; Flags: nowait postinstall skipifsilent
