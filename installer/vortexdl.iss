#ifndef MyAppVersion
  #define MyAppVersion "0.1.5"
#endif

#ifndef RepoRoot
  #define RepoRoot ".."
#endif

#ifndef ReleaseDir
  #define ReleaseDir "..\target\release"
#endif

#ifndef OutputDir
  #define OutputDir "..\dist"
#endif

#ifndef BundleBinDir
  #define BundleBinDir "..\build\bundle-bin"
#endif

[Setup]
AppId={{7A6C7E12-8D9A-4B5A-9F7F-3B5E92B1C001}
AppName=VortexDL
AppVersion={#MyAppVersion}
AppVerName=VortexDL {#MyAppVersion}
AppPublisher=Igniz608
AppPublisherURL=https://github.com/Igniz608/DownloadManager
DefaultDirName={localappdata}\Programs\VortexDL
DefaultGroupName=VortexDL
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
OutputDir={#OutputDir}
OutputBaseFilename=VortexDL-Setup-v{#MyAppVersion}-win64
SetupIconFile={#RepoRoot}\installer\vortexdl.ico
UninstallDisplayIcon={app}\vortexdl.ico
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
DisableProgramGroupPage=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
CloseApplications=yes
CloseApplicationsFilter=*.exe,*.dll
RestartApplications=no

[Languages]
Name: "ptbr"; MessagesFile: "compiler:Languages\BrazilianPortuguese.isl"
Name: "en"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Criar atalho na Área de Trabalho"; GroupDescription: "Atalhos:"
Name: "magnet"; Description: "Associar links magnet:// ao VortexDL"; GroupDescription: "Integração com o Sistema:"

[Files]
Source: "{#ReleaseDir}\vortex-gui.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#ReleaseDir}\vortex-native-host.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#ReleaseDir}\WebView2Loader.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#RepoRoot}\installer\vortexdl.ico"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#RepoRoot}\installer\vortexdl-firefox.xpi"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#RepoRoot}\extension\*"; DestDir: "{app}\extension"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "{#BundleBinDir}\*"; DestDir: "{app}\bin"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{autoprograms}\VortexDL"; Filename: "{app}\vortex-gui.exe"; IconFilename: "{app}\vortexdl.ico"
Name: "{autodesktop}\VortexDL"; Filename: "{app}\vortex-gui.exe"; IconFilename: "{app}\vortexdl.ico"; Tasks: desktopicon

[Run]
Filename: "{app}\vortex-gui.exe"; Parameters: "{code:GetSilentSetupParams}"; Flags: runhidden waituntilterminated; Check: HasSilentSetupAction
Filename: "{app}\vortex-gui.exe"; Parameters: "{code:GetFinishOpenExtParams}"; Description: "Abrir VortexDL e concluir a extensão nos navegadores selecionados"; Flags: postinstall nowait skipifsilent; Check: HasManualBrowsers
Filename: "{app}\vortex-gui.exe"; Parameters: "{code:GetFinishOpenExtParams}"; Description: "Abrir VortexDL (extensão do Microsoft Edge configurada automaticamente)"; Flags: postinstall nowait skipifsilent; Check: OnlyEdgeSelected
Filename: "{app}\vortex-gui.exe"; Description: "Abrir o VortexDL"; Flags: postinstall nowait skipifsilent; Check: NoSelectedBrowsers

[UninstallRun]
Filename: "{app}\vortex-gui.exe"; Parameters: "--no-gui --uninstall-integration"; Flags: runhidden waituntilterminated; RunOnceId: "VortexDLUninstallIntegration"

[UninstallDelete]
Type: filesandordirs; Name: "{app}"
Type: filesandordirs; Name: "{localappdata}\VortexDL"
Type: filesandordirs; Name: "{userappdata}\VortexDL"

[Code]
var
  BrowserPage: TInputOptionWizardPage;
  BrowserIds: array[0..5] of String;

function IsVortexRunning(const AppDir: String): Boolean;
var
  ResultCode: Integer;
  PsCmd: String;
begin
  Result := CheckForMutexes('Local\VortexDLRunningMutex');
  if Result then Exit;

  PsCmd :=
    '-NoProfile -NonInteractive -WindowStyle Hidden -Command "' +
    '$p = Get-Process -Name ''vortex-gui'',''vortex-native-host'' -ErrorAction SilentlyContinue | ' +
    'Where-Object { $_.Path -and $_.Path.StartsWith(''' + AppDir + ''', [System.StringComparison]::OrdinalIgnoreCase) }; ' +
    'if ($p) { exit 0 } else { exit 1 }"';
  if Exec('powershell.exe', PsCmd, '', SW_HIDE, ewWaitUntilTerminated, ResultCode) then
    Result := (ResultCode = 0);
end;

procedure CloseRunningVortexProcesses(const AppDir: String);
var
  ResultCode: Integer;
  PsCmd: String;
begin
  PsCmd :=
    '-NoProfile -NonInteractive -WindowStyle Hidden -Command "' +
    '$procs = Get-Process -Name ''vortex-gui'',''vortex-native-host'' -ErrorAction SilentlyContinue; ' +
    'if (-not $procs) { exit 0 }; ' +
    '$procs | ForEach-Object { try { if ($_.MainWindowHandle -ne 0) { $_.CloseMainWindow() | Out-Null } } catch {} }; ' +
    'Start-Sleep -Milliseconds 700; ' +
    'Get-Process -Name ''vortex-gui'',''vortex-native-host'',''yt-dlp'',''ffmpeg'',''ffprobe'' -ErrorAction SilentlyContinue | ' +
    'Where-Object { (-not $_.Path) -or $_.Path.StartsWith(''' + AppDir + ''', [System.StringComparison]::OrdinalIgnoreCase) -or ($_.ProcessName -like ''vortex-*'') } | ' +
    'Stop-Process -Force -ErrorAction SilentlyContinue"';
  Exec('powershell.exe', PsCmd, '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Sleep(400);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  AppDir: String;
begin
  Result := '';
  AppDir := ExpandConstant('{app}');
  if IsVortexRunning(AppDir) then
    CloseRunningVortexProcesses(AppDir);
end;

function InitializeUninstall(): Boolean;
var
  AppDir: String;
begin
  Result := True;
  AppDir := ExpandConstant('{app}');
  if IsVortexRunning(AppDir) then
  begin
    if not UninstallSilent then
      MsgBox('O VortexDL está em execução. Ele será fechado automaticamente para concluir a desinstalação e remover todos os resíduos.', mbInformation, MB_OK);
    CloseRunningVortexProcesses(AppDir);
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  AppDir, LocalDataDir, RoamingDataDir, TempUpdateDir: String;
begin
  if CurUninstallStep = usPostUninstall then
  begin
    AppDir := ExpandConstant('{app}');
    CloseRunningVortexProcesses(AppDir);

    LocalDataDir := ExpandConstant('{localappdata}\VortexDL');
    RoamingDataDir := ExpandConstant('{userappdata}\VortexDL');
    TempUpdateDir := AddBackslash(ExtractFileDir(RemoveBackslashUnlessRoot(ExpandConstant('{tmp}')))) + 'VortexDL-Update';

    if DirExists(LocalDataDir) then DelTree(LocalDataDir, True, True, True);
    if DirExists(RoamingDataDir) then DelTree(RoamingDataDir, True, True, True);
    if DirExists(AppDir) then DelTree(AppDir, True, True, True);
    if DirExists(TempUpdateDir) then DelTree(TempUpdateDir, True, True, True);
  end;
end;

function IsBrowserInstalled(const ExeName, RelPath1, RelPath2: String): Boolean;
var
  AppKey, RegVal, LocalDir, PfDir, Pf86Dir: String;
begin
  Result := False;
  AppKey := 'SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\' + ExeName;
  if RegQueryStringValue(HKCU, AppKey, '', RegVal) and FileExists(RemoveQuotes(RegVal)) then
  begin
    Result := True;
    Exit;
  end;
  if RegQueryStringValue(HKLM, AppKey, '', RegVal) and FileExists(RemoveQuotes(RegVal)) then
  begin
    Result := True;
    Exit;
  end;

  LocalDir := ExpandConstant('{localappdata}');
  PfDir := ExpandConstant('{commonpf}');
  Pf86Dir := ExpandConstant('{commonpf32}');

  if (RelPath1 <> '') and (
    FileExists(AddBackslash(LocalDir) + RelPath1) or
    FileExists(AddBackslash(PfDir) + RelPath1) or
    FileExists(AddBackslash(Pf86Dir) + RelPath1)
  ) then
  begin
    Result := True;
    Exit;
  end;

  if (RelPath2 <> '') and (
    FileExists(AddBackslash(LocalDir) + RelPath2) or
    FileExists(AddBackslash(PfDir) + RelPath2) or
    FileExists(AddBackslash(Pf86Dir) + RelPath2)
  ) then
  begin
    Result := True;
    Exit;
  end;
end;

procedure InitializeWizard;
var
  HasChrome, HasEdge, HasBrave, HasVivaldi, HasOpera, HasFirefox: Boolean;
begin
  BrowserPage := CreateInputOptionPage(
    wpSelectTasks,
    'Integração com Navegadores',
    'Escolha em quais navegadores deseja integrar o VortexDL.',
    'Microsoft Edge (Edge Add-ons Store) é configurado automaticamente; no Firefox, o pacote assinado (.xpi) abre com 1 clique sem Modo do Desenvolvedor; nos demais navegadores Chromium, o Native Host e a pasta extension/ ficam prontos.',
    False,
    False
  );

  BrowserIds[0] := 'chrome';
  BrowserIds[1] := 'edge';
  BrowserIds[2] := 'brave';
  BrowserIds[3] := 'vivaldi';
  BrowserIds[4] := 'opera';
  BrowserIds[5] := 'firefox';

  HasChrome  := IsBrowserInstalled('chrome.exe',  'Google\Chrome\Application\chrome.exe', '');
  HasEdge    := IsBrowserInstalled('msedge.exe',  'Microsoft\Edge\Application\msedge.exe', '');
  HasBrave   := IsBrowserInstalled('brave.exe',   'BraveSoftware\Brave-Browser\Application\brave.exe', '');
  HasVivaldi := IsBrowserInstalled('vivaldi.exe', 'Vivaldi\Application\vivaldi.exe', '');
  HasOpera   := IsBrowserInstalled('opera.exe',   'Programs\Opera\opera.exe', 'Programs\Opera GX\opera.exe');
  HasFirefox := IsBrowserInstalled('firefox.exe', 'Mozilla Firefox\firefox.exe', '') or
                IsBrowserInstalled('zen.exe',     'Zen Browser\zen.exe', 'Programs\Zen Browser\zen.exe') or
                IsBrowserInstalled('floorp.exe',  'Ablaze Floorp\floorp.exe', '') or
                IsBrowserInstalled('librewolf.exe', 'LibreWolf\librewolf.exe', '') or
                IsBrowserInstalled('waterfox.exe', 'Waterfox\waterfox.exe', '');

  if HasChrome then BrowserPage.Add('Google Chrome (detectado)') else BrowserPage.Add('Google Chrome (não detectado)');
  if HasEdge then BrowserPage.Add('Microsoft Edge (detectado)') else BrowserPage.Add('Microsoft Edge (não detectado)');
  if HasBrave then BrowserPage.Add('Brave (detectado)') else BrowserPage.Add('Brave (não detectado)');
  if HasVivaldi then BrowserPage.Add('Vivaldi (detectado)') else BrowserPage.Add('Vivaldi (não detectado)');
  if HasOpera then BrowserPage.Add('Opera / Opera GX (detectado)') else BrowserPage.Add('Opera / Opera GX (não detectado)');
  if HasFirefox then BrowserPage.Add('Firefox / Zen Browser (detectado)') else BrowserPage.Add('Firefox / Zen Browser (não detectado)');

  BrowserPage.Values[0] := HasChrome;
  BrowserPage.Values[1] := HasEdge;
  BrowserPage.Values[2] := HasBrave;
  BrowserPage.Values[3] := HasVivaldi;
  BrowserPage.Values[4] := HasOpera;
  BrowserPage.Values[5] := HasFirefox;
end;

function SelectedBrowsersCsv(): String;
var
  I: Integer;
begin
  Result := '';
  for I := 0 to 5 do
  begin
    if BrowserPage.Values[I] then
    begin
      if Result <> '' then Result := Result + ',';
      Result := Result + BrowserIds[I];
    end;
  end;
end;

function HasSelectedBrowsers(): Boolean;
begin
  Result := SelectedBrowsersCsv() <> '';
end;

function NoSelectedBrowsers(): Boolean;
begin
  Result := SelectedBrowsersCsv() = '';
end;

function HasManualBrowsers(): Boolean;
var
  I: Integer;
begin
  Result := False;
  for I := 0 to 5 do
  begin
    if (I <> 1) and BrowserPage.Values[I] then
    begin
      Result := True;
      Exit;
    end;
  end;
end;

function OnlyEdgeSelected(): Boolean;
begin
  Result := BrowserPage.Values[1] and (not HasManualBrowsers());
end;

function HasSilentSetupAction(): Boolean;
begin
  Result := HasSelectedBrowsers() or WizardIsTaskSelected('magnet');
end;

function GetSilentSetupParams(Param: String): String;
var
  Csv: String;
begin
  Result := '--no-gui';
  Csv := SelectedBrowsersCsv();
  if Csv <> '' then
    Result := Result + ' --setup-browsers=' + Csv;
  if WizardIsTaskSelected('magnet') then
    Result := Result + ' --setup-magnet';
end;

function GetFinishOpenExtParams(Param: String): String;
begin
  Result := '--tab=extension --open-extension-setup=' + SelectedBrowsersCsv();
end;
