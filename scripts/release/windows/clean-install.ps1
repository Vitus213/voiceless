# Runs offline as ContainerUser on Windows Server Core, alongside the two installers.
param([Parameter(Mandatory)][string]$Version)
$ErrorActionPreference = 'Stop'

if (Test-Path "$env:SystemRoot\System32\vcruntime140.dll") { throw 'The test image already has the Visual C++ runtime' }
$client = 'Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}'
foreach ($hive in 'HKLM:', 'HKCU:') {
    foreach ($view in 'SOFTWARE', 'SOFTWARE\WOW6432Node') {
        if (Test-Path "$hive\$view\$client") { throw 'The test image already has a registered WebView2 runtime' }
    }
}
foreach ($prefix in $env:ProgramFiles, ${env:ProgramFiles(x86)}, $env:LOCALAPPDATA) {
    if (Test-Path "$prefix\Microsoft\EdgeWebView\Application") { throw 'The test image already has WebView2 files' }
}

$root = Join-Path $env:TEMP 'Voiceless clean install'
New-Item -ItemType Directory -Path $root | Out-Null
function Install-Candidate([string]$Kind) {
    $installer = Join-Path $PSScriptRoot "voiceless-$Kind-v$Version-windows-x64.exe"
    $flags = "/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /DIR=`"$root\$Kind`" /LOG=`"$root\$Kind.log`""
    $setup = Start-Process -FilePath $installer -ArgumentList $flags -Wait -PassThru
    Write-Host "$Kind setup exit $($setup.ExitCode), SHA256 $((Get-FileHash $installer).Hash)"
    return $setup.ExitCode
}

if ((Install-Candidate 'desktop') -ne 7) { throw 'Expected desktop setup to stop before installation' }
$hint = 'Install Microsoft Edge WebView2 Evergreen Runtime, then run setup again.'
if (!(Select-String -LiteralPath "$root\desktop.log" -SimpleMatch $hint) -or (Test-Path "$root\desktop\*.exe") -or
    (Test-Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\dev.voiceless.desktop_is1')) {
    throw 'Desktop setup must explain the missing prerequisite and leave the application uninstalled'
}
Write-Host $hint

if ((Install-Candidate 'core') -ne 0) { throw 'Core installation failed' }
$cli = "$root\core\voiceless.exe"
try {
    $reported = & $cli --version
    if ($LASTEXITCODE -ne 0 -or $reported -ne "voiceless $Version") { throw "CLI startup failed ($LASTEXITCODE): $reported" }
    $lines = & $cli model list --json
    if ($LASTEXITCODE -ne 0) { throw "Model catalog failed ($LASTEXITCODE)" }
    $models = @($lines | ForEach-Object { $_ | ConvertFrom-Json })
    if ($models.id -notcontains 'sense-voice-int8') { throw 'SenseVoice missing from the installed model catalog' }
    Write-Host "$reported, $($models.Count) catalog entries"

    $stdout = "$root\web.out"
    $stderr = "$root\web.err"
    $web = Start-Process -FilePath $cli -ArgumentList 'web --port 0 --no-open' -RedirectStandardOutput $stdout -RedirectStandardError $stderr -PassThru
    try {
        $deadline = [DateTime]::UtcNow.AddSeconds(30)
        do {
            if ($web.HasExited) { throw "Web exited with $($web.ExitCode): $(Get-Content $stderr -Raw)" }
            $address = Select-String -LiteralPath $stdout -Pattern 'Voiceless console: (http://127\.0\.0\.1:\d+/)' | Select-Object -First 1
            if ($address) { break }
            Start-Sleep -Milliseconds 200
        } while ([DateTime]::UtcNow -lt $deadline)
        if (!$address) { throw "Web startup timed out: $(Get-Content $stderr -Raw)" }
        $url = $address.Matches[0].Groups[1].Value
        $page = Invoke-WebRequest -UseBasicParsing -Uri $url -TimeoutSec 5
        $token = [regex]::Match($page.Content, '<meta name="voiceless-token" content="([0-9a-f]{48})">')
        if ($page.StatusCode -ne 200 -or !$token.Success) { throw 'The console did not serve its authenticated page' }
        $totals = Invoke-RestMethod -Uri "${url}api/totals" -Headers @{'X-Voiceless-Token'=$token.Groups[1].Value} -TimeoutSec 5
        if ($totals.records -ne 0 -or $totals.characters -ne 0) { throw 'Unexpected fresh history totals' }
        Write-Host "Console HTTP $($page.StatusCode), history $($totals.records) records / $($totals.characters) characters"
    } finally {
        if (!$web.HasExited) { $web.Kill(); $web.WaitForExit() }
    }
} finally {
    $remove = Start-Process -FilePath "$root\core\unins000.exe" -ArgumentList '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART' -Wait -PassThru
    if ($remove.ExitCode -ne 0 -or (Test-Path $cli)) { throw 'Core uninstall failed' }
    Write-Host 'Core uninstalled successfully'
}
