# desktop/packaging/windows.iss adds the core's directory to the user's PATH. The other
# acceptance scripts call the installed CLI by absolute path; this looks it up by name in
# a new user shell, as the user does after installing.
#
# Open a shortcut through Explorer for each probe. Windows can use a broker to
# start PowerShell. Observe the resulting shell in the same user session and
# let Windows supply its environment. Both the working and install directories
# contain spaces. A preexisting PATH entry must still work after uninstall.
#
#   VOICELESS_TEST_INSTALLERS   directory with voiceless-core-v<version>-windows-x64.exe
#   VOICELESS_TEST_CORE_SHA256  digest of that installer, so the log names the build it proved
$ErrorActionPreference = 'Stop'
if ($env:RUNNER_ENVIRONMENT -ne 'github-hosted' -or $env:RUNNER_OS -ne 'Windows') { throw 'Requires a disposable GitHub-hosted Windows guest' }

function Read-UserPath {
    # The stored text, %...% references left as written, and the type it is stored as.
    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment')
    try {
        $text = $key.GetValue('Path', $null, 'DoNotExpandEnvironmentNames')
        [pscustomobject]@{ Text = $text; Kind = if ($null -ne $text) { [string]$key.GetValueKind('Path') } else { $null } }
    } finally { $key.Dispose() }
}

function Write-UserPath($State) {
    $key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment')
    try {
        if ($null -eq $State) { $key.DeleteValue('Path', $false) }
        else { $key.SetValue('Path', $State.Text, [Microsoft.Win32.RegistryValueKind]$State.Kind) }
    } finally { $key.Dispose() }
}

function Join-UserPath($Text, $Entry) {
    if ([string]::IsNullOrEmpty($Text) -or $Text.EndsWith(';')) { "$Text$Entry" } else { "$Text;$Entry" }
}

function Read-CliPath {
    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Software\Voiceless\Installer')
    if ($null -eq $key) { return $null }
    try { $key.GetValue('CliPath') } finally { $key.Dispose() }
}

function Get-Integrity {
    if ((& "$env:WINDIR\System32\whoami.exe" /groups | Out-String) -match 'Mandatory Label\\(\w+) Mandatory Level') { $Matches[1] }
}

function Invoke-Inno([string]$Program, [string]$Switches, [string]$Log) {
    # The uninstaller can continue in a temporary copy after its launcher exits.
    # On Windows, Start-Process -Wait also waits for descendants of the launcher.
    $process = Start-Process -FilePath $Program -ArgumentList "$Switches /LOG=`"$Log`"" -Wait -PassThru
    if ($process.ExitCode -ne 0) {
        Get-Content -Tail 15 -LiteralPath $Log -ErrorAction SilentlyContinue
        throw "$Program exited with $($process.ExitCode)"
    }
}

function Get-SessionShell {
    # Identify the desktop and the account that owns this interactive session.
    Add-Type -Namespace VoicelessTest -Name Desktop -MemberDefinition @'
[DllImport("user32.dll")]
public static extern IntPtr GetShellWindow();
[DllImport("user32.dll")]
public static extern uint GetWindowThreadProcessId(IntPtr window, out uint processId);
'@
    $session = (Get-Process -Id $PID).SessionId
    $pids = @(Get-Process -Name explorer -ErrorAction SilentlyContinue | Where-Object SessionId -eq $session | ForEach-Object Id)
    $id = [uint32]0
    $desktop = [VoicelessTest.Desktop]::GetShellWindow()
    if ($desktop -ne [IntPtr]::Zero) { $null = [VoicelessTest.Desktop]::GetWindowThreadProcessId($desktop, [ref]$id) }
    if ($id -notin $pids) { throw "No desktop shell runs in session $session, where this script runs" }
    $owner = Invoke-CimMethod -InputObject (Get-CimInstance Win32_Process -Filter "ProcessId = $id") -MethodName GetOwner
    $account = "$($owner.Domain)\$($owner.User)"
    if ($account -ine "$env:USERDOMAIN\$env:USERNAME") { throw "The desktop shell runs as $account, not as this script" }
    [pscustomobject]@{ Id = $id; Session = $session; Owner = $account }
}

function New-Shortcut([string]$Path, [string]$Target, [string]$Arguments, [string]$Directory) {
    $wsh = New-Object -ComObject WScript.Shell
    $link = $wsh.CreateShortcut($Path)
    $link.TargetPath = $Target
    $link.Arguments = $Arguments
    $link.WorkingDirectory = $Directory
    $link.Save()
}

# Runs in the new shell. It reports what that shell finds and changes nothing.
$ProbeText = @'
$ErrorActionPreference = 'Stop'
$r = [ordered]@{ pid = $PID; parent = (Get-Process -Id $PID).Parent.Id }
try {
    $r.account = [System.Security.Principal.WindowsIdentity]::GetCurrent().Name
    $r.cwd = (Get-Location).Path
    $r.session = (Get-Process -Id $PID).SessionId
    if ((& "$env:WINDIR\System32\whoami.exe" /groups | Out-String) -match 'Mandatory Label\\(\w+) Mandatory Level') { $r.integrity = $Matches[1] }
    $entries = @($env:Path -split [System.IO.Path]::PathSeparator)
    $slash = [System.IO.Path]::DirectorySeparatorChar
    $r.path = [ordered]@{
        install = @($entries | Where-Object { $_.TrimEnd($slash) -ieq '__INSTALL__' }).Count
        marker = @($entries | Where-Object { $_.TrimEnd($slash) -ieq '__MARKER__' }).Count
    }
    $r.voiceless = [ordered]@{ command = (Get-Command voiceless -ErrorAction SilentlyContinue).Path }
    $r.voiceless.where = (& "$env:WINDIR\System32\where.exe" voiceless | Out-String).Trim()
    $r.voiceless.whereExit = $LASTEXITCODE
    if ($r.voiceless.command) {
        $r.voiceless.version = (& voiceless --version | Out-String).Trim()
        $r.voiceless.versionExit = $LASTEXITCODE
    }
    $r.marker = [ordered]@{ command = (Get-Command voiceless-path-sentinel -ErrorAction SilentlyContinue).Path }
    if ($r.marker.command) {
        $r.marker.output = (& voiceless-path-sentinel | Out-String).Trim()
        $r.marker.exit = $LASTEXITCODE
    }
} catch { $r.error = "$_" }
[IO.File]::WriteAllText('__RESULT__.tmp', ($r | ConvertTo-Json -Depth 4))
Move-Item -LiteralPath '__RESULT__.tmp' -Destination '__RESULT__'
'@

function Start-ShellProbe {
    # Open the shortcut normally with the desktop shell. A broker can be the
    # immediate parent of the new PowerShell process.
    Remove-Item -LiteralPath $result -ErrorAction SilentlyContinue
    $start = [System.Diagnostics.ProcessStartInfo]::new((Join-Path $env:WINDIR 'explorer.exe'), "`"$shortcut`"")
    $start.UseShellExecute = $false
    $null = [System.Diagnostics.Process]::Start($start)
    $deadline = [DateTime]::UtcNow.AddSeconds(60)
    while (-not (Test-Path -LiteralPath $result)) {
        if ([DateTime]::UtcNow -gt $deadline) { throw 'The desktop shell did not start the new shell' }
        Start-Sleep -Milliseconds 200
    }
    $seen = Get-Content -Raw -LiteralPath $result | ConvertFrom-Json
    if ($seen.error) { throw "The new shell failed: $($seen.error)" }
    if ($seen.pid -eq $PID -or $seen.account -ine $shell.Owner -or $seen.session -ne $shell.Session -or $seen.cwd -ine $work) {
        throw "Wrong user session or working directory for the new shell: $($seen | ConvertTo-Json -Compress)"
    }
    $seen
}

function Wait-ShellProbe([scriptblock]$Announced) {
    # Open a fresh shell on each attempt so that asynchronous updates can arrive.
    $deadline = [DateTime]::UtcNow.AddSeconds(20)
    $attempts = 0
    do {
        $attempts++
        $seen = Start-ShellProbe
        if (& $Announced $seen) { break }
        Start-Sleep -Milliseconds 500
    } while ([DateTime]::UtcNow -lt $deadline)
    $seen | Add-Member -NotePropertyName attempts -NotePropertyValue $attempts -PassThru
}

function Check([string]$Name, [bool]$Ok, $Detail) {
    $script:checks.Add([ordered]@{ check = $Name; ok = $Ok; detail = $Detail })
}

if (-not $env:VOICELESS_TEST_INSTALLERS -or -not $env:VOICELESS_TEST_CORE_SHA256) { throw 'VOICELESS_TEST_INSTALLERS and VOICELESS_TEST_CORE_SHA256 are required' }
$version = (Get-Content -Raw -LiteralPath (Join-Path $PSScriptRoot '..\..\..\desktop\tauri.conf.json') | ConvertFrom-Json).version
$setup = Join-Path $env:VOICELESS_TEST_INSTALLERS "voiceless-core-v$version-windows-x64.exe"
$sha256 = (Get-FileHash -LiteralPath $setup -Algorithm SHA256).Hash
if ($sha256 -ine $env:VOICELESS_TEST_CORE_SHA256) { throw "$setup is not the pinned build: $sha256" }
$root = Join-Path $env:RUNNER_TEMP 'Voiceless CLI path check'
$install = Join-Path $root 'Voiceless CLI'
$work = Join-Path $root 'outside the installation'
$marker = Join-Path $root 'preexisting user entry'
$probeScript = Join-Path $root 'probe.ps1'
$shortcut = Join-Path $root 'new shell.lnk'
$result = Join-Path $root 'result.json'
$uninstaller = Join-Path $install 'unins000.exe'
$shell = Get-SessionShell
$original = Read-UserPath
$checks = [System.Collections.Generic.List[object]]::new()
$proof = [ordered]@{
    installer = [ordered]@{ name = (Split-Path -Leaf $setup); sha256 = $sha256.ToLowerInvariant() }
    version = $version
    image = $env:ImageOS
    desktop_shell = [ordered]@{ pid = $shell.Id; session = $shell.Session; owner = $shell.Owner; script_integrity = (Get-Integrity) }
    paths = [ordered]@{ install = $install; cwd = $work; marker = $marker }
}
$touched = $false

try {
    foreach ($directory in $work, $marker) { $null = New-Item -ItemType Directory -Path $directory -Force }
    $before = [pscustomobject]@{ Text = (Join-UserPath $original.Text $marker); Kind = 'ExpandString' }
    Write-UserPath $before
    $touched = $true
    [IO.File]::WriteAllText((Join-Path $marker 'voiceless-path-sentinel.cmd'), "@echo off`r`necho voiceless-path-sentinel`r`n")
    [IO.File]::WriteAllText($probeScript, $ProbeText.Replace('__WORK__', $work).Replace('__INSTALL__', $install).Replace('__MARKER__', $marker).Replace('__RESULT__', $result))
    New-Shortcut $shortcut (Get-Process -Id $PID).Path "-NoProfile -NonInteractive -File `"$probeScript`"" $work

    $first = Wait-ShellProbe { $true }
    Check 'before install: a new shell cannot find voiceless' (-not $first.voiceless.command -and $first.voiceless.whereExit -ne 0) $first.voiceless.whereExit
    Check 'before install: the installation directory is absent from the new shell PATH' ($first.path.install -eq 0) $first.path

    Invoke-Inno $setup "/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /DIR=`"$install`"" (Join-Path $root 'install.log')
    $installed = Read-UserPath
    $text = [string]$installed.Text
    $added = if ($text.StartsWith($before.Text, [StringComparison]::Ordinal)) { $text.Substring($before.Text.Length) } else { $null }
    $slash = [System.IO.Path]::DirectorySeparatorChar
    $listed = @($text -split ';' | Where-Object { $_.TrimEnd($slash) -ieq $install }).Count
    Check 'install: the user PATH keeps its text and gains the installation once, last' (($added -ieq $install -or $added -ieq ";$install") -and $listed -eq 1 -and $installed.Kind -eq 'ExpandString') $installed
    Check 'install: the installer notes its directory for the uninstall' ((Read-CliPath) -ieq $install)
    Check 'install: this script still cannot find voiceless' (-not (Get-Command voiceless -ErrorAction SilentlyContinue))
    $live = Wait-ShellProbe { param($seen) $seen.path.install -ge 1 }
    Check 'after install: a new shell lists the installation exactly once' ($live.path.install -eq 1) $live.path
    $exe = Join-Path $install 'voiceless.exe'
    $found = @($live.voiceless.where -split '\r?\n' | Where-Object { $_ })
    Check 'after install: where.exe voiceless finds only the installed CLI' ($live.voiceless.whereExit -eq 0 -and $found.Count -eq 1 -and $found[0] -ieq $exe) $live.voiceless.where
    Check 'after install: Get-Command voiceless resolves the installed CLI' ($live.voiceless.command -ieq $exe) $live.voiceless.command
    Check 'after install: voiceless --version runs by name' ($live.voiceless.versionExit -eq 0 -and $live.voiceless.version -ceq "voiceless $version") $live.voiceless.version
    Check 'after install: the preexisting entry stays listed and its command runs' ($live.path.marker -eq 1 -and $live.marker.exit -eq 0 -and $live.marker.output -ceq 'voiceless-path-sentinel') $live.marker

    Invoke-Inno $uninstaller '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART' (Join-Path $root 'uninstall.log')
    $after = Read-UserPath
    Check 'uninstall: the user PATH is what it was before the install' (([string]$after.Text -ceq $before.Text) -and $after.Kind -eq $before.Kind) $after
    Check 'uninstall: the noted directory is forgotten' ($null -eq (Read-CliPath))
    Check 'uninstall: the installation is removed' (-not (Test-Path -LiteralPath $install))
    $last = Wait-ShellProbe { param($seen) $seen.path.install -eq 0 }
    Check 'after uninstall: a new shell no longer lists the installation' ($last.path.install -eq 0) $last.path
    Check 'after uninstall: a new shell can no longer find voiceless' (-not $last.voiceless.command -and $last.voiceless.whereExit -ne 0)
    Check 'after uninstall: the preexisting entry is still listed and its command runs' ($last.path.marker -eq 1 -and $last.marker.exit -eq 0 -and $last.marker.output -ceq 'voiceless-path-sentinel') $last.marker
    $proof.user_path = [ordered]@{ original = $original; before_install = $before; after_install = $installed; after_uninstall = $after }
    $proof.shells = [ordered]@{ before_install = $first; after_install = $live; after_uninstall = $last }
} catch {
    Write-Output "CLI_PATH_ABORTED $(@{ message = "$_"; checks = $checks } | ConvertTo-Json -Depth 6 -Compress)"
    foreach ($log in 'install.log', 'uninstall.log') {
        $file = Join-Path $root $log
        if (Test-Path -LiteralPath $file) { Write-Output "--- $log"; Get-Content -Tail 12 -LiteralPath $file }
    }
    throw
} finally {
    # The guest is disposable; this keeps a failed run from leaving the entry behind anyway.
    if ($touched) {
        if (Test-Path -LiteralPath $uninstaller) { try { Invoke-Inno $uninstaller '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART' (Join-Path $root 'cleanup.log') } catch { Write-Warning "Cleanup uninstall failed: $_" } }
        Write-UserPath $original
        Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue
    }
}

$proof.checks = $checks
Write-Output "CLI_PATH $($proof | ConvertTo-Json -Depth 7 -Compress)"
$failed = @($checks | Where-Object { -not $_.ok }).Count
if ($failed) { throw "$failed of $($checks.Count) checks failed" }
