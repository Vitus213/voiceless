param([Parameter(Mandatory)][string]$Executable)
$ErrorActionPreference = 'Stop'
$desktop = Start-Process -FilePath $Executable -PassThru
function Wait-Window([bool]$Visible) {
    $deadline = [DateTime]::UtcNow.AddSeconds(30)
    do {
        $desktop.Refresh()
        if ($desktop.HasExited) { throw "Desktop exited with $($desktop.ExitCode)" }
        if (($desktop.MainWindowHandle -ne [IntPtr]::Zero) -eq $Visible) { return }
        Start-Sleep -Milliseconds 150
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Desktop window did not reach visible=$Visible"
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
    Wait-Window $true
    $original = Get-RuntimeId
    if (!$desktop.CloseMainWindow()) { throw 'Desktop did not accept WM_CLOSE' }
    Wait-Window $false
    $second = Start-Process -FilePath $Executable -PassThru
    if (!$second.WaitForExit(15000)) { throw 'Second launcher did not reuse the resident desktop' }
    Wait-Window $true
    if ((Get-RuntimeId) -ne $original) { throw 'Reopening the window replaced the daemon' }
} finally {
    # Limit teardown to this candidate and the adjacent core it started.
    if (!$desktop.HasExited) { Stop-Process -Id $desktop.Id -Force }
    Get-Process -Name voiceless -ErrorAction SilentlyContinue | Where-Object Path -eq $core | Stop-Process -Force
}
