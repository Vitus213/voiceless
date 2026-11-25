param(
    [string]$Core,
    [string]$Speech,
    # Hosted mode: the editor stays open and reports itself for the desktop practice test.
    [string]$Serve,
    [ValidateSet('A', 'B')][string]$Editor = 'A'
)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
Add-Type -Namespace VoicelessTest -Name Editor -MemberDefinition @'
[DllImport("user32.dll")]
public static extern IntPtr GetForegroundWindow();
'@

if ($Serve) {
    # A native editor a test can keep open. It prints one JSON line per focus or text change
    # and answers the command files <Serve>\<Editor>.cmd (state, activate, caret-end, stop) with a
    # "done:<command>" line; practice.py reads these lines from its stdout.
    [Console]::OutputEncoding = [System.Text.Encoding]::UTF8
    $root = (Resolve-Path -LiteralPath $Serve).Path
    $command = Join-Path $root "$Editor.cmd"
    if (Test-Path -LiteralPath $command) { Remove-Item -LiteralPath $command }
    # Commands: state, activate, caret-end, stop.
    $form = [System.Windows.Forms.Form]::new()
    $form.Text = "Voiceless practice editor $Editor"
    $form.StartPosition = 'Manual'
    $form.Location = if ($Editor -eq 'A') { [System.Drawing.Point]::new(60, 60) } else { [System.Drawing.Point]::new(760, 60) }
    $form.Size = [System.Drawing.Size]::new(640, 300)
    $field = [System.Windows.Forms.TextBox]::new()
    $field.Multiline = $true
    $field.Dock = 'Fill'
    $field.Text = "Voiceless practice editor ${Editor}: "
    $form.Controls.Add($field)
    $script:running = $true
    function Write-State([string]$Event) {
        $state = [ordered]@{
            editor = $Editor
            event = $Event
            form = [long]$form.Handle
            field = [long]$field.Handle
            focused = [bool]$field.Focused
            foreground = ([VoicelessTest.Editor]::GetForegroundWindow() -eq $form.Handle)
            text = $field.Text
        }
        [Console]::Out.WriteLine(($state | ConvertTo-Json -Compress))
        [Console]::Out.Flush()
    }
    $field.Add_TextChanged({ Write-State 'changed' })
    $field.Add_GotFocus({ Write-State 'focused' })
    $field.Add_LostFocus({ Write-State 'blurred' })
    $form.Show()
    $form.Activate()
    $null = $field.Focus()
    $field.Select($field.TextLength, $field.TextLength)
    Write-State 'shown'
    $watch = [System.Diagnostics.Stopwatch]::StartNew()
    while ($script:running) {
        [System.Windows.Forms.Application]::DoEvents()
        if ($watch.ElapsedMilliseconds -ge 40) {
            $watch.Restart()
            if (Test-Path -LiteralPath $command) {
                $name = ([System.IO.File]::ReadAllText($command)).Trim()
                Remove-Item -LiteralPath $command
                if ($name -eq 'activate') { $form.Activate(); $null = $field.Focus() }
                if ($name -eq 'caret-end') { $field.Select($field.TextLength, $field.TextLength) }
                if ($name -eq 'stop') { $script:running = $false }
                Write-State "done:$name"
            }
        }
        Start-Sleep -Milliseconds 10
    }
    $form.Close()
    $form.Dispose()
    exit 0
}

if (!$Core -or !$Speech) { throw 'Give -Core and -Speech, or -Serve with a scratch directory' }
# Exercise the installed CLI with the same checked model and audio used by accept.py.
# Keep pumping messages so the native edit control can receive the clipboard paste.
$form = [System.Windows.Forms.Form]::new()
$form.Text = 'Voiceless installer acceptance'
$field = [System.Windows.Forms.TextBox]::new()
$field.Multiline = $true
$field.Dock = 'Fill'
$form.Controls.Add($field)
$process = [System.Diagnostics.Process]::new()
$started = $false
try {
    $form.Show()
    $form.Activate()
    $null = $field.Focus()
    [System.Windows.Forms.Application]::DoEvents()
    if (!$field.Focused -or [VoicelessTest.Editor]::GetForegroundWindow() -ne $form.Handle) {
        throw 'The acceptance editor did not obtain foreground focus'
    }

    $info = $process.StartInfo
    $info.FileName = $Core
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $info.StandardOutputEncoding = [System.Text.Encoding]::UTF8
    foreach ($argument in @('transcribe', $Speech, '--backend', 'local', '--inject', '--json')) {
        $info.ArgumentList.Add($argument)
    }
    $started = $process.Start()
    $output = $process.StandardOutput.ReadToEndAsync()
    $errors = $process.StandardError.ReadToEndAsync()
    $timer = [System.Diagnostics.Stopwatch]::StartNew()
    while (!$process.HasExited) {
        [System.Windows.Forms.Application]::DoEvents()
        if ($timer.Elapsed.TotalSeconds -ge 90) { throw 'Installed transcription timed out' }
        Start-Sleep -Milliseconds 15
    }
    if ($process.ExitCode -ne 0) { throw "Installed transcription failed: $($errors.GetAwaiter().GetResult())" }
    $events = $output.GetAwaiter().GetResult() -split '\r?\n' | Where-Object { $_ } | ForEach-Object { $_ | ConvertFrom-Json }
    $transcript = $events | Where-Object type -eq transcript
    $delivery = $events | Where-Object type -eq finalized
    if ($transcript.backend -ne 'local' -or [string]::IsNullOrWhiteSpace($transcript.text) -or !$delivery.injection.ok) {
        throw 'The installed CLI did not report a local transcript and successful delivery'
    }
    [System.Windows.Forms.Application]::DoEvents()
    if (![string]::Equals($field.Text, $transcript.text, [StringComparison]::Ordinal)) {
        throw 'The native editor contents differ from the installed CLI transcript'
    }
    Write-Output "Installed CLI delivered all $($field.Text.Length) transcript characters to the native editor ($($delivery.injection.method))"
} finally {
    if ($started -and !$process.HasExited) {
        $process.Kill()
        $process.WaitForExit()
    }
    $process.Dispose()
    $form.Dispose()
}
