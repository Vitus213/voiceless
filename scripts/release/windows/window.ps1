param([Parameter(Mandatory)][string]$Executable)
$ErrorActionPreference = 'Stop'
# Tauri creates helper windows before it opens the console. Locate the titled
# top-level window owned by this installation for both observation and WM_CLOSE.
Add-Type -Namespace VoicelessTest -Name Window -MemberDefinition @'
[DllImport("user32.dll", CharSet = CharSet.Unicode)]
private static extern IntPtr FindWindowEx(IntPtr parent, IntPtr after, string className, string title);
[DllImport("user32.dll")]
private static extern uint GetWindowThreadProcessId(IntPtr window, out uint owner);
[DllImport("user32.dll")]
private static extern bool IsWindowVisible(IntPtr window);
[DllImport("user32.dll")]
private static extern bool PostMessageW(IntPtr window, uint message, IntPtr wParam, IntPtr lParam);

public static IntPtr Visible(uint processId) {
    IntPtr window = IntPtr.Zero;
    while ((window = FindWindowEx(IntPtr.Zero, window, null, "Voiceless")) != IntPtr.Zero) {
        GetWindowThreadProcessId(window, out uint owner);
        if (owner == processId && IsWindowVisible(window)) return window;
    }
    return IntPtr.Zero;
}
public static bool RequestClose(IntPtr window) {
    return PostMessageW(window, 0x0010, IntPtr.Zero, IntPtr.Zero);
}
'@
$startupLog = Join-Path $env:XDG_CONFIG_HOME 'desktop-stderr.log'
$desktop = Start-Process -FilePath $Executable -PassThru -RedirectStandardError $startupLog
function Wait-Window([bool]$Visible, [int]$TimeoutSeconds = 30) {
    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    do {
        $desktop.Refresh()
        if ($desktop.HasExited) { throw "Desktop exited with $($desktop.ExitCode)" }
        $window = [VoicelessTest.Window]::Visible($desktop.Id)
        if (($window -ne [IntPtr]::Zero) -eq $Visible) { return $window }
        Start-Sleep -Milliseconds 150
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Timed out waiting for the Voiceless window (visible=$Visible). Current process window title: '$($desktop.MainWindowTitle)'"
}
$core = Join-Path (Split-Path $Executable) 'voiceless.exe'
$descriptor = Join-Path $env:XDG_DATA_HOME 'voiceless/console-endpoint.json'
function Get-RuntimeId {
    & $core ctl status | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Installed CLI cannot reach the desktop daemon' }
    $id = (Get-Content -Raw -LiteralPath $descriptor | ConvertFrom-Json).runtime_id
    if (!$id) { throw 'Missing desktop runtime identity' }
    return $id
}
try {
    $window = Wait-Window $true 75
    $original = Get-RuntimeId
    if (![VoicelessTest.Window]::RequestClose($window)) { throw 'Desktop did not accept WM_CLOSE' }
    $null = Wait-Window $false
    $second = Start-Process -FilePath $Executable -PassThru
    if (!$second.WaitForExit(15000)) { throw 'Second launcher did not reuse the resident desktop' }
    $null = Wait-Window $true
    if ((Get-RuntimeId) -ne $original) { throw 'Reopening the window replaced the daemon' }
} catch {
    if (Test-Path -LiteralPath $startupLog) { Get-Content -Tail 15 -LiteralPath $startupLog | Out-Host }
    throw
} finally {
    # Limit teardown to this candidate and the adjacent core it started.
    if (!$desktop.HasExited) { Stop-Process -Id $desktop.Id -Force }
    Get-Process -Name voiceless -ErrorAction SilentlyContinue | Where-Object Path -eq $core | Stop-Process -Force
}
