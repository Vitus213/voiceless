# Record, without changing anything, what this guest decides about microphone
# access for desktop programs and whether Settings offers a page to decide it.
# The privacy store and its policies are read; two Settings pages are opened,
# written down and left open.
#
#   pwsh -NoProfile -File consent-probe.ps1 -Output <dir>
param([Parameter(Mandatory)][string]$Output)
$ErrorActionPreference = 'Stop'
if ($env:RUNNER_ENVIRONMENT -ne 'github-hosted' -or $env:RUNNER_OS -ne 'Windows') { throw 'Requires a GitHub-hosted Windows guest' }
New-Item -Type Directory -Path $Output -Force | Out-Null
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
$element = [System.Windows.Automation.AutomationElement]
$tree = [System.Windows.Automation.TreeScope]
$any = [System.Windows.Automation.Condition]::TrueCondition

function Read-Key([string]$Path) {
    $lines = @(& reg.exe query $Path /s)
    [ordered]@{ key = $Path; exit = $LASTEXITCODE; lines = $lines }
}

function Save-Record {
    $Value = $record | ConvertTo-Json -Depth 8
    Set-Content -LiteralPath (Join-Path $Output 'consent-inventory.json') -Value $Value -Encoding utf8
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
            $bitmap.Save((Join-Path $Output "$Name.png"), [System.Drawing.Imaging.ImageFormat]::Png)
        } finally { $bitmap.Dispose() }
    } catch { Write-Output "No screenshot $Name : $($_.Exception.Message)" }
}

# What the Settings window shows, in reading order.
function Read-Window($Window) {
    $rows = [System.Collections.Generic.List[object]]::new()
    foreach ($item in $Window.FindAll($tree::Descendants, $any)) {
        if ($rows.Count -ge 500) { break }
        $pattern = $null
        try {
            $current = $item.Current
            $toggle = $null
            if ($current.IsTogglePatternAvailable -and $item.TryGetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern, [ref]$pattern)) {
                $toggle = [string]$pattern.Current.ToggleState
            }
            $rectangle = $current.BoundingRectangle
            $rows.Add([ordered]@{
                type = ($current.ControlType.ProgrammaticName -replace '^ControlType\.', '')
                name = $current.Name
                id = $current.AutomationId
                className = $current.ClassName
                help = $current.HelpText
                status = $current.ItemStatus
                toggle = $toggle
                enabled = $current.IsEnabled
                offscreen = $current.IsOffscreen
                rectangle = ('{0},{1} {2}x{3}' -f [int]$rectangle.X, [int]$rectangle.Y, [int]$rectangle.Width, [int]$rectangle.Height)
            })
        } catch { $rows.Add([ordered]@{ type = 'unreadable'; name = $_.Exception.Message }) }
    }
    $rows.ToArray()
}

function Open-Settings([string]$Page) {
    Start-Process $Page | Out-Null
    $deadline = [DateTime]::UtcNow.AddSeconds(45)
    do {
        $settings = @(Get-Process -Name SystemSettings, ApplicationFrameHost -ErrorAction SilentlyContinue | ForEach-Object Id)
        $window = $element::RootElement.FindAll($tree::Children, $any) |
            Where-Object { $_.Current.ProcessId -in $settings -and $_.Current.Name } | Select-Object -First 1
        if ($window) { break }
        Start-Sleep -Milliseconds 500
    } while ([DateTime]::UtcNow -lt $deadline)
    if (!$window) { return $null }
    try { $window.SetFocus() } catch { Write-Host "Settings could not be brought forward: $($_.Exception.Message)" }
    Start-Sleep -Seconds 5
    $window
}

$store = 'SOFTWARE\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone'
$system = Get-CimInstance Win32_OperatingSystem
$session = (Get-Process -Id $PID).SessionId
$record = [ordered]@{
    os = [ordered]@{ caption = $system.Caption; version = $system.Version; build = $system.BuildNumber }
    architecture = $env:PROCESS_ARCHITECTURE
    image = $env:ImageVersion
    session = $session
    explorer = @(Get-Process -Name explorer -ErrorAction SilentlyContinue | Where-Object SessionId -eq $session).Count
    account = [System.Security.Principal.WindowsIdentity]::GetCurrent().Name
    mandatoryLabel = @(& whoami.exe /groups /fo csv /nh | Where-Object { $_ -match 'Mandatory Label' })
    administrators = @(& whoami.exe /groups /fo csv /nh | Where-Object { $_ -match 'Administrators' })
    audioEndpoints = @(Get-CimInstance Win32_PnPEntity -Filter "PNPClass = 'AudioEndpoint'" | Where-Object Present | ForEach-Object Name)
    store = @((Read-Key "HKLM\$store"), (Read-Key "HKCU\$store"))
    policies = @(
        (Read-Key 'HKLM\SOFTWARE\Policies\Microsoft\Windows\AppPrivacy'),
        (Read-Key 'HKLM\SOFTWARE\Microsoft\PolicyManager\current\device\Privacy')
    )
    pages = @()
}
Save-Record

# The Privacy hub lists the pages this system offers a user; the microphone page
# is the one the installed CLI's refusal message tells a user to open.
foreach ($page in 'ms-settings:privacy', 'ms-settings:privacy-microphone') {
    $window = Open-Settings $page
    $name = $page -replace '^ms-settings:', ''
    if (!$window) {
        $record.pages += [ordered]@{ page = $page; window = $null; elements = @() }
        Write-Output "$page : no Settings window appeared"
        continue
    }
    $record.pages += [ordered]@{
        page = $page
        window = [ordered]@{
            title = $window.Current.Name
            className = $window.Current.ClassName
            process = (Get-Process -Id $window.Current.ProcessId).ProcessName
        }
        elements = @(Read-Window $window)
    }
    Save-Screen "settings-$name"
}
Save-Record

foreach ($key in @($record.store) + @($record.policies)) {
    Write-Output "$($key.key) (reg exit $($key.exit))"
    foreach ($line in $key.lines) { Write-Output "    $line" }
}
foreach ($page in $record.pages) {
    $switches = @($page.elements | Where-Object { $_.toggle } | ForEach-Object { "'$($_.name)' [$($_.status)] = $($_.toggle)" })
    $texts = @($page.elements | Where-Object { $_.type -eq 'Text' -and $_.name } | Select-Object -First 60 | ForEach-Object { $_.name })
    Write-Output "$($page.page) : window '$($page.window.title)' ($($page.window.process)), $(@($page.elements).Count) elements"
    Write-Output "  switches: $($switches -join ' | ')"
    Write-Output "  texts: $($texts -join ' / ')"
}
