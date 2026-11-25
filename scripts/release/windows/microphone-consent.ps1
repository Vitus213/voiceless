# Move the switches of Settings > Privacy > Microphone the way a user does, with
# one pointer click each, and report what Windows then stores. Nothing is written
# to the privacy store here: Settings writes what was chosen.
#
#   pwsh -NoProfile -File microphone-consent.ps1 -State on|off -Evidence <dir> [-Name <label>]
#
# "on" turns on the switch for apps ("Allow apps to access your microphone" on
# Windows 10 and Server, "Let apps access your microphone" on Windows 11) and,
# with it, the switch for desktop apps. "off" turns the first of them off, which
# refuses every app. The switches are found by their automation ids; the clicks
# are synthetic pointer input, no person took part.
param(
    [Parameter(Mandatory)][ValidateSet('on', 'off')][string]$State,
    [Parameter(Mandatory)][string]$Evidence,
    [string]$Name = 'microphone-switches'
)
$ErrorActionPreference = 'Stop'
if ($env:RUNNER_ENVIRONMENT -ne 'github-hosted' -or $env:RUNNER_OS -ne 'Windows') { throw 'Requires a disposable GitHub-hosted Windows guest' }
New-Item -Type Directory -Path $Evidence -Force | Out-Null
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Threading;

namespace VoicelessTest {
  [StructLayout(LayoutKind.Sequential)]
  public struct Point { public int X; public int Y; }

  // Pointer input of the interactive desktop. It is synthetic: no person clicks.
  public static class ConsentPointer {
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extra);
    [DllImport("user32.dll")] public static extern IntPtr WindowFromPoint(Point point);
    [DllImport("user32.dll")] public static extern IntPtr GetAncestor(IntPtr window, uint flags);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr window, out uint owner);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr window);
    [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr window);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr window, int command);
    [DllImport("user32.dll")] static extern void keybd_event(byte key, byte scan, uint flags, UIntPtr extra);

    // The process of the top-level window at a screen point.
    public static uint OwnerAt(int x, int y) {
      Point point = new Point();
      point.X = x;
      point.Y = y;
      uint owner;
      GetWindowThreadProcessId(GetAncestor(WindowFromPoint(point), 2), out owner);
      return owner;
    }

    // One left click at a screen point.
    public static void Click(int x, int y) {
      SetCursorPos(x, y);
      Thread.Sleep(150);
      mouse_event(0x0002, 0, 0, 0, UIntPtr.Zero);
      mouse_event(0x0004, 0, 0, 0, UIntPtr.Zero);
    }

    // The wheel over a screen point; 120 is one notch, down when positive.
    public static void Wheel(int x, int y, int notches) {
      SetCursorPos(x, y);
      Thread.Sleep(100);
      mouse_event(0x0800, 0, 0, unchecked((uint)(-120 * notches)), UIntPtr.Zero);
    }

    // A minimized window comes back.
    public static void Restore(IntPtr window) {
      if (IsIconic(window)) ShowWindow(window, 9);
    }

    // Alt pressed and released lets this process hand the foreground to a window.
    public static void BringForward(IntPtr window) {
      keybd_event(0x12, 0, 0, UIntPtr.Zero);
      keybd_event(0x12, 0, 2, UIntPtr.Zero);
      SetForegroundWindow(window);
    }
  }
}
'@
$element = [System.Windows.Automation.AutomationElement]
$tree = [System.Windows.Automation.TreeScope]
$any = [System.Windows.Automation.Condition]::TrueCondition
$store = 'SOFTWARE\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone'
$settingsProcesses = 'SystemSettings', 'ApplicationFrameHost'
$apps = @{ label = 'microphone access for apps'; id = 'SystemSettings_CapabilityAccess_Microphone_UserGlobal_ToggleSwitch' }
$desktop = @{ label = 'microphone access for desktop apps'; id = 'SystemSettings_CapabilityAccess_Microphone_ClassicGlobal_ToggleSwitch' }
$record = [ordered]@{
    state = $State
    input = 'Synthetic pointer clicks and wheel turns of the harness on the switches of Settings; no person took part.'
    clicks = @()
}

function Read-Decision([Microsoft.Win32.RegistryKey]$Hive, [string]$Path) {
    $key = $Hive.OpenSubKey($Path)
    if (!$key) { return '<no key>' }
    try {
        $value = $key.GetValue('Value', $null)
        if ($null -eq $value) { '<no value>' } else { [string]$value }
    } finally { $key.Dispose() }
}

# What the two switches stand for, as read back; nothing here writes.
function Read-Decisions {
    $machine = [Microsoft.Win32.Registry]::LocalMachine
    $user = [Microsoft.Win32.Registry]::CurrentUser
    try {
        [ordered]@{
            device = Read-Decision $machine $store
            apps = Read-Decision $user $store
            desktopOfUser = Read-Decision $user "$store\NonPackaged"
            desktopOfDevice = Read-Decision $machine "$store\NonPackaged"
        }
    } finally { $machine.Dispose(); $user.Dispose() }
}

function Read-Tree([string]$Path) {
    [ordered]@{ key = $Path; lines = @(& reg.exe query $Path /s | Where-Object { $_.Trim() }) }
}

function Test-On($Decisions) {
    if ($Decisions.apps -ne 'Allow' -or $Decisions.desktopOfUser -eq 'Deny') { return $false }
    $Decisions.desktopOfUser -eq 'Allow' -or $Decisions.desktopOfDevice -eq 'Allow'
}

function Test-Off($Decisions) { $Decisions.apps -eq 'Deny' }

function Save-Screen([string]$Label) {
    try {
        $bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
        $bitmap = [System.Drawing.Bitmap]::new($bounds.Width, $bounds.Height)
        try {
            $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
            try {
                $graphics.CopyFromScreen($bounds.Location, [System.Drawing.Point]::Empty, $bounds.Size)
            } finally { $graphics.Dispose() }
            $bitmap.Save((Join-Path $Evidence "$Label.png"), [System.Drawing.Imaging.ImageFormat]::Png)
        } finally { $bitmap.Dispose() }
    } catch { Write-Output "No screenshot $Label : $($_.Exception.Message)" }
}

function Get-SettingsWindow {
    $processes = @(Get-Process -Name $settingsProcesses -ErrorAction SilentlyContinue | ForEach-Object Id)
    $element::RootElement.FindAll($tree::Children, $any) |
        Where-Object { $_.Current.ProcessId -in $processes -and $_.Current.Name } | Select-Object -First 1
}

function Find-Switch([string]$Id) {
    $window = Get-SettingsWindow
    if (!$window) { return $null }
    $window.FindFirst($tree::Descendants, [System.Windows.Automation.PropertyCondition]::new($element::AutomationIdProperty, $Id))
}

# A point over the page of the Settings window, where the wheel scrolls it.
function Get-PagePoint {
    $bounds = (Get-SettingsWindow).Current.BoundingRectangle
    , @([int]($bounds.X + $bounds.Width * 0.75), [int]($bounds.Y + $bounds.Height * 0.6))
}

# Which program shows the screen point.
function Get-OwnerName([int]$X, [int]$Y) {
    (Get-Process -Id ([int][VoicelessTest.ConsentPointer]::OwnerAt($X, $Y))).ProcessName
}

# The switch, wheeled into view when the page has to scroll for it, clicked once.
function Move-Switch($Switch) {
    $target = $null
    foreach ($attempt in 1..16) {
        $target = $null
        $window = Get-SettingsWindow
        if ($window) {
            $handle = [IntPtr]$window.Current.NativeWindowHandle
            [VoicelessTest.ConsentPointer]::Restore($handle)
            $found = Find-Switch $Switch.id
            if ($found -and !$found.Current.IsOffscreen -and !$found.Current.BoundingRectangle.IsEmpty) {
                $rectangle = $found.Current.BoundingRectangle
                $x = [int]($rectangle.X + $rectangle.Width / 2)
                $y = [int]($rectangle.Y + $rectangle.Height / 2)
                if ((Get-OwnerName $x $y) -in $settingsProcesses) {
                    $target = $found
                    break
                }
                # Another window hides it, as one can hide what a person looks for.
                [VoicelessTest.ConsentPointer]::BringForward($handle)
            } elseif ($attempt -gt 2) {
                $page = Get-PagePoint
                [VoicelessTest.ConsentPointer]::Wheel($page[0], $page[1], 3)
            }
        }
        Start-Sleep -Milliseconds 500
    }
    if (!$target) { Save-Screen "$Name-missing"; throw "Settings does not show '$($Switch.label)' in front" }
    $rectangle = $target.Current.BoundingRectangle
    $x = [int]($rectangle.X + $rectangle.Width / 2)
    $y = [int]($rectangle.Y + $rectangle.Height / 2)
    $under = Get-OwnerName $x $y
    if ($under -notin $settingsProcesses) { Save-Screen "$Name-covered"; throw "Another program ($under) is in front of '$($Switch.label)'" }
    [VoicelessTest.ConsentPointer]::Click($x, $y)
    [ordered]@{
        switch = $Switch.label
        automationId = $Switch.id
        patterns = @($target.GetSupportedPatterns() | ForEach-Object ProgrammaticName)
        clickedAt = "$x,$y"
        overTheWindowOf = $under
    }
}

function Wait-Decision([scriptblock]$Done, [string]$What) {
    $deadline = [DateTime]::UtcNow.AddSeconds(15)
    do {
        $decisions = Read-Decisions
        if (& $Done $decisions) { return $decisions }
        Start-Sleep -Milliseconds 300
    } while ([DateTime]::UtcNow -lt $deadline)
    Save-Screen "$Name-unchanged"
    throw "Settings did not store $What`: $($decisions | ConvertTo-Json -Compress)"
}

try {
    $record.storeBefore = @((Read-Tree "HKLM\$store"), (Read-Tree "HKCU\$store"))
    $record.before = Read-Decisions
    Start-Process 'ms-settings:privacy-microphone' | Out-Null
    $deadline = [DateTime]::UtcNow.AddSeconds(45)
    do {
        Start-Sleep -Milliseconds 500
        $window = Get-SettingsWindow
    } while (!$window -and [DateTime]::UtcNow -lt $deadline)
    Start-Sleep -Seconds 3
    # The page may have been left scrolled; a user starts reading it from the top.
    $page = Get-PagePoint
    [VoicelessTest.ConsentPointer]::Wheel($page[0], $page[1], -40)
    Start-Sleep -Milliseconds 700
    Save-Screen "$Name-before"
    if ($State -eq 'on') {
        if (!(Test-On $record.before)) {
            if ($record.before.apps -ne 'Allow') {
                $record.clicks += @(Move-Switch $apps)
                $null = Wait-Decision { param($decisions) $decisions.apps -eq 'Allow' } 'Allow for apps'
            }
            if (!(Test-On (Read-Decisions))) {
                $record.clicks += @(Move-Switch $desktop)
                $null = Wait-Decision { param($decisions) Test-On $decisions } 'Allow for desktop apps'
            }
        }
        $record.after = Wait-Decision { param($decisions) Test-On $decisions } 'microphone access for desktop apps'
    } else {
        if (!(Test-Off $record.before)) {
            $record.clicks += @(Move-Switch $apps)
            $record.after = Wait-Decision { param($decisions) Test-Off $decisions } 'Deny for apps'
        } else {
            $record.after = Read-Decisions
        }
    }
    $record.storeAfter = @((Read-Tree "HKLM\$store"), (Read-Tree "HKCU\$store"))
    Save-Screen "$Name-after"
} catch {
    $record.failure = $_.Exception.Message
    $record.after = Read-Decisions
    $record | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $Evidence "$Name-$State.json") -Encoding utf8
    Write-Output ($record | ConvertTo-Json -Depth 8)
    exit 1
}
$record | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $Evidence "$Name-$State.json") -Encoding utf8
Write-Output ($record | ConvertTo-Json -Depth 8)
exit 0
