# Reopen the hidden Voiceless window through the installed host's real notification
# icon, the way a user does. UI Automation finds the icon, in the taskbar's
# notification area or in its hidden overflow; the pointer opens the icon's
# context menu and chooses one of its commands. Dot-source it once window.ps1
# has hidden the window:
#
#   . (Join-Path $PSScriptRoot 'tray.ps1')
#   Write-Output (Invoke-TrayItem -ProcessId $desktop.Id)
#   $null = Wait-Window $true
#
# Explorer's taskbar must be on the interactive desktop of this session. The
# notification overflow is shown and then dismissed; nothing else about the
# shell, the icon or the host is written to.
function Invoke-TrayItem {
    param(
        [Parameter(Mandatory)][int]$ProcessId,
        [string]$Tooltip = 'Voiceless',
        [string]$Item = '打开窗口'
    )
    Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
    if (!('VoicelessTest.Tray' -as [type])) {
        Add-Type -Namespace VoicelessTest -Name Tray -MemberDefinition @'
[DllImport("user32.dll")]
public static extern bool SetCursorPos(int x, int y);
[DllImport("user32.dll")]
public static extern int GetSystemMetrics(int index);
[DllImport("user32.dll")]
public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extraInfo);
[DllImport("user32.dll")]
public static extern void keybd_event(byte key, byte scan, uint flags, UIntPtr extraInfo);
'@
    }
    $element = [System.Windows.Automation.AutomationElement]
    $children = [System.Windows.Automation.TreeScope]::Children
    $descendants = [System.Windows.Automation.TreeScope]::Descendants
    $root = $element::RootElement
    # The window Explorer shows for the hidden icons; it stays off screen until asked for.
    $overflow = { Find-Class $root $children 'NotifyIconOverflowWindow' }
    $hidden = { $flyout = & $overflow; !$flyout -or $flyout.Current.IsOffscreen }
    function New-Condition($Property, $Value) {
        [System.Windows.Automation.PropertyCondition]::new($Property, $Value)
    }
    function Find-Class($Root, [System.Windows.Automation.TreeScope]$Scope, [string]$Class) {
        $Root.FindFirst($Scope, (New-Condition $element::ClassNameProperty $Class))
    }
    function Find-Buttons($Container) {
        if ($Container) {
            $Container.FindAll($descendants, (New-Condition $element::ControlTypeProperty ([System.Windows.Automation.ControlType]::Button)))
        }
    }
    function Get-Names($Container) {
        @(Find-Buttons $Container | ForEach-Object { if ($_.Current.Name) { $_.Current.Name } else { $_.Current.HelpText } }) -join ', '
    }
    function Find-Icon($Container) {
        # Explorer names an icon's button after its tooltip; the text also shows up as help text on some Windows builds.
        Find-Buttons $Container |
            Where-Object { $_.Current.Name -like "$Tooltip*" -or $_.Current.HelpText -like "$Tooltip*" } | Select-Object -First 1
    }
    function Find-Menu([int[]]$Owners) {
        $root.FindAll($children, (New-Condition $element::ClassNameProperty '#32768')) |
            Where-Object { $_.Current.ProcessId -in $Owners } | Select-Object -First 1
    }
    function Wait-For([scriptblock]$Probe, [string]$Failure, [int]$Seconds = 10) {
        $deadline = [DateTime]::UtcNow.AddSeconds($Seconds)
        do {
            if ($found = & $Probe) { return $found }
            Start-Sleep -Milliseconds 150
        } while ([DateTime]::UtcNow -lt $deadline)
        throw $Failure
    }
    function Move-Pointer([int]$X, [int]$Y) {
        $null = [VoicelessTest.Tray]::SetCursorPos($X, $Y)
    }
    function Invoke-Click($Target, [ValidateSet('Left', 'Right')][string]$Button) {
        $rect = $Target.Current.BoundingRectangle
        if ($rect.IsEmpty) { throw "The '$($Target.Current.Name)' target is not on screen" }
        # Give the target time to see the pointer before the button is pressed.
        Move-Pointer ([int]($rect.Left + $rect.Width / 2)) ([int]($rect.Top + $rect.Height / 2))
        Start-Sleep -Milliseconds 150
        # mouse_event flags: LEFTDOWN 0x0002, LEFTUP 0x0004, RIGHTDOWN 0x0008, RIGHTUP 0x0010.
        $down, $up = if ($Button -eq 'Right') { 0x0008, 0x0010 } else { 0x0002, 0x0004 }
        [VoicelessTest.Tray]::mouse_event($down, 0, 0, 0, [UIntPtr]::Zero)
        [VoicelessTest.Tray]::mouse_event($up, 0, 0, 0, [UIntPtr]::Zero)
    }
    function Close-Overflow {
        # Explorer dismisses the overflow once the pointer that entered it has left;
        # one the pointer never entered closes when the taskbar's button is clicked again.
        Move-Pointer ([VoicelessTest.Tray]::GetSystemMetrics(0) / 2) ([VoicelessTest.Tray]::GetSystemMetrics(1) / 2)
        $deadline = [DateTime]::UtcNow.AddSeconds(2)
        while ($chevron -and !(& $hidden) -and [DateTime]::UtcNow -lt $deadline) { Start-Sleep -Milliseconds 100 }
        if ($chevron -and !(& $hidden)) { Invoke-Click $chevron Left }
    }

    $taskbar = Find-Class $root $children 'Shell_TrayWnd'
    if (!$taskbar) { throw "Explorer has no taskbar (Shell_TrayWnd) in this session, so the '$Tooltip' icon cannot be observed" }
    $area = Find-Class $taskbar $descendants 'TrayNotifyWnd'
    if (!$area) { throw 'The Explorer taskbar has no notification area (TrayNotifyWnd)' }
    $chevron = $null
    try {
        $icon = Find-Icon $area
        $location = 'notification area'
        if (!$icon) {
            # A newly registered icon starts out hidden. The taskbar's leftmost button
            # shows and hides the overflow that holds the hidden icons.
            $chevron = $area.FindAll($children, (New-Condition $element::ClassNameProperty 'Button')) |
                Sort-Object { $_.Current.BoundingRectangle.Left } | Select-Object -First 1
            if (!$chevron) { throw "The notification area has no '$Tooltip' icon and the taskbar has no hidden-icons button; its buttons: $(Get-Names $area)" }
            if (& $hidden) { Invoke-Click $chevron Left }
            $null = Wait-For { $flyout = & $overflow; $flyout -and !$flyout.Current.IsOffscreen } 'The hidden-icons button did not open the notification overflow'
            $location = 'notification overflow'
            $icon = Wait-For {
                $found = Find-Icon (& $overflow)
                if ($found -and !$found.Current.BoundingRectangle.IsEmpty) { $found }
            } "The notification overflow has no '$Tooltip' icon; its buttons: $(Get-Names (& $overflow))"
        }

        Invoke-Click $icon Right
        # The menu is a popup window of the host's own process.
        $menu = Wait-For { Find-Menu $ProcessId } "The '$Tooltip' icon did not open a menu owned by process $ProcessId"
        $menuItem = New-Condition $element::ControlTypeProperty ([System.Windows.Automation.ControlType]::MenuItem)
        $items = $menu.FindAll($descendants, $menuItem) | ForEach-Object { $_.Current.Name }
        $command = $menu.FindFirst($descendants, [System.Windows.Automation.AndCondition]::new($menuItem, (New-Condition $element::NameProperty $Item)))
        if (!$command) { throw "The tray menu has no '$Item'; it offers: $($items -join ', ')" }
        Invoke-Click $command Left
        $null = Wait-For { !(Find-Menu $ProcessId) } "The tray menu stayed open after '$Item' was chosen"
        Close-Overflow
        $null = Wait-For $hidden 'The notification overflow stayed open' 5
        return "Tray icon (name '$($icon.Current.Name)', help text '$($icon.Current.HelpText)') observed in the $location; its menu, owned by process $ProcessId, offers [$($items -join ', ')]; '$Item' chosen"
    } finally {
        # Leave the shell as found, whatever went wrong above. A menu the clicks
        # opened belongs to the host or to Explorer.
        if (Find-Menu @($ProcessId, $taskbar.Current.ProcessId)) {
            # keybd_event: VK_ESCAPE 0x1B, then the same key with KEYEVENTF_KEYUP 0x0002.
            [VoicelessTest.Tray]::keybd_event(0x1B, 0, 0, [UIntPtr]::Zero)
            [VoicelessTest.Tray]::keybd_event(0x1B, 0, 2, [UIntPtr]::Zero)
        }
        Close-Overflow
    }
}
