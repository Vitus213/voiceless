# Runs as ContainerUser on a stock Server Core image with only these two installers.
param([string]$Candidates = $PSScriptRoot)
$ErrorActionPreference = 'Stop'

$client = 'Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}'
foreach ($hive in 'HKLM:', 'HKCU:') {
    foreach ($view in 'SOFTWARE', 'SOFTWARE\WOW6432Node') {
        $key = "$hive\$view\$client"
        if (Test-Path -LiteralPath $key) { throw "WebView2 is registered at $key" }
    }
}
foreach ($prefix in $env:ProgramFiles, ${env:ProgramFiles(x86)}, $env:LOCALAPPDATA) {
    $root = Join-Path $prefix 'Microsoft\EdgeWebView\Application'
    if (Test-Path -LiteralPath $root) { throw "An Edge WebView runtime is present in $root" }
}
Write-Host "Windows $([Environment]::OSVersion.Version), user $env:USERNAME, no WebView2 registration or runtime directory"

$root = Join-Path $env:TEMP 'Voiceless dependency check'
New-Item -Type Directory -Path $root | Out-Null
function Install-Candidate([string]$Kind) {
    $installer = @(Get-ChildItem -LiteralPath $Candidates -Filter "voiceless-$Kind-v*-windows-x64.exe")
    if ($installer.Count -ne 1) { throw "Expected one $Kind installer, found $($installer.Count)" }
    $destination = Join-Path $root $Kind
    $log = Join-Path $root "$Kind.log"
    $flags = "/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /DIR=`"$destination`" /LOG=`"$log`""
    $process = Start-Process $installer[0].FullName -ArgumentList $flags -Wait -PassThru
    Write-Host "$Kind setup exit $($process.ExitCode); SHA256 $((Get-FileHash $installer[0].FullName).Hash)"
    if (Test-Path -LiteralPath $log) { Get-Content -LiteralPath $log -Tail 6 | Out-Host }
    return $process.ExitCode
}

if ((Install-Candidate 'desktop') -ne 7) { throw 'Expected Inno Setup to refuse the desktop prerequisite before installation' }
$hint = Select-String -LiteralPath (Join-Path $root 'desktop.log') -SimpleMatch 'Install Microsoft Edge WebView2 Evergreen Runtime, then run setup again.'
if (!$hint) { throw 'The installer did not identify the missing runtime and how to install it' }
Write-Host $hint[0].Line
if ((Test-Path -LiteralPath "$root\desktop\voiceless-desktop.exe") -or (Test-Path -LiteralPath "$root\desktop\voiceless.exe") -or
    (Test-Path -LiteralPath 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\dev.voiceless.desktop_is1')) {
    throw 'The refused desktop installation left an executable or uninstall registration'
}

if ((Install-Candidate 'core') -ne 0) { throw 'Core installation failed' }
$cli = Join-Path $root 'core\voiceless.exe'
try {
    $version = & $cli --version
    if ($LASTEXITCODE -ne 0 -or !(Test-Path -LiteralPath (Join-Path $Candidates ('voiceless-core-v' + ($version -replace '^voiceless ', '') + '-windows-x64.exe')))) {
        throw "Installed CLI version failure ($LASTEXITCODE): $version"
    }
    $lines = & $cli model list --json
    if ($LASTEXITCODE -ne 0) { throw "model list failed with $LASTEXITCODE" }
    $catalog = @($lines | ForEach-Object { $_ | ConvertFrom-Json })
    if ($catalog.id -notcontains 'sense-voice-int8') { throw 'The local model catalog lacks SenseVoice' }
    Write-Host "$version, $($catalog.Count) catalog entries"

    $stdout = Join-Path $root 'console.out'
    $stderr = Join-Path $root 'console.err'
    $web = Start-Process -FilePath $cli -ArgumentList 'web --port 0 --no-open' -RedirectStandardOutput $stdout -RedirectStandardError $stderr -PassThru
    try {
        $deadline = [DateTime]::UtcNow.AddSeconds(30)
        do {
            if ($web.HasExited) { throw "Standalone web exited $($web.ExitCode): $(Get-Content $stderr -Raw)" }
            $announced = [regex]::Match([string](Get-Content $stdout -Raw), 'Voiceless console: (http://127\.0\.0\.1:\d+/)')
            if ($announced.Success) { break }
            Start-Sleep -Milliseconds 200
        } while ([DateTime]::UtcNow -lt $deadline)
        if (!$announced.Success) { throw 'Standalone web did not announce its loopback address' }
        $url = $announced.Groups[1].Value
        $page = Invoke-WebRequest -UseBasicParsing -Uri $url -TimeoutSec 5
        $meta = [regex]::Match($page.Content, '<meta name="voiceless-token" content="([0-9a-f]{48})">')
        if ($page.StatusCode -ne 200 -or !$meta.Success) { throw 'Missing standalone console page' }
        $totals = Invoke-RestMethod -Uri "${url}api/totals" -Headers @{'X-Voiceless-Token'=$meta.Groups[1].Value} -TimeoutSec 5
        if ($null -eq $totals.records -or $null -eq $totals.characters) { throw 'Incomplete history response' }
        Write-Host "Standalone console HTTP $($page.StatusCode), history $($totals.records) records / $($totals.characters) characters"
    } finally {
        if (!$web.HasExited) { $web.Kill(); $web.WaitForExit() }
    }
} finally {
    $uninstall = Start-Process -FilePath "$root\core\unins000.exe" -ArgumentList '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART' -Wait -PassThru
    if ($uninstall.ExitCode -ne 0 -or (Test-Path -LiteralPath $cli)) { throw 'Core did not uninstall cleanly' }
    Write-Host 'Core uninstalled successfully'
}
