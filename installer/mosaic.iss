; Mosaic installer. Build everything with ..\build.ps1 (it runs cargo, then ISCC on this file).
; One UAC prompt at install: the app goes to Program Files and the background index service is installed and
; started by the installer itself, so nothing is left for the user to set up.
#define Version "2.0.0"

[Setup]
AppId={{6B0A8E52-3F0B-4C57-9B7E-7E5D2A1C4F10}
AppName=Mosaic
AppVersion={#Version}
AppPublisher=Mosaic
DefaultDirName={autopf}\Mosaic
DefaultGroupName=Mosaic
DisableProgramGroupPage=yes
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\dist
OutputBaseFilename=MosaicSetup
SetupIconFile=setup.ico
UninstallDisplayIcon={app}\mosaic.exe
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
CloseApplications=no

[Files]
; the unstamped base; the first [Run] step turns it into the four named exes (native::dist) with the chosen icons, then it is deleted
Source: "..\dist\mosaic-base.exe"; DestDir: "{app}"; Flags: ignoreversion deleteafterinstall
Source: "..\dist\prev\*.bmp"; DestDir: "{tmp}"; Flags: dontcopy

[Icons]
Name: "{autoprograms}\Mosaic"; Filename: "{app}\mosaic.exe"
Name: "{autodesktop}\Mosaic"; Filename: "{app}\mosaic.exe"

[InstallDelete]
; the earlier per-user install (and its shortcut), so there is only ever one Mosaic
Type: filesandordirs; Name: "{localappdata}\Programs\Mosaic"
Type: files; Name: "{userprograms}\Mosaic.lnk"

[Run]
Filename: "{app}\mosaic-base.exe"; Parameters: "--dist ""{app}"" {code:IconArgs}"; Flags: runhidden waituntilterminated; StatusMsg: "Creating Mosaic..."
; Defender would otherwise scan every file the indexer reads (it used ~5 cores doing so). Only this process, by its full
; path (admin-writable folder, so nothing else can sit at that path); not the index folder, not any other program.
Filename: "powershell.exe"; Parameters: "-NoProfile -WindowStyle Hidden -Command ""try {{ Add-MpPreference -ExclusionProcess '{app}\mosaic-index.exe' -ErrorAction Stop } catch {{ }"""; Flags: runhidden waituntilterminated
; make Explorer re-read icons so shortcuts and folders pick up the chosen icon set
Filename: "{sys}\ie4uinit.exe"; Parameters: "-show"; Flags: runhidden waituntilterminated
; registers the service (it copies the exe to {app}\mosaic-index.exe) and starts it
Filename: "{app}\mosaic.exe"; Parameters: "--svc-setup"; Flags: runhidden waituntilterminated; StatusMsg: "Installing the indexing service..."
; keep a taskbar pin from the old install working
Filename: "powershell.exe"; Parameters: "-NoProfile -WindowStyle Hidden -Command ""$l=Join-Path $env:APPDATA 'Microsoft\Internet Explorer\Quick Launch\User Pinned\TaskBar'; Get-ChildItem $l -Filter 'Mosaic*.lnk' -ErrorAction SilentlyContinue | ForEach-Object {{ $s=(New-Object -ComObject WScript.Shell).CreateShortcut($_.FullName); $s.TargetPath='{app}\mosaic.exe'; $s.IconLocation='{app}\mosaic.exe,0'; $s.Save() }"""; Flags: runhidden runasoriginaluser
Filename: "{app}\mosaic.exe"; Description: "Launch Mosaic"; Flags: nowait postinstall skipifsilent runasoriginaluser

[UninstallRun]
Filename: "powershell.exe"; Parameters: "-NoProfile -WindowStyle Hidden -Command ""try {{ Remove-MpPreference -ExclusionProcess '{app}\mosaic-index.exe' -ErrorAction Stop } catch {{ }"""; Flags: runhidden waituntilterminated; RunOnceId: "RemoveDefenderExclusion"
Filename: "{app}\mosaic.exe"; Parameters: "--svc-uninstall"; Flags: runhidden waituntilterminated; RunOnceId: "RemoveService"

[UninstallDelete]
Type: files; Name: "{app}\mosaic-index.exe"
Type: files; Name: "{app}\mosaic.exe"
Type: files; Name: "{app}\mosaic-content.exe"
Type: files; Name: "{app}\mosaic-thumbs.exe"
Type: files; Name: "{app}\icons.txt"
Type: dirifempty; Name: "{app}"

[Code]
procedure Kill(const Image: String);
var R: Integer;
begin
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM ' + Image, '', SW_HIDE, ewWaitUntilTerminated, R);
end;

// stop the running app, helpers and service so their exes can be replaced
procedure StopMosaic;
var R: Integer;
begin
  Exec(ExpandConstant('{sys}\sc.exe'), 'stop AllTheThingsSvc', '', SW_HIDE, ewWaitUntilTerminated, R);
  Kill('mosaic.exe');
  Kill('mosaic-thumbs.exe');
  Kill('mosaic-content.exe');
  Kill('mosaic-index.exe');
  Sleep(1000);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  StopMosaic;
  Result := '';
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then StopMosaic;
end;

// ---- icon presets: pick a role (radio), then click a tile ----
const Presets = 'crimson,crimson-bis,gold,gold-bis,graphite,graphite-bis,green,green-bis,light,light-bis,mint,mint-bis,navy,navy-bis,ocean,ocean-bis,purple,purple-bis,rose,rose-bis,sunset,sunset-bis';
var
  Radio: array[0..3] of TNewRadioButton;
  Cur: array[0..3] of TBitmapImage;   // small preview of each role's current choice
  Pick: array[0..3] of String;        // '' = default
  Tile: array[0..21] of TPanel;
  TileName: array[0..21] of String;
  Role: Integer;

function RoleName(I: Integer): String;
begin
  case I of 0: Result := 'main'; 1: Result := 'index'; 2: Result := 'content'; else Result := 'thumbs'; end;
end;

procedure Load(Img: TBitmapImage; N: String);
begin
  if N = '' then Img.Visible := False
  else begin
    Img.Bitmap.LoadFromFile(ExpandConstant('{tmp}\' + N + '.bmp'));
    Img.Visible := True;
  end;
end;

procedure Highlight;
var I: Integer;
begin
  for I := 0 to 21 do
    if TileName[I] = Pick[Role] then Tile[I].Color := clHighlight else Tile[I].Color := clBtnFace;
end;

procedure SetPick(N: String);
begin
  Pick[Role] := N;
  Load(Cur[Role], N);
  Highlight;
end;

procedure TileClick(Sender: TObject);
begin
  SetPick(TileName[TComponent(Sender).Tag]);
end;

procedure DefaultClick(Sender: TObject);
begin
  SetPick('');
end;

procedure RoleClick(Sender: TObject);
var I: Integer;
begin
  for I := 0 to 3 do if Sender = Radio[I] then Role := I;
  Highlight;
end;

procedure InitializeWizard;
var Pg: TWizardPage; I: Integer; T, N: String; Img: TBitmapImage; B: TNewButton;
begin
  Pg := CreateCustomPage(wpSelectDir, 'Icons', 'Choose an icon for each of Mosaic''s Task Manager entries: select an entry, then click an icon.');
  for I := 0 to 3 do begin
    case I of
      0: T := 'Main app';
      1: T := 'Index service';
      2: T := 'Content helper';
      else T := 'Thumbnails helper';
    end;
    Radio[I] := TNewRadioButton.Create(Pg);
    Radio[I].Parent := Pg.Surface; Radio[I].Caption := T;
    Radio[I].Left := I * 100; Radio[I].Top := 0; Radio[I].Width := 96;
    Radio[I].Checked := (I = 0);
    Radio[I].OnClick := @RoleClick;
    Cur[I] := TBitmapImage.Create(Pg);
    Cur[I].Parent := Pg.Surface; Cur[I].Stretch := True; Cur[I].Visible := False;
    Cur[I].Left := I * 100 + 20; Cur[I].Top := 22; Cur[I].Width := 32; Cur[I].Height := 32;
    case I of 0: Pick[I] := 'light-bis'; 1: Pick[I] := 'sunset-bis'; 2: Pick[I] := 'purple-bis'; else Pick[I] := 'green-bis'; end;
  end;
  Role := 0;
  T := Presets + ',';
  I := 0;
  while T <> '' do begin
    N := Copy(T, 1, Pos(',', T) - 1); Delete(T, 1, Pos(',', T));
    ExtractTemporaryFile(N + '.bmp');
    TileName[I] := N;
    Tile[I] := TPanel.Create(Pg);
    Tile[I].Parent := Pg.Surface; Tile[I].BevelOuter := bvNone; Tile[I].ParentBackground := False;
    Tile[I].Width := 50; Tile[I].Height := 50;
    Tile[I].Left := (I mod 8) * 52; Tile[I].Top := 68 + (I div 8) * 52;
    Img := TBitmapImage.Create(Pg);
    Img.Parent := Tile[I]; Img.Left := 1; Img.Top := 1; Img.Width := 48; Img.Height := 48;
    Img.Bitmap.LoadFromFile(ExpandConstant('{tmp}\' + N + '.bmp'));
    Img.Tag := I; Img.OnClick := @TileClick;
    Tile[I].Tag := I; Tile[I].OnClick := @TileClick;
    I := I + 1;
  end;
  B := TNewButton.Create(Pg);
  B.Parent := Pg.Surface; B.Caption := 'Use default icon'; B.Left := 6 * 52 + 4; B.Top := 68 + 2 * 52 + 12;
  B.Width := 120; B.Height := 26; B.OnClick := @DefaultClick;
  for I := 0 to 3 do Load(Cur[I], Pick[I]);
  Highlight;
end;

// role=preset for every non-default choice
function IconArgs(Param: String): String;
var I: Integer;
begin
  Result := '';
  for I := 0 to 3 do
    if Pick[I] <> '' then Result := Result + ' ' + RoleName(I) + '=' + Pick[I];
end;
