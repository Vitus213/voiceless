; Both installers are per-user. The desktop owns a matching adjacent daemon.
#ifdef Desktop
  #define Product "Voiceless Desktop"
  #define Kind "desktop"
#else
  #define Product "Voiceless CLI"
  #define Kind "core"
#endif

[Setup]
AppId=dev.voiceless.{#Kind}
AppName={#Product}
AppVersion={#Version}
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequired=lowest
DefaultDirName={localappdata}\Programs\{#Product}
DisableProgramGroupPage=yes
OutputBaseFilename=voiceless-{#Kind}-v{#Version}-windows-x64
ChangesEnvironment=yes
LicenseFile=..\..\LICENSE
Compression=lzma2
SolidCompression=yes

[Files]
Source: "{#Payload}\voiceless.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Payload}\*.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
#ifdef Desktop
Source: "{#Payload}\voiceless-desktop.exe"; DestDir: "{app}"; Flags: ignoreversion
[Icons]
Name: "{userprograms}\Voiceless"; Filename: "{app}\voiceless-desktop.exe"
#endif

[Code]
#ifdef Desktop
const
  RuntimeKey = 'Software\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}';
function HasWebView2(Root: Integer): Boolean;
var
  Version: String;
  Parsed: Int64;
begin
  Result := RegQueryStringValue(Root, RuntimeKey, 'pv', Version)
    and StrToVersion(Version, Parsed) and (Parsed > 0);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  if not (HasWebView2(HKLM32) or HasWebView2(HKCU)) then
    Result := 'Install Microsoft Edge WebView2 Evergreen Runtime, then run setup again.';
end;
#else
// Retain the existing expandable PATH and remove only the entry we added.
procedure CurStepChanged(Step: TSetupStep);
var Value, Dir: String;
begin
  if Step <> ssPostInstall then exit;
  Dir := ExpandConstant('{app}');
  RegQueryStringValue(HKCU, 'Environment', 'Path', Value);
  if Pos(';' + Lowercase(Dir) + ';', ';' + Lowercase(Value) + ';') = 0 then begin
    if Value = '' then Value := Dir
    else if Value[Length(Value)] = ';' then Value := Value + Dir + ';'
    else Value := Value + ';' + Dir;
    if not RegWriteExpandStringValue(HKCU, 'Environment', 'Path', Value) then RaiseException('Cannot add CLI to user PATH');
    RegWriteStringValue(HKCU, 'Software\Voiceless\Installer', 'CliPath', Dir);
  end;
end;

procedure CurUninstallStepChanged(Step: TUninstallStep);
var Value, Dir: String; I: Integer;
begin
  if Step <> usPostUninstall then exit;
  if not RegQueryStringValue(HKCU, 'Software\Voiceless\Installer', 'CliPath', Dir) then exit;
  if RegQueryStringValue(HKCU, 'Environment', 'Path', Value) then begin
    Value := ';' + Value + ';';
    I := Pos(';' + Lowercase(Dir) + ';', Lowercase(Value));
    if I > 0 then begin
      Delete(Value, I + 1, Length(Dir) + 1);
      RegWriteExpandStringValue(HKCU, 'Environment', 'Path', Copy(Value, 2, Length(Value) - 2));
    end;
  end;
  RegDeleteValue(HKCU, 'Software\Voiceless\Installer', 'CliPath');
end;
#endif
