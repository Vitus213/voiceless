# The installed desktop as a user without administrative rights sees it. A
# standard account is created on this disposable guest; it installs both
# candidates and runs the editor, the CLI and the desktop itself at medium
# integrity. This elevated harness only observes, moves the pointer for it and
# stands in as the elevated window that must not receive a paste.
#
#   pwsh -NoProfile -STA -File standard-user.ps1 -Candidates <dir> -Root <dir> -Speech <wav> -Evidence <dir>
#
# The process starts and pointer clicks are synthetic; no person took part.
param(
    [Parameter(Mandatory)][string]$Candidates,
    [Parameter(Mandatory)][string]$Root,
    [Parameter(Mandatory)][string]$Speech,
    [Parameter(Mandatory)][string]$Evidence
)
$ErrorActionPreference = 'Stop'
if ($env:RUNNER_ENVIRONMENT -ne 'github-hosted' -or $env:RUNNER_OS -ne 'Windows') { throw 'Requires a disposable GitHub-hosted Windows guest' }
New-Item -Type Directory -Path $Evidence -Force | Out-Null
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
Add-Type -Path (Join-Path $PSScriptRoot 'ProcessToken.cs'), (Join-Path $PSScriptRoot 'AcceptanceDesktop.cs')

$report = [ordered]@{
    input = 'Synthetic only: process starts and mouse_event pointer clicks from the harness; no physical key, button or microphone took part.'
}

function Wait-For([scriptblock]$Probe, [string]$Failure, [int]$Seconds = 30) {
    $deadline = [DateTime]::UtcNow.AddSeconds($Seconds)
    do {
        $found = & $Probe
        if ($found) { return $found }
        Start-Sleep -Milliseconds 150
    } while ([DateTime]::UtcNow -lt $deadline)
    throw $Failure
}

function Save-Screen([string]$Name) {
    try {
        $bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
        $bitmap = [System.Drawing.Bitmap]::new($bounds.Width, $bounds.Height)
        try {
            $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
            try {
                $graphics.CopyFromScreen($bounds.Location, [System.Drawing.Point]::Empty, $bounds.Size)
            } finally { $graphics.Dispose() }
            $bitmap.Save((Join-Path $Evidence "$Name.png"), [System.Drawing.Imaging.ImageFormat]::Png)
        } finally { $bitmap.Dispose() }
    } catch { Write-Output "No screenshot $Name : $($_.Exception.Message)" }
}

function Get-Sid([int]$OfProcess) { try { [ProcessToken]::Account([uint32]$OfProcess) } catch { $null } }
function Get-Level([int]$OfProcess) { try { [ProcessToken]::Integrity([uint32]$OfProcess) } catch { $null } }
function Format-Level($Level) { if ($null -eq $Level) { 'unknown' } else { '0x{0:X4}' -f $Level } }

function Get-RuntimeId {
    $descriptor = Join-Path $Root 'data\voiceless\console-endpoint.json'
    if (!(Test-Path -LiteralPath $descriptor)) { return $null }
    (Get-Content -Raw -LiteralPath $descriptor | ConvertFrom-Json).runtime_id
}

function Hide-DesktopWindow([uint32]$OfProcess, [IntPtr]$Window) {
    if (![AcceptanceDesktop]::RequestClose($Window)) { throw 'The desktop window did not accept WM_CLOSE' }
    $null = Wait-For { [AcceptanceDesktop]::VisibleWindow($OfProcess) -eq [IntPtr]::Zero } 'The Voiceless window did not hide' 15
}

function New-Editor([string]$Title, [string]$Text) {
    $form = [System.Windows.Forms.Form]::new()
    $form.Text = $Title
    $form.StartPosition = 'Manual'
    $form.Location = [System.Drawing.Point]::new(120, 120)
    $form.Size = [System.Drawing.Size]::new(720, 360)
    $field = [System.Windows.Forms.TextBox]::new()
    $field.Multiline = $true
    $field.Dock = 'Fill'
    $field.Text = $Text
    $form.Controls.Add($field)
    $form.Show()
    $form.Activate()
    $null = $field.Focus()
    [System.Windows.Forms.Application]::DoEvents()
    , @($form, $field)
}

# The focus of the harness's own window: waited for, then clicked for as a person would.
function Wait-OwnFocus($Form, $Field, [string]$Failure) {
    $timer = [System.Diagnostics.Stopwatch]::StartNew()
    while ($timer.Elapsed.TotalSeconds -lt 5 -and ([AcceptanceDesktop]::GetForegroundWindow() -ne $Form.Handle -or !$Field.Focused)) {
        [System.Windows.Forms.Application]::DoEvents()
        Start-Sleep -Milliseconds 15
    }
    if ([AcceptanceDesktop]::GetForegroundWindow() -ne $Form.Handle) { [AcceptanceDesktop]::Click($Form.Handle) }
    $timer.Restart()
    while ($timer.Elapsed.TotalSeconds -lt 10 -and ([AcceptanceDesktop]::GetForegroundWindow() -ne $Form.Handle -or !$Field.Focused)) {
        [System.Windows.Forms.Application]::DoEvents()
        Start-Sleep -Milliseconds 15
    }
    if ([AcceptanceDesktop]::GetForegroundWindow() -ne $Form.Handle -or !$Field.Focused) { throw $Failure }
}

$session = (Get-Process -Id $PID).SessionId
$report.guest = [ordered]@{
    account = [System.Security.Principal.WindowsIdentity]::GetCurrent().Name
    integrity = Format-Level (Get-Level $PID)
    session = $session
    explorer = @(Get-Process -Name explorer -ErrorAction SilentlyContinue | Where-Object SessionId -eq $session | ForEach-Object {
        [ordered]@{ id = $_.Id; account = Get-Sid $_.Id; integrity = Format-Level (Get-Level $_.Id) }
    })
}
if ((Get-Service -Name seclogon).Status -eq 'Stopped') {
    # The service behind "Run as a different user"; a disabled one is left as it is.
    Start-Service -Name seclogon
    Write-Output 'Started the Secondary Logon service, which was stopped'
}
$report.guest.secondaryLogon = [string](Get-Service -Name seclogon).Status

# The standard account and its access to the working directory that holds the
# model, the audio and the scripts, to the evidence directory it writes to, and
# to the two installers.
$name = 'VoicelessStandard'
$bytes = [byte[]]::new(18)
[System.Security.Cryptography.RandomNumberGenerator]::Fill($bytes)
$plain = [Convert]::ToHexString($bytes) + 'a1!A'
$account = ([ADSI]'WinNT://.').Create('User', $name)
try {
    # ADS_UF_DONT_EXPIRE_PASSWD 0x10000 with ADS_UF_PASSWD_CANT_CHANGE 0x40
    $account.SetPassword($plain)
    $account.UserFlags = 0x10040
    $null = $account.SetInfo()
} catch {
    throw "The standard user could not be created: $($_.Exception.Message)"
}
$credential = [pscredential]::new("$env:COMPUTERNAME\$name", (ConvertTo-SecureString $plain -AsPlainText -Force))
$report.standardAccount = $name
$workerDirectory = Join-Path $Root 'worker'
New-Item -Type Directory -Path $workerDirectory -Force | Out-Null
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'standard-user-worker.ps1'), (Join-Path $PSScriptRoot 'ProcessToken.cs') -Destination $workerDirectory
& icacls.exe $Root /grant "${name}:(OI)(CI)M" /T /Q | Out-Null
if ($LASTEXITCODE -ne 0) { throw "The standard user could not be given access to the working directory ($LASTEXITCODE)" }
& icacls.exe $Evidence /grant "${name}:(OI)(CI)M" /Q | Out-Null
& icacls.exe $Candidates /grant "${name}:(OI)(CI)RX" /Q | Out-Null
if ($LASTEXITCODE -ne 0) { throw "The standard user could not be given access to the evidence directory and the installers ($LASTEXITCODE)" }
$worker = Join-Path $workerDirectory 'standard-user-worker.ps1'

$script:run = 0
$script:installed = $null

# Start one worker step as the standard user; its process comes back while it runs.
function Start-Worker([string]$Step) {
    $script:run++
    $arguments = @('-NoProfile', '-NonInteractive', '-STA', '-File', $worker, '-Step', $Step, '-Root', $Root, '-Candidates', $Candidates,
        '-Speech', $Speech, '-Evidence', $Evidence, '-Run', "$script:run") |
        ForEach-Object { if ($_ -match '\s') { '"' + $_ + '"' } else { $_ } }
    Start-Process -FilePath (Join-Path $PSHOME 'pwsh.exe') -Credential $credential -WorkingDirectory $Root -WindowStyle Hidden -LoadUserProfile -PassThru -ArgumentList $arguments
}

# Wait for a worker step and report its exit code.
function Wait-Worker($Process, [string]$Step, [int]$Seconds = 300) {
    if (!$Process.WaitForExit($Seconds * 1000)) { $Process.Kill(); throw "The $Step step did not finish in $Seconds seconds" }
    $Process.ExitCode
}

function Get-WorkerResult([string]$Step) {
    Get-Content -Raw -LiteralPath (Join-Path $Evidence "result-$Step-$script:run.json") | ConvertFrom-Json
}

# One worker step that must succeed, and its findings.
function Invoke-Step([string]$Step, [string]$Failure) {
    $process = Start-Worker $Step
    $code = Wait-Worker $process $Step
    if ($code -ne 0) { throw "$Failure (exit $code)" }
    Get-WorkerResult $Step
}

function Stop-StandardVoiceless {
    if (!$script:installed) { return }
    Get-Process -Name voiceless, voiceless-desktop -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -and $_.Path.StartsWith($script:installed, [StringComparison]::OrdinalIgnoreCase) } |
        Stop-Process -Force
}

# The runner account's Explorer leaves and the standard user's own Explorer
# becomes the shell of this session, so that the taskbar runs where a signed-in
# user's taskbar runs: at the integrity level of that user's programs.
function Use-StandardShell {
    Get-Process -Name explorer -ErrorAction SilentlyContinue | Where-Object SessionId -eq $session | Stop-Process -Force
    Start-Sleep -Seconds 2
    Start-Process -FilePath (Join-Path $env:WINDIR 'explorer.exe') -Credential $credential -LoadUserProfile | Out-Null
    Wait-For {
        Get-Process -Name explorer -ErrorAction SilentlyContinue |
            Where-Object { $_.SessionId -eq $session -and [AcceptanceDesktop]::OwnsTaskbar([uint32]$_.Id) -and (Get-Sid $_.Id) -eq $identity.sid } |
            Select-Object -First 1
    } 'The standard user has no taskbar after their Explorer started' 60 | Out-Null
}

function Restore-RunnerShell {
    Get-Process -Name explorer -ErrorAction SilentlyContinue | Where-Object { (Get-Sid $_.Id) -eq $identity.sid } | Stop-Process -Force
    Start-Sleep -Seconds 2
    Start-Process -FilePath (Join-Path $env:WINDIR 'explorer.exe') | Out-Null
}

$identity = $null
$shellReplaced = $false
try {
    # The account as Windows runs it.
    $identity = Invoke-Step identity 'The standard user could not be started on the interactive desktop'
    $script:installed = Join-Path $identity.profile 'AppData\Local\Programs'
    $report.standardUser = [ordered]@{
        account = $identity.account
        sid = $identity.sid
        profile = $identity.profile
        administrator = $identity.administrator
        integrity = Format-Level $identity.integrity
        desktop = $identity.desktop
        groups = @($identity.groups)
    }
    if ($identity.integrity -ne 0x2000) { throw "Windows runs the standard user at $(Format-Level $identity.integrity), not at medium integrity" }
    if ($identity.administrator) { throw "Windows treats $name as an administrator" }
    if ($identity.desktop -ne 'ok') { throw "The standard user cannot show a window on the interactive desktop: $($identity.desktop)" }
    Write-Output "Standard user $($identity.account) runs at $(Format-Level $identity.integrity); the harness runs at $(Format-Level (Get-Level $PID))"

    # Per-user installation without administrative rights.
    $install = Invoke-Step install 'The standard user could not install both candidates'
    $refused = @($install.installers | Where-Object { $_.exit -ne 0 })
    if ($refused.Count) { throw "An installer refused the standard user: $(($refused | ForEach-Object { "$($_.kind) exit $($_.exit)" }) -join ', ')" }
    $expected = 'voiceless ' + [regex]::Match($install.installers[0].installer, '-v(\d+\.\d+\.\d+)-').Groups[1].Value
    if (!$install.cliPresent -or !$install.desktopPresent) { throw 'The standard user has no installed programs' }
    if ($install.version -ne $expected) { throw "The installed CLI reports '$($install.version)', expected '$expected'" }
    if (!$install.shortcutPresent -or $install.shortcutTarget -ne $install.desktop) { throw 'The Start menu entry is missing or does not start the installed desktop' }
    $report.installation = [ordered]@{
        integrity = Format-Level $install.integrity
        installers = @($install.installers)
        cli = $install.cli
        desktop = $install.desktop
        version = $install.version
        shortcut = $install.shortcut
        shortcutTarget = $install.shortcutTarget
        uninstallEntries = @($install.uninstallEntries)
        doctor = $install.doctor
    }
    Write-Output "Installed as $($identity.account) at $(Format-Level $install.integrity) under $script:installed; voiceless --version says '$($install.version)'"

    # Delivery into the standard user's own editor.
    $process = Start-Worker editor
    $ready = Wait-For {
        $path = Join-Path $Evidence 'editor.ready'
        if (Test-Path -LiteralPath $path) { Get-Content -Raw -LiteralPath $path | ConvertFrom-Json }
        elseif ($process.HasExited) { throw 'The editor step ended before its window appeared' }
    } "The standard user's editor did not appear" 90
    $editorWindow = [IntPtr]::new([long]$ready.form)
    Start-Sleep -Milliseconds 500
    if ([AcceptanceDesktop]::GetForegroundWindow() -ne $editorWindow) { [AcceptanceDesktop]::Click($editorWindow) }
    $paste = Wait-For {
        $path = Join-Path $Evidence "result-editor-$script:run.json"
        if (Test-Path -LiteralPath $path) { Get-Content -Raw -LiteralPath $path | ConvertFrom-Json }
        elseif ($process.HasExited) { throw 'The editor step ended without a result' }
    } "The delivery into the standard user's editor did not finish" 300
    Save-Screen 'editor-standard-user'
    New-Item -ItemType File -Path (Join-Path $Evidence 'editor.release') -Force | Out-Null
    $null = Wait-Worker $process editor
    if ($paste.exit -ne 0 -or !$paste.injection.ok) { throw "The installed CLI did not deliver into the standard user's editor: $($paste.injection.message) $($paste.stderr)" }
    if ([string]::IsNullOrEmpty($paste.transcript) -or $paste.editor -cne $paste.transcript) { throw 'The editor of the standard user did not receive the transcript' }
    $report.paste = [ordered]@{
        editorIntegrity = Format-Level $paste.integrity
        method = $paste.injection.method
        delivery = $paste.injection.delivery
        message = $paste.injection.message
        characters = ([string]$paste.editor).Length
        transcript = $paste.transcript
        clipboardBefore = $paste.clipboardBefore
        clipboardAfter = $paste.clipboardAfter
    }
    Write-Output "Paste: $($report.paste.characters) characters reached the standard user's editor through $($paste.injection.method) at $(Format-Level $paste.integrity); clipboard before: '$($paste.clipboardBefore)', after: '$($paste.clipboardAfter)'"

    # A window Windows keeps above the standard user receives nothing.
    $elevatedForm, $elevatedField = New-Editor 'Voiceless elevated editor' 'Voiceless elevated editor: this text stays'
    $sentinel = 'Voiceless elevated harness clipboard'
    $sentinelSet = $false
    try {
        Wait-OwnFocus $elevatedForm $elevatedField 'The elevated editor did not obtain the foreground focus'
        try {
            [System.Windows.Forms.Clipboard]::SetText($sentinel)
            $sentinelSet = [System.Windows.Forms.Clipboard]::GetText() -ceq $sentinel
        } catch { Write-Output "No clipboard sentinel: $($_.Exception.Message)" }
        $process = Start-Worker inject
        Save-Screen 'elevated-editor-before-the-refusal'
        $timer = [System.Diagnostics.Stopwatch]::StartNew()
        while (!$process.HasExited) {
            [System.Windows.Forms.Application]::DoEvents()
            if ($timer.Elapsed.TotalSeconds -ge 300) { $process.Kill(); throw 'The installed CLI did not finish its withheld delivery' }
            Start-Sleep -Milliseconds 15
        }
        [System.Windows.Forms.Application]::DoEvents()
        $elevatedText = $elevatedField.Text
        $elevatedForeground = [AcceptanceDesktop]::GetForegroundWindow() -eq $elevatedForm.Handle
        $clipboardAfter = if ($sentinelSet) { [System.Windows.Forms.Clipboard]::GetText() } else { $null }
    } finally { $elevatedForm.Dispose() }
    $null = Wait-Worker $process inject
    $refusal = Get-WorkerResult inject
    if (!$refusal.transcript) { throw 'Nothing was recognized, so no delivery into the elevated editor was attempted' }
    if ($refusal.exit -eq 0 -or $refusal.injection.ok) { throw "The standard user's CLI reported a delivery into the elevated editor: $($refusal.injection.message)" }
    if ($refusal.injection.message -notmatch 'integrity') { throw "Unexpected reason for the withheld delivery while the elevated editor was foreground = $elevatedForeground`: $($refusal.injection.message)" }
    if ($elevatedText -cne 'Voiceless elevated editor: this text stays') { throw "The elevated editor changed to '$elevatedText'" }
    if ($sentinelSet -and $clipboardAfter -cne $sentinel) { throw "The clipboard changed to '$clipboardAfter' although the delivery was withheld" }
    $report.elevatedTarget = [ordered]@{
        cliIntegrity = Format-Level $refusal.integrity
        editorIntegrity = Format-Level (Get-Level $PID)
        editorForeground = $elevatedForeground
        exit = $refusal.exit
        injection = $refusal.injection
        editor = $elevatedText
        clipboard = $clipboardAfter
    }
    Write-Output "Refused: the CLI at $(Format-Level $refusal.integrity) left the editor at $(Format-Level (Get-Level $PID)) unchanged; $($refusal.injection.message)"

    # The desktop's window and tray at the standard user's integrity level.
    $started = Invoke-Step desktop-start 'The standard user could not start the installed desktop'
    $desktopId = [uint32]$started.process
    $window = Wait-For { [AcceptanceDesktop]::VisibleWindow($desktopId) } 'The installed desktop showed no window for the standard user' 75
    $runtime = Get-RuntimeId
    if (!$runtime) { throw 'The desktop left no runtime identity' }
    Save-Screen 'desktop-window-standard-user'
    . (Join-Path $PSScriptRoot 'tray.ps1')
    $firstAttempt = $null
    $tray = $null
    foreach ($attempt in 1..2) {
        if ([AcceptanceDesktop]::VisibleWindow($desktopId) -ne [IntPtr]::Zero) { Hide-DesktopWindow $desktopId $window }
        try {
            $tray = Invoke-TrayItem -ProcessId $desktopId
            $shell = "the runner account's Explorer"
            break
        } catch {
            $firstAttempt = "$($_.Exception.Message)"
            Write-Output "No tray command through the runner account's Explorer (attempt $attempt): $firstAttempt"
        }
    }
    if (!$tray) {
        try { Use-StandardShell } catch { Start-Process -FilePath (Join-Path $env:WINDIR 'explorer.exe') | Out-Null; throw }
        $shellReplaced = $true
        Stop-StandardVoiceless
        $started = Invoke-Step desktop-start 'The standard user could not start the installed desktop again'
        $desktopId = [uint32]$started.process
        $window = Wait-For { [AcceptanceDesktop]::VisibleWindow($desktopId) } 'The installed desktop showed no window under the standard user''s Explorer' 75
        $runtime = Get-RuntimeId
        Hide-DesktopWindow $desktopId $window
        $tray = Invoke-TrayItem -ProcessId $desktopId
        $shell = "$name's own Explorer"
    }
    $reopened = Wait-For { [AcceptanceDesktop]::VisibleWindow($desktopId) } 'The tray command did not show the window again' 30
    if ($reopened -ne $window) { throw 'The tray command showed another window' }
    if ((Get-RuntimeId) -ne $runtime) { throw 'The tray command replaced the runtime' }
    Save-Screen 'tray-reopened-standard-user'
    $taskbar = Get-Process -Name explorer -ErrorAction SilentlyContinue | Where-Object { [AcceptanceDesktop]::OwnsTaskbar([uint32]$_.Id) } | Select-Object -First 1
    $report.tray = [ordered]@{
        shell = $shell
        throughTheRunnerExplorer = if ($firstAttempt) { $firstAttempt } else { 'worked' }
        taskbar = [ordered]@{ id = $taskbar.Id; account = Get-Sid $taskbar.Id; integrity = Format-Level (Get-Level $taskbar.Id) }
        detail = $tray
        sameWindow = $true
        runtimeId = $runtime
    }
    Write-Output "Tray: $tray; the window and the runtime are the same after reopening"

    # The launcher again, as the Start menu entry starts it.
    $null = Invoke-Step desktop-relaunch 'The second launcher did not reuse the resident desktop'
    $null = Wait-For { [AcceptanceDesktop]::VisibleWindow($desktopId) } 'The window did not stay after the second launch' 30
    if ((Get-RuntimeId) -ne $runtime) { throw 'The second launcher replaced the runtime' }
    $report.desktopProcesses = @(Get-Process -Name voiceless, voiceless-desktop -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -and $_.Path.StartsWith($script:installed, [StringComparison]::OrdinalIgnoreCase) } |
        ForEach-Object { [ordered]@{ name = $_.Name; id = $_.Id; path = $_.Path; account = Get-Sid $_.Id; integrity = Format-Level (Get-Level $_.Id) } })
    $elsewhere = @($report.desktopProcesses | Where-Object { $_.integrity -ne '0x2000' -or $_.account -ne $identity.sid })
    if (!$report.desktopProcesses.Count -or $elsewhere.Count) { throw "The desktop's processes do not all run as the standard user at medium integrity: $($report.desktopProcesses | ConvertTo-Json -Compress)" }

    # The installed CLI of that user reaches the runtime of that user.
    $ctl = Invoke-Step ctl 'The standard user could not reach the desktop runtime with the installed CLI'
    if ($ctl.exit -ne 0 -or $ctl.runtimeId -ne $runtime) { throw "voiceless ctl status answered '$($ctl.status)' for runtime $($ctl.runtimeId), expected $runtime" }
    if (@($ctl.resolved).Count -ne 1 -or !$ctl.resolved[0].StartsWith((Split-Path $install.cli), [StringComparison]::OrdinalIgnoreCase)) { throw "A new console of the standard user finds voiceless at $($ctl.resolved -join ', ')" }
    $cliDirectory = Split-Path $install.cli
    $installedPath = if ($identity.path) { ([string]$identity.path).TrimEnd(';') + ';' + $cliDirectory } else { $cliDirectory }
    if ($ctl.pathKind -ne 'ExpandString' -or [string]$ctl.path -cne $installedPath) { throw "The user's PATH is $($ctl.pathKind): '$($ctl.path)'; before the installation it was '$($identity.path)' ($($identity.pathKind))" }
    $report.lookup = [ordered]@{ status = $ctl.status; resolved = @($ctl.resolved); where = @($ctl.where); path = $ctl.path; pathKind = $ctl.pathKind }
    Write-Output "A new console of $($identity.account) finds voiceless at $($ctl.resolved -join ', ') and reaches the runtime"

    # Removal by the same user.
    Stop-StandardVoiceless
    $uninstall = Invoke-Step uninstall 'The standard user could not remove both candidates'
    $left = @($uninstall.uninstallers | Where-Object { $_.exit -ne 0 -or $_.remains })
    if ($left.Count) { throw "An uninstaller did not finish for the standard user: $($left | ConvertTo-Json -Compress)" }
    $after = Invoke-Step lookup 'The standard user could not be consulted after the removal'
    if (@($after.resolved).Count -or $after.cliDirectory -or $after.desktopDirectory -or $after.shortcut -or @($after.uninstallEntries).Count) { throw "Something of the installation stayed: $($after | ConvertTo-Json -Compress)" }
    if ([string]$after.path -cne [string]$identity.path -or ($identity.pathKind -and [string]$after.pathKind -cne [string]$identity.pathKind)) { throw "The user's PATH after the removal is '$($after.path)' ($($after.pathKind)); before the installation it was '$($identity.path)' ($($identity.pathKind))" }
    $report.removal = [ordered]@{ uninstallers = @($uninstall.uninstallers); path = $after.path; pathKind = $after.pathKind; resolved = @($after.resolved) }
    Write-Output 'Removed: a new console of the standard user no longer finds voiceless; the PATH is as before the installation'
} catch {
    $report.failure = $_.Exception.Message
    $report.failureAt = [string]$_.ScriptStackTrace
    throw
} finally {
    try { Stop-StandardVoiceless } catch { Write-Output "Installed programs of $name stayed: $($_.Exception.Message)" }
    if ($shellReplaced) {
        try { Restore-RunnerShell } catch { Write-Output "The runner account's Explorer stayed away: $($_.Exception.Message)" }
    }
    # What the steps of the standard user noted, in the order they ran.
    Get-ChildItem -LiteralPath $Evidence -Filter 'worker-*.log' | Sort-Object LastWriteTime | ForEach-Object {
        $step = $_.Name -replace '^worker-', ''
        Get-Content -LiteralPath $_.FullName | ForEach-Object { Write-Output "  $step : $_" }
    }
    $report | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath (Join-Path $Evidence 'summary.json') -Encoding utf8
    Write-Output ($report | ConvertTo-Json -Depth 12)
}

exit 0
