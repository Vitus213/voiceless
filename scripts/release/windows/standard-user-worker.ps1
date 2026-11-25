# One step in the life of the installed desktop for the standard user who
# installed it. The elevated harness (standard-user.ps1) starts this script with
# that user's credentials, so nothing here runs with administrative rights; the
# account is one the harness created on the disposable guest.
#
#   pwsh -STA -File standard-user-worker.ps1 -Step <step> -Root <dir> -Candidates <dir> -Speech <wav> -Evidence <dir> -Run <n>
#
# Each step appends to worker-<step>-<n>.log in the evidence directory and leaves
# its findings in result-<step>-<n>.json there.
param(
    [Parameter(Mandatory)][ValidateSet('identity', 'install', 'editor', 'inject', 'desktop-start', 'desktop-relaunch', 'ctl', 'uninstall', 'lookup')][string]$Step,
    [Parameter(Mandatory)][string]$Root,
    [Parameter(Mandatory)][string]$Candidates,
    [Parameter(Mandatory)][string]$Speech,
    [Parameter(Mandatory)][string]$Evidence,
    [string]$Run = '1'
)
$ErrorActionPreference = 'Stop'
Add-Type -Path (Join-Path $PSScriptRoot 'ProcessToken.cs')
Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;

namespace VoicelessWorker {
  // The profile directory of the account a token belongs to.
  public static class Profile {
    [DllImport("userenv.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool GetUserProfileDirectoryW(IntPtr token, IntPtr buffer, ref uint size);

    public static string Directory(IntPtr token) {
      uint size = 0;
      GetUserProfileDirectoryW(token, IntPtr.Zero, ref size);
      IntPtr buffer = Marshal.AllocHGlobal((IntPtr)(size * 2 + 2));
      try {
        if (!GetUserProfileDirectoryW(token, buffer, ref size)) {
          throw new Win32Exception(Marshal.GetLastWin32Error(), "GetUserProfileDirectoryW");
        }
        return Marshal.PtrToStringUni(buffer);
      } finally {
        Marshal.FreeHGlobal(buffer);
      }
    }
  }
}
'@

# The secondary logon service starts this script with the environment of the
# harness that asked for it. A console this user opens has the environment of
# their own account; it is rebuilt here the way a logon builds it.
function Enter-OwnEnvironment {
    $identity = [System.Security.Principal.WindowsIdentity]::GetCurrent()
    $domain, $user = $identity.Name.Split('\')
    $directory = [VoicelessWorker.Profile]::Directory($identity.Token)
    $root = [IO.Path]::GetPathRoot($directory)
    $environment = [ordered]@{
        USERPROFILE = $directory
        USERNAME = $user
        USERDOMAIN = $domain
        HOMEDRIVE = $root.TrimEnd('\')
        HOMEPATH = $directory.Substring($root.Length - 1)
        APPDATA = Join-Path $directory 'AppData\Roaming'
        LOCALAPPDATA = Join-Path $directory 'AppData\Local'
        TEMP = Join-Path $directory 'AppData\Local\Temp'
        TMP = Join-Path $directory 'AppData\Local\Temp'
    }
    $expand = [regex]::new('%([^%]+)%')
    $resolve = [System.Text.RegularExpressions.MatchEvaluator] { param($match) if ($environment.Contains($match.Groups[1].Value)) { [string]$environment[$match.Groups[1].Value] } else { $match.Value } }
    $ofDevice, $ofUser = [ordered]@{}, [ordered]@{}
    $machine = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey('SYSTEM\CurrentControlSet\Control\Session Manager\Environment')
    $own = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment')
    try {
        foreach ($pair in @(@($machine, $ofDevice), @($own, $ofUser))) {
            foreach ($name in $pair[0].GetValueNames()) {
                $pair[1][$name] = [string]$pair[0].GetValue($name, $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
            }
        }
    } finally { $machine.Dispose(); $own.Dispose() }
    foreach ($name in $ofDevice.Keys) { $environment[$name] = $expand.Replace($ofDevice[$name], $resolve) }
    foreach ($name in $ofUser.Keys) { if ($name -ine 'Path') { $environment[$name] = $expand.Replace($ofUser[$name], $resolve) } }
    $environment['Path'] = @(@($ofDevice['Path'], $ofUser['Path']) | ForEach-Object { if ($_) { $expand.Replace($_, $resolve) } }) -join ';'
    foreach ($name in @($environment.Keys)) { [Environment]::SetEnvironmentVariable($name, [string]$environment[$name]) }
    foreach ($place in $env:TEMP, $env:LOCALAPPDATA, $env:APPDATA) { $null = New-Item -ItemType Directory -Path $place -Force }
}
Enter-OwnEnvironment
$env:XDG_CONFIG_HOME = Join-Path $Root 'config'
$env:XDG_DATA_HOME = Join-Path $Root 'data'
$env:XDG_CACHE_HOME = Join-Path $Root 'cache'
$log = Join-Path $Evidence "worker-$Step-$Run.log"

function Note([string]$Text) {
    Add-Content -LiteralPath $log -Value ('{0:HH:mm:ss.fff} {1}' -f [DateTime]::Now, $Text) -Encoding utf8
}

# Written whole and then renamed: the harness polls for these files and must
# never read a half-written one.
function Save-Json([string]$Path, $Value) {
    $Value | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath "$Path.partial" -Encoding utf8
    Move-Item -LiteralPath "$Path.partial" -Destination $Path -Force
}

function Save-Result($Value) {
    Save-Json (Join-Path $Evidence "result-$Step-$Run.json") $Value
}

function Get-InstalledCli {
    Join-Path $env:LOCALAPPDATA 'Programs\Voiceless CLI\voiceless.exe'
}

function Get-InstalledDesktop {
    Join-Path $env:LOCALAPPDATA 'Programs\Voiceless Desktop\voiceless-desktop.exe'
}

function Get-InstalledShortcut {
    Join-Path ([Environment]::GetFolderPath('Programs')) 'Voiceless.lnk'
}

function Get-ShortcutTarget([string]$Shortcut) {
    if (!(Test-Path -LiteralPath $Shortcut)) { return $null }
    $shell = New-Object -ComObject WScript.Shell
    try { $shell.CreateShortcut($Shortcut).TargetPath } finally { $null = [System.Runtime.InteropServices.Marshal]::FinalReleaseComObject($shell) }
}

function Get-UninstallEntries {
    @(Get-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*' -ErrorAction SilentlyContinue |
        Where-Object { $_.DisplayName -like 'Voiceless*' } |
        ForEach-Object { [ordered]@{ name = $_.DisplayName; version = $_.DisplayVersion; location = $_.InstallLocation } })
}

# How a console this user opens later finds `voiceless`, and what the user's PATH holds.
function Get-Lookup {
    $resolved = @(Get-Command voiceless -ErrorAction SilentlyContinue | ForEach-Object Source)
    $found = @(& where.exe voiceless 2>$null | Where-Object { $_ })
    $environment = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment')
    $path, $kind = $null, $null
    if ($environment) {
        try {
            $path = $environment.GetValue('Path', $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
            $kind = if ($null -ne $path) { [string]$environment.GetValueKind('Path') } else { $null }
        } finally { $environment.Dispose() }
    }
    $version = if ($resolved.Count) { (& $resolved[0] --version | Out-String).Trim() } else { $null }
    [ordered]@{ resolved = $resolved; where = $found; path = $path; pathKind = $kind; version = $version }
}

# Run a console program as this user and collect what it printed.
function Invoke-Console([string]$Executable, [string[]]$Arguments, [scriptblock]$WhileRunning) {
    $process = [System.Diagnostics.Process]::new()
    $info = $process.StartInfo
    $info.FileName = $Executable
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $info.StandardOutputEncoding = [System.Text.Encoding]::UTF8
    $info.StandardErrorEncoding = [System.Text.Encoding]::UTF8
    foreach ($argument in $Arguments) { $info.ArgumentList.Add($argument) }
    $started = $false
    try {
        $started = $process.Start()
        $output = $process.StandardOutput.ReadToEndAsync()
        $errors = $process.StandardError.ReadToEndAsync()
        $timer = [System.Diagnostics.Stopwatch]::StartNew()
        while (!$process.HasExited) {
            if ($WhileRunning) { & $WhileRunning }
            if ($timer.Elapsed.TotalSeconds -ge 180) { throw "Installed program did not finish in 180 seconds: $Executable $($Arguments -join ' ')" }
            Start-Sleep -Milliseconds 15
        }
        $process.WaitForExit()
        [ordered]@{ exit = $process.ExitCode; stdout = $output.GetAwaiter().GetResult(); stderr = $errors.GetAwaiter().GetResult() }
    } finally {
        if ($started -and !$process.HasExited) { $process.Kill(); $null = $process.WaitForExit(10000) }
        $process.Dispose()
    }
}

function Read-Events([string]$Text) {
    @($Text -split '\r?\n' | Where-Object { $_ } | ForEach-Object { $_ | ConvertFrom-Json })
}

try {
    switch ($Step) {
        'identity' {
            $account = [System.Security.Principal.WindowsIdentity]::GetCurrent()
            $principal = [System.Security.Principal.WindowsPrincipal]::new($account)
            $desktop = 'ok'
            try {
                # A window handle proves this account may use the interactive desktop.
                Add-Type -AssemblyName System.Windows.Forms
                $probe = [System.Windows.Forms.Form]::new()
                $null = $probe.Handle
                $probe.Dispose()
            } catch { $desktop = $_.Exception.Message }
            $lookup = Get-Lookup
            Save-Result ([ordered]@{
                account = $account.Name
                sid = $account.User.Value
                administrator = $principal.IsInRole([System.Security.Principal.WindowsBuiltInRole]::Administrator)
                integrity = [ProcessToken]::Integrity([uint32]$PID)
                profile = $env:USERPROFILE
                localAppData = $env:LOCALAPPDATA
                temp = $env:TEMP
                processPath = $env:Path
                desktop = $desktop
                groups = @(& whoami.exe /groups /fo csv /nh | Where-Object { $_ })
                path = $lookup.path
                pathKind = $lookup.pathKind
                resolved = $lookup.resolved
            })
            Note "$($account.Name) runs at integrity $('0x{0:X4}' -f [ProcessToken]::Integrity([uint32]$PID)); desktop $desktop"
        }
        'install' {
            $flags = '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART'
            $installers = @()
            foreach ($kind in 'core', 'desktop') {
                $setup = @(Get-ChildItem -LiteralPath $Candidates -Filter "voiceless-$kind-v*-windows-x64.exe")
                if ($setup.Count -ne 1) { throw "Expected one $kind installer, found $($setup.Count)" }
                $process = Start-Process -FilePath $setup[0].FullName -ArgumentList $flags -PassThru
                if (!$process.WaitForExit(240000)) { $process.Kill(); throw "The $kind installer did not finish" }
                $installers += [ordered]@{
                    kind = $kind
                    installer = $setup[0].Name
                    sha256 = (Get-FileHash -LiteralPath $setup[0].FullName -Algorithm SHA256).Hash
                    exit = $process.ExitCode
                }
                Note "$kind installer $($setup[0].Name) exited with $($process.ExitCode)"
            }
            $cli = Get-InstalledCli
            $desktopExecutable = Get-InstalledDesktop
            $shortcut = Get-InstalledShortcut
            $target = Get-ShortcutTarget $shortcut
            Save-Result ([ordered]@{
                integrity = [ProcessToken]::Integrity([uint32]$PID)
                installers = $installers
                cli = $cli
                cliPresent = Test-Path -LiteralPath $cli
                desktop = $desktopExecutable
                desktopPresent = Test-Path -LiteralPath $desktopExecutable
                version = (& $cli --version | Out-String).Trim()
                shortcut = $shortcut
                shortcutPresent = Test-Path -LiteralPath $shortcut
                shortcutTarget = $target
                uninstallEntries = Get-UninstallEntries
                doctor = Invoke-Console $cli @('doctor')
            })
        }
        'editor' {
            Add-Type -AssemblyName System.Windows.Forms, System.Drawing
            Add-Type -Namespace VoicelessWorker -Name Editor -MemberDefinition @'
[DllImport("user32.dll")]
public static extern IntPtr GetForegroundWindow();
[DllImport("user32.dll")]
public static extern bool ShowWindow(IntPtr window, int command);
'@
            $cli = Get-InstalledCli
            $form = [System.Windows.Forms.Form]::new()
            $form.Text = 'Voiceless standard-user acceptance'
            $form.StartPosition = 'Manual'
            $form.Location = [System.Drawing.Point]::new(120, 120)
            $form.Size = [System.Drawing.Size]::new(720, 360)
            $field = [System.Windows.Forms.TextBox]::new()
            $field.Multiline = $true
            $field.Dock = 'Fill'
            $form.Controls.Add($field)
            $timer = [System.Diagnostics.Stopwatch]::StartNew()
            try {
                $form.Show()
                # SW_SHOW: the window is shown whatever the hidden console's startup asked for.
                $null = [VoicelessWorker.Editor]::ShowWindow($form.Handle, 5)
                $null = $field.Focus()
                $sentinel = 'Voiceless standard-user clipboard'
                $before = 'not set'
                try {
                    [System.Windows.Forms.Clipboard]::SetText($sentinel)
                    $before = if ([System.Windows.Forms.Clipboard]::GetText() -ceq $sentinel) { 'sentinel' } else { 'unreadable' }
                } catch { $before = "error: $($_.Exception.Message)" }
                Save-Json (Join-Path $Evidence 'editor.ready') ([ordered]@{ form = $form.Handle.ToInt64() })
                # The harness clicks the field, as a person does before dictating into it.
                while ($timer.Elapsed.TotalSeconds -lt 45 -and ([VoicelessWorker.Editor]::GetForegroundWindow() -ne $form.Handle -or !$field.Focused)) {
                    [System.Windows.Forms.Application]::DoEvents()
                    Start-Sleep -Milliseconds 15
                }
                if ([VoicelessWorker.Editor]::GetForegroundWindow() -ne $form.Handle -or !$field.Focused) { throw 'The editor did not obtain the foreground focus' }
                Note 'The editor has the focus; the installed CLI transcribes and delivers'
                $run = Invoke-Console $cli @('transcribe', $Speech, '--backend', 'local', '--inject', '--json') {
                    [System.Windows.Forms.Application]::DoEvents()
                }
                [System.Windows.Forms.Application]::DoEvents()
                $events = Read-Events $run.stdout
                $transcript = $events | Where-Object type -eq 'transcript' | Select-Object -First 1
                $delivery = $events | Where-Object type -eq 'finalized' | Select-Object -First 1
                $after = try { [System.Windows.Forms.Clipboard]::GetText() } catch { "error: $($_.Exception.Message)" }
                Save-Result ([ordered]@{
                    integrity = [ProcessToken]::Integrity([uint32]$PID)
                    exit = $run.exit
                    transcript = $transcript.text
                    injection = $delivery.injection
                    stderr = $run.stderr
                    editor = $field.Text
                    clipboardBefore = $before
                    clipboardAfter = $after
                })
                Note "CLI exit $($run.exit); the editor holds $($field.Text.Length) characters"
                # Stay open until the harness has recorded the screen.
                while (!(Test-Path -LiteralPath (Join-Path $Evidence 'editor.release')) -and $timer.Elapsed.TotalSeconds -lt 150) {
                    [System.Windows.Forms.Application]::DoEvents()
                    Start-Sleep -Milliseconds 15
                }
            } finally { $form.Dispose() }
        }
        'inject' {
            $run = Invoke-Console (Get-InstalledCli) @('transcribe', $Speech, '--backend', 'local', '--inject', '--json')
            $events = Read-Events $run.stdout
            $transcript = $events | Where-Object type -eq 'transcript' | Select-Object -First 1
            $delivery = $events | Where-Object type -eq 'finalized' | Select-Object -First 1
            Save-Result ([ordered]@{
                integrity = [ProcessToken]::Integrity([uint32]$PID)
                exit = $run.exit
                transcript = $transcript.text
                injection = $delivery.injection
                stderr = $run.stderr
            })
            Note "CLI exit $($run.exit)"
        }
        'desktop-start' {
            $executable = Get-InstalledDesktop
            $process = Start-Process -FilePath $executable -PassThru -RedirectStandardError (Join-Path $Evidence "desktop-stderr-$Run.log")
            Save-Result ([ordered]@{
                process = $process.Id
                executable = $executable
                integrity = [ProcessToken]::Integrity([uint32]$process.Id)
            })
            Note "Desktop process $($process.Id) started from $executable"
        }
        'desktop-relaunch' {
            $process = Start-Process -FilePath (Get-InstalledDesktop) -PassThru
            $exited = $process.WaitForExit(15000)
            Save-Result ([ordered]@{ exited = $exited; exit = $(if ($exited) { $process.ExitCode } else { $null }) })
            if (!$exited) { throw 'The second launcher stayed open instead of reusing the resident desktop' }
            Note "Second launcher exited with $($process.ExitCode)"
        }
        'ctl' {
            $run = Invoke-Console (Get-InstalledCli) @('ctl', 'status')
            $descriptor = Join-Path $env:XDG_DATA_HOME 'voiceless\console-endpoint.json'
            $runtime = $null
            if (Test-Path -LiteralPath $descriptor) { $runtime = (Get-Content -Raw -LiteralPath $descriptor | ConvertFrom-Json).runtime_id }
            $lookup = Get-Lookup
            Save-Result ([ordered]@{
                exit = $run.exit
                status = ($run.stdout + $run.stderr).Trim()
                runtimeId = $runtime
                resolved = $lookup.resolved
                where = $lookup.where
                path = $lookup.path
                pathKind = $lookup.pathKind
                version = $lookup.version
            })
            Note "voiceless ctl status exited with $($run.exit)"
        }
        'uninstall' {
            $flags = '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART'
            $uninstallers = @()
            foreach ($directory in (Split-Path (Get-InstalledCli)), (Split-Path (Get-InstalledDesktop))) {
                $uninstaller = Join-Path $directory 'unins000.exe'
                if (!(Test-Path -LiteralPath $uninstaller)) { $uninstallers += [ordered]@{ uninstaller = $uninstaller; exit = 'missing' }; continue }
                $process = Start-Process -FilePath $uninstaller -ArgumentList $flags -PassThru
                if (!$process.WaitForExit(180000)) { $process.Kill(); throw "The uninstaller in $directory did not finish" }
                $uninstallers += [ordered]@{ uninstaller = $uninstaller; exit = $process.ExitCode }
                Note "Uninstaller in $directory exited with $($process.ExitCode)"
            }
            Start-Sleep -Seconds 3
            foreach ($uninstaller in $uninstallers) { $uninstaller.remains = Test-Path -LiteralPath (Split-Path $uninstaller.uninstaller) }
            Save-Result ([ordered]@{ uninstallers = $uninstallers })
        }
        'lookup' {
            $lookup = Get-Lookup
            Save-Result ([ordered]@{
                resolved = $lookup.resolved
                where = $lookup.where
                path = $lookup.path
                pathKind = $lookup.pathKind
                shortcut = Test-Path -LiteralPath (Get-InstalledShortcut)
                cliDirectory = Test-Path -LiteralPath (Split-Path (Get-InstalledCli))
                desktopDirectory = Test-Path -LiteralPath (Split-Path (Get-InstalledDesktop))
                uninstallEntries = Get-UninstallEntries
            })
        }
    }
    exit 0
} catch {
    Note "FAILED: $($_.Exception.Message)"
    Note ([string]$_.ScriptStackTrace)
    exit 1
}
