; Thrennix Windows installer (Inno Setup 6).
;
; Build with packaging\build-installer.ps1, or directly:
;   ISCC.exe /DAppVersion=0.1.0 /DBinDir=C:\path\to\target\release packaging\installer\thrennix.iss
;
; Defines (all optional):
;   AppVersion         release version, e.g. 0.1.0 or 0.2.0-beta.1   (default below)
;   AppVersionNumeric  x.y.z for the version resource                 (default: AppVersion minus any -suffix)
;   BinDir             folder holding thrennix.exe and thrennix-cli.exe (default: target\release)
;
; Per-user install by default (no admin): {autopf} = %LOCALAPPDATA%\Programs.
; "Install for all users" is offered through the privileges dialog or /ALLUSERS.

#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif
#ifndef AppVersionNumeric
  #if Pos("-", AppVersion) > 0
    #define AppVersionNumeric Copy(AppVersion, 1, Pos("-", AppVersion) - 1)
  #elif Pos("+", AppVersion) > 0
    #define AppVersionNumeric Copy(AppVersion, 1, Pos("+", AppVersion) - 1)
  #else
    #define AppVersionNumeric AppVersion
  #endif
#endif
#define RepoRoot AddBackslash(SourcePath) + "..\.."
#ifndef BinDir
  #define BinDir RepoRoot + "\target\release"
#endif

#define AppName "Thrennix"
#define AppPublisher "PNT Apps"
#define AppExe "thrennix.exe"
#define PrivacyURL "https://www.pntapps.com/privacy"
#define TermsURL "https://www.pntapps.com/terms"

[Setup]
; Never change AppId: upgrades and uninstall entries are keyed on it.
AppId={{62A9DB20-6BCC-44BE-9204-DC569A8A75C7}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL=https://www.pntapps.com/
AppSupportURL=https://www.pntapps.com/about
AppUpdatesURL=https://github.com/pntapps/thrennix/releases
AppCopyright=© 2026 PNT Apps
AppComments=Fast, open-source download manager with browser integration.
VersionInfoVersion={#AppVersionNumeric}
VersionInfoProductVersion={#AppVersionNumeric}
VersionInfoProductTextVersion={#AppVersion}
VersionInfoCompany={#AppPublisher}
VersionInfoDescription={#AppName} Setup
VersionInfoProductName={#AppName}
VersionInfoCopyright=© 2026 PNT Apps

DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
DisableWelcomePage=no
DisableDirPage=auto
UsePreviousAppDir=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog commandline
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0

LicenseFile={#RepoRoot}\LICENSE-MIT
ShowLanguageDialog=no
WizardStyle=modern
WizardImageFile=wizard-large-100.bmp,wizard-large-125.bmp,wizard-large-150.bmp,wizard-large-200.bmp
WizardSmallImageFile=wizard-small-100.bmp,wizard-small-125.bmp,wizard-small-150.bmp,wizard-small-200.bmp
WizardImageStretch=yes
SetupIconFile={#RepoRoot}\crates\thrennix-app\assets\thrennix.ico
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}

OutputDir={#RepoRoot}\dist
OutputBaseFilename=Thrennix-Setup-{#AppVersion}
Compression=lzma2/ultra64
SolidCompression=yes

CloseApplications=yes
CloseApplicationsFilter=*.exe
RestartApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Messages]
WelcomeLabel1=Welcome to Thrennix
WelcomeLabel2=This will install [name/ver] on your computer.%n%nThrennix is a fast, open-source download manager for Windows. It splits downloads into parallel connections, resumes interrupted transfers, and picks up downloads straight from Chrome, Edge, Brave and Firefox.%n%nDeveloped by PNT Apps.%n%nNo administrator rights are needed. Your settings and download history are kept when you upgrade.
FinishedHeadingLabel=Thrennix is ready
FinishedLabel=Setup has installed [name] on your computer.%n%nConnect your browser in three steps:%n  1. Open chrome://extensions (or edge://extensions).%n  2. Turn on Developer mode.%n  3. Click Load unpacked and select the "extension" folder in the Thrennix install folder.%n%nCheck the box below to open that folder now. Start menu > Thrennix browser extension folder opens it later.

[CustomMessages]
TaskOptional=Optional components (downloaded with winget):
TaskFfmpeg=Install ffmpeg (needed for video downloads)
TaskYtdlp=Install yt-dlp (needed for YouTube downloads)
AlreadyInstalled=  (already installed)
TaskStartup=Start Thrennix when Windows starts
RunLaunch=Launch Thrennix
RunExtension=Open the browser extension setup guide
StatusRegister=Connecting Thrennix to your browsers...
StatusFfmpeg=Installing ffmpeg with winget (this can take a minute)...
StatusYtdlp=Installing yt-dlp with winget...
ExtensionSteps=Load the Thrennix extension in your browser:%n%n1. Open chrome://extensions (Edge: edge://extensions).%n2. Turn on Developer mode.%n3. Click "Load unpacked" and select this folder:%n%n%1
DeleteUserData=Also delete your Thrennix settings and download history?%n%n%1%n%nChoose No to keep them for a future install.
FooterCopyright=© 2026 PNT Apps
LinkWebsite=pntapps.com
LinkPrivacy=Privacy Policy
LinkTerms=Terms of Service

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "startup"; Description: "{cm:TaskStartup}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
; Unchecked here so silent installs never pull extra software unless asked for with /TASKS.
; The wizard ticks them on the Tasks page when the tool is not found (see CurPageChanged).
Name: "ffmpeg"; Description: "{cm:TaskFfmpeg}"; GroupDescription: "{cm:TaskOptional}"; Flags: unchecked; Check: WingetAvailable
Name: "ytdlp"; Description: "{cm:TaskYtdlp}"; GroupDescription: "{cm:TaskOptional}"; Flags: unchecked; Check: WingetAvailable

[InstallDelete]
; Replace the extension folder wholesale so files removed upstream do not linger.
Type: filesandordirs; Name: "{app}\extension"

[Files]
Source: "{#BinDir}\thrennix.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BinDir}\thrennix-cli.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#RepoRoot}\LICENSE-MIT"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#RepoRoot}\LICENSE-APACHE"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#RepoRoot}\README.md"; DestDir: "{app}"; Flags: ignoreversion
; extension\.keys holds the private signing key and must never ship.
Source: "{#RepoRoot}\extension\*"; DestDir: "{app}\extension"; \
  Excludes: "\.keys,\.keys\*,\tools,\tools\*,\.gitignore,*.pem,*.key,*.p12,*.pfx,*.crx"; \
  Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExe}"; Comment: "Thrennix download manager"
Name: "{group}\Thrennix browser extension folder"; Filename: "{app}\extension"; Comment: "Load this folder in chrome://extensions with Developer mode on"
Name: "{group}\{cm:UninstallProgram,{#AppName}}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Registry]
Root: HKA; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "Thrennix"; \
  ValueData: """{app}\{#AppExe}"" --minimized"; Flags: uninsdeletevalue; Tasks: startup

[Run]
; Native messaging host manifests + HKCU keys for Chrome/Edge/Chromium/Brave/Firefox.
Filename: "{app}\{#AppExe}"; Parameters: "--register"; StatusMsg: "{cm:StatusRegister}"; \
  Flags: runhidden waituntilterminated runasoriginaluser
Filename: "{code:WingetExe}"; Parameters: "install --id Gyan.FFmpeg -e --silent --accept-package-agreements --accept-source-agreements"; \
  StatusMsg: "{cm:StatusFfmpeg}"; Tasks: ffmpeg; Check: WingetAvailable; \
  Flags: runhidden waituntilterminated runasoriginaluser
Filename: "{code:WingetExe}"; Parameters: "install --id yt-dlp.yt-dlp -e --silent --accept-package-agreements --accept-source-agreements"; \
  StatusMsg: "{cm:StatusYtdlp}"; Tasks: ytdlp; Check: WingetAvailable; \
  Flags: runhidden waituntilterminated runasoriginaluser
; Finish page check boxes.
Filename: "{app}\{#AppExe}"; Description: "{cm:RunLaunch}"; Flags: postinstall nowait skipifsilent runasoriginaluser
Filename: "{app}\extension"; Description: "{cm:RunExtension}"; AfterInstall: ShowExtensionSteps; \
  Flags: postinstall shellexec skipifsilent unchecked runasoriginaluser

[UninstallRun]
; Runs after InitializeUninstall has stopped Thrennix and before any file is removed.
Filename: "{app}\{#AppExe}"; Parameters: "--unregister"; Flags: runhidden waituntilterminated; RunOnceId: "UnregisterHost"

[UninstallDelete]
; Native messaging host manifests written by --register.
Type: filesandordirs; Name: "{localappdata}\Thrennix"

[Code]
const
  LinkColor = $00E8731A; { #1a73e8 as BGR }

var
  FfmpegFound, YtdlpFound, TasksPageSeen: Boolean;

function WingetExe(Param: String): String;
begin
  Result := ExpandConstant('{localappdata}\Microsoft\WindowsApps\winget.exe');
end;

function WingetAvailable: Boolean;
begin
  Result := FileExists(WingetExe(''));
end;

{ True when Root\Exe, Root\bin\Exe or Root\<any>\bin\Exe exists (winget package layouts). }
function ExeInPackage(const Root, Exe: String): Boolean;
var
  FR: TFindRec;
begin
  Result := FileExists(Root + '\' + Exe) or FileExists(Root + '\bin\' + Exe);
  if Result then Exit;
  if FindFirst(Root + '\*', FR) then
  try
    repeat
      if (FR.Attributes and FILE_ATTRIBUTE_DIRECTORY <> 0) and (FR.Name <> '.') and (FR.Name <> '..') then
        if FileExists(Root + '\' + FR.Name + '\bin\' + Exe) or FileExists(Root + '\' + FR.Name + '\' + Exe) then
        begin
          Result := True;
          Exit;
        end;
    until not FindNext(FR);
  finally
    FindClose(FR);
  end;
end;

{ Looks for Exe on PATH, in the WinGet Links folder, and in WinGet packages whose folder name contains Hint. }
function ToolPresent(const Exe, Hint: String): Boolean;
var
  WinGet: String;
  FR: TFindRec;
begin
  Result := FileSearch(Exe, GetEnv('PATH')) <> '';
  if Result then Exit;
  WinGet := ExpandConstant('{localappdata}\Microsoft\WinGet');
  Result := FileExists(WinGet + '\Links\' + Exe);
  if Result then Exit;
  if FindFirst(WinGet + '\Packages\*', FR) then
  try
    repeat
      if (FR.Attributes and FILE_ATTRIBUTE_DIRECTORY <> 0) and (Pos(Hint, Lowercase(FR.Name)) > 0) then
        if ExeInPackage(WinGet + '\Packages\' + FR.Name, Exe) then
        begin
          Result := True;
          Exit;
        end;
    until not FindNext(FR);
  finally
    FindClose(FR);
  end;
end;

procedure OpenUrl(const Url: String);
var
  Rc: Integer;
begin
  ShellExecAsOriginalUser('open', Url, '', '', SW_SHOWNORMAL, ewNoWait, Rc);
end;

procedure WebsiteClick(Sender: TObject);
begin
  OpenUrl('https://www.pntapps.com/');
end;

procedure PrivacyClick(Sender: TObject);
begin
  OpenUrl('{#PrivacyURL}');
end;

procedure TermsClick(Sender: TObject);
begin
  OpenUrl('{#TermsURL}');
end;

function AddLink(Page: TWinControl; const Caption: String; Left, Top: Integer; Handler: TNotifyEvent): TNewStaticText;
begin
  Result := TNewStaticText.Create(WizardForm);
  Result.Parent := Page;
  Result.Caption := Caption;
  Result.Left := Left;
  Result.Top := Top;
  Result.Cursor := crHand;
  Result.Font.Color := LinkColor;
  Result.Font.Style := [fsUnderline];
  Result.OnClick := Handler;
  Result.Anchors := [akLeft, akBottom];
end;

{ "© 2026 PNT Apps · pntapps.com   Privacy Policy   Terms of Service" along the bottom of a page. }
procedure AddFooter(Page: TWinControl; Left: Integer);
var
  Lbl: TNewStaticText;
  L: TNewStaticText;
  Top, X: Integer;
begin
  Top := Page.Height - ScaleY(28);
  Lbl := TNewStaticText.Create(WizardForm);
  Lbl.Parent := Page;
  Lbl.Caption := CustomMessage('FooterCopyright') + '  ·';
  Lbl.Left := Left;
  Lbl.Top := Top;
  Lbl.Anchors := [akLeft, akBottom];
  X := Lbl.Left + Lbl.Width + ScaleX(6);
  L := AddLink(Page, CustomMessage('LinkWebsite'), X, Top, @WebsiteClick);
  X := L.Left + L.Width + ScaleX(12);
  L := AddLink(Page, CustomMessage('LinkPrivacy'), X, Top, @PrivacyClick);
  X := L.Left + L.Width + ScaleX(12);
  AddLink(Page, CustomMessage('LinkTerms'), X, Top, @TermsClick);
end;

procedure InitializeWizard;
begin
  FfmpegFound := ToolPresent('ffmpeg.exe', 'ffmpeg');
  YtdlpFound := ToolPresent('yt-dlp.exe', 'yt-dlp');
  AddFooter(WizardForm.WelcomePage, WizardForm.WelcomeLabel2.Left);
  AddFooter(WizardForm.FinishedPage, WizardForm.FinishedLabel.Left);
end;

procedure MarkTask(const Caption: String; Found: Boolean);
var
  I: Integer;
begin
  for I := 0 to WizardForm.TasksList.Items.Count - 1 do
    if WizardForm.TasksList.ItemCaption[I] = Caption then
    begin
      WizardForm.TasksList.Checked[I] := not Found;
      if Found then
        WizardForm.TasksList.ItemCaption[I] := Caption + CustomMessage('AlreadyInstalled');
    end;
end;

procedure CurPageChanged(CurPageID: Integer);
begin
  { First visit only, so the user's own choices survive Back/Next. }
  if (CurPageID = wpSelectTasks) and not TasksPageSeen then
  begin
    TasksPageSeen := True;
    MarkTask(CustomMessage('TaskFfmpeg'), FfmpegFound);
    MarkTask(CustomMessage('TaskYtdlp'), YtdlpFound);
  end;
end;

{ Stops every thrennix.exe / thrennix-cli.exe running from Dir (the app, or a native host a browser started). }
procedure StopThrennix(const Dir: String);
var
  Rc: Integer;
  Filter: String;
begin
  Filter := Dir;
  StringChangeEx(Filter, '''', '''''', True);
  Exec(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'),
    '-NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "Get-Process -Name thrennix,thrennix-cli -ErrorAction SilentlyContinue | ' +
    'Where-Object { $_.Path -and $_.Path.StartsWith(''' + AddBackslash(Filter) + ''', [System.StringComparison]::OrdinalIgnoreCase) } | ' +
    'Stop-Process -Force -ErrorAction SilentlyContinue"',
    '', SW_HIDE, ewWaitUntilTerminated, Rc);
  Sleep(300);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := '';
  { Restart Manager (CloseApplications) asks politely first; this catches native hosts it missed. }
  StopThrennix(ExpandConstant('{app}'));
end;

procedure ShowExtensionSteps;
begin
  MsgBox(FmtMessage(CustomMessage('ExtensionSteps'), [ExpandConstant('{app}\extension')]), mbInformation, MB_OK);
end;

function InitializeUninstall: Boolean;
begin
  StopThrennix(ExpandConstant('{app}'));
  Result := True;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  DataDir: String;
begin
  if CurUninstallStep = usPostUninstall then
  begin
    DataDir := ExpandConstant('{userappdata}\Thrennix');
    if DirExists(DataDir) then
      { Default button is No; /SUPPRESSMSGBOXES also answers No, so silent uninstalls keep user data. }
      if SuppressibleMsgBox(FmtMessage(CustomMessage('DeleteUserData'), [DataDir]),
           mbConfirmation, MB_YESNO or MB_DEFBUTTON2, IDNO) = IDYES then
        DelTree(DataDir, True, True, True);
  end;
end;
