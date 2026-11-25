# Diagnostic variant for the disposable branch: records what the shell, UI Automation,
# MSAA and a screenshot say about the installed host's notification icon, then
# reopens the hidden window through the icon's own context menu.
function Invoke-TrayItem {
    param(
        [Parameter(Mandatory)][int]$ProcessId,
        [string]$Tooltip = 'Voiceless',
        [string]$Item = '打开窗口'
    )
    Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
    if (!('VoicelessTest.Tray' -as [type])) {
        Add-Type -Namespace VoicelessTest -Name Tray -MemberDefinition @'
[StructLayout(LayoutKind.Sequential)]
public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }
[StructLayout(LayoutKind.Sequential)]
public struct NOTIFYICONIDENTIFIER { public uint cbSize; public IntPtr hWnd; public uint uID; public Guid guidItem; }
[DllImport("user32.dll", CharSet = CharSet.Unicode)]
private static extern IntPtr FindWindowExW(IntPtr parent, IntPtr after, string className, string title);
[DllImport("user32.dll", CharSet = CharSet.Unicode)]
private static extern int GetClassNameW(IntPtr window, System.Text.StringBuilder className, int maximum);
[DllImport("user32.dll")]
private static extern uint GetWindowThreadProcessId(IntPtr window, out uint owner);
[DllImport("user32.dll")]
private static extern bool GetWindowRect(IntPtr window, out RECT rect);
[DllImport("user32.dll")]
private static extern bool IsWindowVisible(IntPtr window);
[DllImport("user32.dll")]
public static extern bool SetCursorPos(int x, int y);
[DllImport("user32.dll")]
public static extern int GetSystemMetrics(int index);
[DllImport("user32.dll")]
public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extraInfo);
[DllImport("user32.dll")]
public static extern void keybd_event(byte key, byte scan, uint flags, UIntPtr extraInfo);
[DllImport("shell32.dll")]
private static extern int Shell_NotifyIconGetRect(ref NOTIFYICONIDENTIFIER identifier, out RECT iconLocation);
[DllImport("oleacc.dll")]
public static extern int AccessibleObjectFromWindow(IntPtr window, uint objectId, ref Guid interfaceId, [MarshalAs(UnmanagedType.IUnknown)] out object accessible);

public static IntPtr[] TrayWindows(uint processId) {
    System.Collections.Generic.List<IntPtr> found = new System.Collections.Generic.List<IntPtr>();
    IntPtr window = IntPtr.Zero;
    while ((window = FindWindowExW(IntPtr.Zero, window, "tray_icon_app", null)) != IntPtr.Zero) {
        GetWindowThreadProcessId(window, out uint owner);
        if (owner == processId) found.Add(window);
    }
    return found.ToArray();
}
public static IntPtr[] Children(IntPtr parent) {
    System.Collections.Generic.List<IntPtr> found = new System.Collections.Generic.List<IntPtr>();
    IntPtr window = IntPtr.Zero;
    while ((window = FindWindowExW(parent, window, null, null)) != IntPtr.Zero) found.Add(window);
    return found.ToArray();
}
public static string Describe(IntPtr window) {
    GetWindowRect(window, out RECT rect);
    System.Text.StringBuilder name = new System.Text.StringBuilder(256);
    GetClassNameW(window, name, name.Capacity);
    GetWindowThreadProcessId(window, out uint owner);
    return string.Format("0x{0:X} class={1} pid={2} visible={3} rect=({4},{5})-({6},{7})", window.ToInt64(), name, owner, IsWindowVisible(window), rect.Left, rect.Top, rect.Right, rect.Bottom);
}
public static RECT? IconRect(IntPtr tray, uint id) {
    NOTIFYICONIDENTIFIER identifier = new NOTIFYICONIDENTIFIER();
    identifier.cbSize = (uint)Marshal.SizeOf(typeof(NOTIFYICONIDENTIFIER));
    identifier.hWnd = tray;
    identifier.uID = id;
    RECT rect;
    return Shell_NotifyIconGetRect(ref identifier, out rect) == 0 ? rect : (RECT?)null;
}
'@
    }
    function Say([string]$Text) { Write-Host "tray: $Text" }
    function Say-Rect($Rect) {
        if ($null -eq $Rect) { 'none' } else { '({0},{1})-({2},{3})' -f $Rect.Left, $Rect.Top, $Rect.Right, $Rect.Bottom }
    }
    function Say-ElementRect($Rect) {
        if ($Rect.IsEmpty) { 'empty' } else { '({0},{1} {2}x{3})' -f [int]$Rect.Left, [int]$Rect.Top, [int]$Rect.Width, [int]$Rect.Height }
    }
    function Save-Screenshot([string]$Name) {
        try {
            Add-Type -AssemblyName System.Windows.Forms, System.Drawing
            $bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
            $image = [System.Drawing.Bitmap]::new($bounds.Width, $bounds.Height)
            $graphics = [System.Drawing.Graphics]::FromImage($image)
            $graphics.CopyFromScreen($bounds.Location, [System.Drawing.Point]::Empty, $bounds.Size)
            $path = Join-Path $(if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [IO.Path]::GetTempPath() }) "tray-$Name.png"
            $image.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
            $graphics.Dispose()
            $image.Dispose()
            Say "screenshot $path"
        } catch { Say "screenshot $Name failed: $($_.Exception.Message)" }
    }
    function Dump-Tree($Node, [int]$Depth = 0) {
        if (!$Node -or $Depth -gt 5) { return }
        try { $current = $Node.Current } catch { Say ('{0}<gone>' -f (' ' * $Depth)); return }
        Say ('{0}{1} name=[{2}] class=[{3}] id=[{4}] hwnd={5} rect={6} offscreen={7} help=[{8}] pid={9}' -f (' ' * $Depth), $current.ControlType.ProgrammaticName, $current.Name, $current.ClassName, $current.AutomationId, $current.NativeWindowHandle, (Say-ElementRect $current.BoundingRectangle), $current.IsOffscreen, $current.HelpText, $current.ProcessId)
        $child = [System.Windows.Automation.TreeWalker]::RawViewWalker.GetFirstChild($Node)
        while ($child) {
            Dump-Tree $child ($Depth + 1)
            $child = [System.Windows.Automation.TreeWalker]::RawViewWalker.GetNextSibling($child)
        }
    }
    function Dump-Accessible([IntPtr]$Window) {
        try {
            $interfaceId = [guid]'618736e0-3c3d-11cf-810c-00aa00389b71'
            $accessible = $null
            $result = [VoicelessTest.Tray]::AccessibleObjectFromWindow($Window, [uint32]4294967292, [ref]$interfaceId, [ref]$accessible)
            if ($result -ne 0 -or !$accessible) { Say "  msaa: AccessibleObjectFromWindow failed hr=$result"; return }
            $count = $accessible.accChildCount
            Say "  msaa: children=$count name=[$($accessible.get_accName(0))] role=$($accessible.get_accRole(0))"
            for ($child = 1; $child -le $count; $child++) {
                try {
                    $left = 0
                    $top = 0
                    $width = 0
                    $height = 0
                    $accessible.accLocation([ref]$left, [ref]$top, [ref]$width, [ref]$height, $child)
                    Say ("  msaa child {0}: name=[{1}] role={2} desc=[{3}] rect=({4},{5} {6}x{7})" -f $child, $accessible.get_accName($child), $accessible.get_accRole($child), $accessible.get_accDescription($child), $left, $top, $width, $height)
                } catch { Say "  msaa child ${child}: $($_.Exception.Message)" }
            }
        } catch { Say "  msaa: $($_.Exception.Message)" }
    }

    $element = [System.Windows.Automation.AutomationElement]
    $children = [System.Windows.Automation.TreeScope]::Children
    $descendants = [System.Windows.Automation.TreeScope]::Descendants
    $root = $element::RootElement
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
    function Click-At([int]$X, [int]$Y, [ValidateSet('Left', 'Right')][string]$Button) {
        Move-Pointer $X $Y
        Start-Sleep -Milliseconds 150
        # mouse_event flags: LEFTDOWN 0x0002, LEFTUP 0x0004, RIGHTDOWN 0x0008, RIGHTUP 0x0010.
        $down, $up = if ($Button -eq 'Right') { 0x0008, 0x0010 } else { 0x0002, 0x0004 }
        [VoicelessTest.Tray]::mouse_event($down, 0, 0, 0, [UIntPtr]::Zero)
        [VoicelessTest.Tray]::mouse_event($up, 0, 0, 0, [UIntPtr]::Zero)
    }
    function Invoke-Click($Target, [ValidateSet('Left', 'Right')][string]$Button) {
        $rect = $Target.Current.BoundingRectangle
        if ($rect.IsEmpty) { throw "The '$($Target.Current.Name)' target is not on screen" }
        Click-At ([int]($rect.Left + $rect.Width / 2)) ([int]($rect.Top + $rect.Height / 2)) $Button
    }
    function Close-Overflow {
        Move-Pointer ([VoicelessTest.Tray]::GetSystemMetrics(0) / 2) ([VoicelessTest.Tray]::GetSystemMetrics(1) / 2)
        $deadline = [DateTime]::UtcNow.AddSeconds(2)
        while ($chevron -and !(& $hidden) -and [DateTime]::UtcNow -lt $deadline) { Start-Sleep -Milliseconds 100 }
        if ($chevron -and !(& $hidden)) { Invoke-Click $chevron Left }
    }
    function Find-ShellIcon {
        foreach ($tray in $trays) {
            for ($id = 0; $id -lt 16; $id++) {
                $rect = [VoicelessTest.Tray]::IconRect($tray, $id)
                if ($rect) { return [pscustomobject]@{ Tray = $tray; Id = $id; Rect = $rect } }
            }
        }
        $null
    }
    function Test-Inside($Inner, $Outer) {
        $Inner.Left -ge $Outer.Left -and $Inner.Right -le ($Outer.Left + $Outer.Width) -and $Inner.Top -ge $Outer.Top -and $Inner.Bottom -le ($Outer.Top + $Outer.Height)
    }

    $taskbar = Find-Class $root $children 'Shell_TrayWnd'
    if (!$taskbar) { throw "Explorer has no taskbar (Shell_TrayWnd) in this session, so the '$Tooltip' icon cannot be observed" }
    $area = Find-Class $taskbar $descendants 'TrayNotifyWnd'
    if (!$area) { throw 'The Explorer taskbar has no notification area (TrayNotifyWnd)' }
    $trays = [VoicelessTest.Tray]::TrayWindows($ProcessId)
    Say "screen $([VoicelessTest.Tray]::GetSystemMetrics(0))x$([VoicelessTest.Tray]::GetSystemMetrics(1)); taskbar rect=$(Say-ElementRect $taskbar.Current.BoundingRectangle); host tray windows=$($trays.Count)"
    foreach ($tray in $trays) { Say "  $([VoicelessTest.Tray]::Describe($tray))" }
    foreach ($tray in $trays) {
        for ($id = 0; $id -lt 4; $id++) { Say "  shell rect of icon $id before: $(Say-Rect ([VoicelessTest.Tray]::IconRect($tray, $id)))" }
    }
    Say 'taskbar notification area, raw UIA tree:'
    Dump-Tree $area
    Save-Screenshot '1-before'
    $shellIcon = Find-ShellIcon
    $chevron = $null
    $location = 'notification area'
    try {
        if (!$shellIcon -or !(Test-Inside $shellIcon.Rect $taskbar.Current.BoundingRectangle)) {
            $chevron = $area.FindAll($children, (New-Condition $element::ClassNameProperty 'Button')) |
                Sort-Object { $_.Current.BoundingRectangle.Left } | Select-Object -First 1
            if (!$chevron) { throw "The shell places no '$Tooltip' icon on the taskbar and the taskbar has no hidden-icons button; its buttons: $(Get-Names $area)" }
            if (& $hidden) { Invoke-Click $chevron Left }
            $null = Wait-For { $flyout = & $overflow; $flyout -and !$flyout.Current.IsOffscreen } 'The hidden-icons button did not open the notification overflow'
            $location = 'notification overflow'
            Say "overflow open: rect=$(Say-ElementRect ((& $overflow).Current.BoundingRectangle))"
            Start-Sleep -Milliseconds 300
            Say 'overflow, raw UIA tree 0.3 s after opening:'
            Dump-Tree (& $overflow)
            Save-Screenshot '2-overflow'
            foreach ($window in [VoicelessTest.Tray]::Children([IntPtr]((& $overflow).Current.NativeWindowHandle))) {
                Say "  overflow child $([VoicelessTest.Tray]::Describe($window))"
                Dump-Accessible $window
            }
            Start-Sleep -Milliseconds 2000
            Say "overflow 2.5 s after opening: offscreen=$((& $overflow).Current.IsOffscreen)"
            Say 'overflow, raw UIA tree 2.5 s after opening:'
            Dump-Tree (& $overflow)
            foreach ($tray in $trays) {
                for ($id = 0; $id -lt 4; $id++) { Say "  shell rect of icon $id while open: $(Say-Rect ([VoicelessTest.Tray]::IconRect($tray, $id)))" }
            }
            if (!$shellIcon) { $shellIcon = Find-ShellIcon }
            $shellIcon = Wait-For {
                $flyout = & $overflow
                $found = Find-ShellIcon
                if ($found -and $flyout -and !$flyout.Current.IsOffscreen -and (Test-Inside $found.Rect $flyout.Current.BoundingRectangle)) { $found }
            } "The shell does not place a '$Tooltip' icon in the notification overflow; its buttons: $(Get-Names (& $overflow))"
        }
        $iconRect = $shellIcon.Rect
        $x = [int](($iconRect.Left + $iconRect.Right) / 2)
        $y = [int](($iconRect.Top + $iconRect.Bottom) / 2)
        Say "shell icon id=$($shellIcon.Id) rect=$(Say-Rect $iconRect) in the $location"
        $seen = Find-Buttons $root | Where-Object { $r = $_.Current.BoundingRectangle; !$r.IsEmpty -and $r.Contains([System.Windows.Point]::new($x, $y)) } | Select-Object -First 1
        if ($seen) { Say "UIA at that point: $($seen.Current.ControlType.ProgrammaticName) name=[$($seen.Current.Name)] help=[$($seen.Current.HelpText)] class=[$($seen.Current.ClassName)]" }
        else { Say 'UIA exposes no button at that point' }
        Save-Screenshot '3-icon'
        Click-At $x $y Right
        $menu = Wait-For { Find-Menu $ProcessId } "The '$Tooltip' icon did not open a menu owned by process $ProcessId"
        Say "host menu open: rect=$(Say-ElementRect $menu.Current.BoundingRectangle)"
        Save-Screenshot '4-menu'
        $menuItem = New-Condition $element::ControlTypeProperty ([System.Windows.Automation.ControlType]::MenuItem)
        $items = $menu.FindAll($descendants, $menuItem) | ForEach-Object { $_.Current.Name }
        Say "menu offers: $($items -join ', ')"
        Dump-Tree $menu
        $command = $menu.FindFirst($descendants, [System.Windows.Automation.AndCondition]::new($menuItem, (New-Condition $element::NameProperty $Item)))
        if (!$command) { throw "The tray menu has no '$Item'; it offers: $($items -join ', ')" }
        Invoke-Click $command Left
        $null = Wait-For { !(Find-Menu $ProcessId) } "The tray menu stayed open after '$Item' was chosen"
        Close-Overflow
        $null = Wait-For $hidden 'The notification overflow stayed open' 5
        Save-Screenshot '5-after'
        return "Tray icon (shell id $($shellIcon.Id) at $(Say-Rect $iconRect), UIA name '$($seen.Current.Name)') observed in the $location; its menu, owned by process $ProcessId, offers [$($items -join ', ')]; '$Item' chosen"
    } finally {
        if (Find-Menu @($ProcessId, $taskbar.Current.ProcessId)) {
            [VoicelessTest.Tray]::keybd_event(0x1B, 0, 0, [UIntPtr]::Zero)
            [VoicelessTest.Tray]::keybd_event(0x1B, 0, 2, [UIntPtr]::Zero)
        }
        Close-Overflow
    }
}
