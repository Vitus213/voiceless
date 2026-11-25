param([Parameter(Mandatory)][string]$Core, [Parameter(Mandatory)][string]$Speech)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms
Add-Type -Namespace VoicelessTest -Name Editor -MemberDefinition @'
[DllImport("user32.dll")]
public static extern IntPtr GetForegroundWindow();
'@

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
