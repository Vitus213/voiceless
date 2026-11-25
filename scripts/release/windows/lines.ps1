# Install a signed, two-line evaluation driver on this disposable guest. The driver
# and its catalogs are fetched from the vendor and are never included in a candidate.
$ErrorActionPreference = 'Stop'
if ($env:RUNNER_ENVIRONMENT -ne 'github-hosted' -or $env:RUNNER_OS -ne 'Windows') { throw 'Requires a disposable GitHub-hosted Windows guest' }
$work = Join-Path $env:RUNNER_TEMP 'vac-evaluation'
New-Item -Type Directory -Path $work | Out-Null
$archive = Join-Path $work 'vac471.zip'
Invoke-WebRequest 'https://software.muzychenko.net/trials/vac471.zip' -OutFile $archive -TimeoutSec 120
if ((Get-FileHash $archive -Algorithm SHA256).Hash -ne '380338e5cf4e9186c7e5fc44db1509761369e61eafbb650b139a58f7c3ecf2a6') { throw 'VAC 4.71 trial archive checksum mismatch' }
Expand-Archive $archive -DestinationPath $work
$catalog = Get-AuthenticodeSignature (Join-Path $work 'vrtaucbl.cat')
Write-Output "Catalog $($catalog.Status), $($catalog.SignerCertificate.Subject)"
if ($catalog.Status -ne 'Valid' -or $catalog.SignerCertificate.GetNameInfo('SimpleName', $false) -ne 'Microsoft Windows Hardware Compatibility Publisher') { throw 'Expected the Microsoft-signed driver catalog' }
$inf = Join-Path $work 'vrtaucbl.inf'
& pnputil.exe /add-driver $inf
if ($LASTEXITCODE -ne 0) { throw "Staging the signed driver failed ($LASTEXITCODE)" }
$settings = 'HKLM:\SOFTWARE\EuMus Design\Virtual Audio Cable\4'
New-Item -Path $settings -Force | Out-Null
New-ItemProperty -Path $settings -Name 'Number of cables' -PropertyType DWord -Value 2 -Force | Out-Null
Add-Type -Path (Join-Path $PSScriptRoot 'RootDevice.cs')
$id = [RootDevice]::Create('VirtualAudioCable_83ed7f0e', '4d36e96c-e325-11ce-bfc1-08002be10318', 'MEDIA')
Write-Output "Evaluation device $id"
& pnputil.exe /add-driver $inf /install
if ($LASTEXITCODE -ne 0) { throw "Binding the signed driver failed ($LASTEXITCODE)" }
$driverSettings = 'HKLM:\SYSTEM\CurrentControlSet\Services\VirtualAudioCable_83ed7f0e\Parameters'
New-Item -Path $driverSettings -Force | Out-Null
New-ItemProperty -Path $driverSettings -Name 'Number of cables' -PropertyType DWord -Value 2 -Force | Out-Null
& pnputil.exe /restart-device $id
if ($LASTEXITCODE -ne 0) { throw "Restarting the evaluation device failed ($LASTEXITCODE)" }
foreach ($service in 'AudioEndpointBuilder', 'Audiosrv') { Set-Service $service -StartupType Manual; Start-Service $service }
$deadline = [DateTime]::UtcNow.AddSeconds(45)
do {
    $lines = @(Get-PnpDevice -Class AudioEndpoint -PresentOnly -ErrorAction SilentlyContinue | Where-Object { $_.Status -eq 'OK' -and $_.FriendlyName -like '*Virtual Audio Cable*' })
    if ($lines.Count -ge 4) { break }
    Start-Sleep -Milliseconds 500
} while ([DateTime]::UtcNow -lt $deadline)
$lines | Format-Table Status, FriendlyName, InstanceId -AutoSize | Out-Host
if ($lines.Count -lt 4) { Get-PnpDevice -InstanceId $id | Format-List *; throw "Expected both capture/render pairs, found $($lines.Count) endpoints" }
"VOICELESS_VAC_DEVICE=$id" | Out-File $env:GITHUB_ENV -Append -Encoding utf8
foreach ($port in @(@{name='MIC'; label='Line 1'; prefix='{0.0.1.'}, @{name='OUTPUT'; label='Line 2'; prefix='{0.0.0.'})) {
    $ids = @($lines | Where-Object FriendlyName -eq "$($port.label) (Virtual Audio Cable)" | ForEach-Object { ($_.InstanceId -split '\\')[-1] } | Where-Object { $_.StartsWith($port.prefix) })
    if ($ids.Count -ne 1) { throw "Missing unique $($port.name) endpoint for $($port.label)" }
    "VOICELESS_VAC_$($port.name)=wasapi:$($ids[0])" | Out-File $env:GITHUB_ENV -Append -Encoding utf8
}
# Both endpoints are virtual, and this guest is discarded after the test.
$permission = 'SOFTWARE\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone'
foreach ($hive in @([Microsoft.Win32.Registry]::LocalMachine, [Microsoft.Win32.Registry]::CurrentUser)) {
    foreach ($path in @($permission, "$permission\NonPackaged")) {
        $key = $hive.CreateSubKey($path)
        try {
            Write-Output "Fixture consent $($key.Name): $($key.GetValue('Value', '<inherited>')) -> Allow"
            $key.SetValue('Value', 'Allow', [Microsoft.Win32.RegistryValueKind]::String)
        } finally { $key.Dispose() }
    }
}
