# Diagnostic variant for the disposable branch: locates the installed host's notification
# icon through the shell, records what MSAA, UI Automation and a screenshot say, then
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
    function Say-ElementRect($Rect) {
        if ($Rect.IsEmpty) { 'empty' } else { '({0},{1} {2}x{3})' -f [int]$Rect.Left, [int]$Rect.Top, [int]$Rect.Width, [int]$Rect.Height }
    }
    function Say-Rect($Rect) {
        if ($null -eq $Rect) { 'none' } else { '({0},{1})-({2},{3})' -f $Rect.Left, $Rect.Top, $Rect.Right, $Rect.Bottom }
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
    function Read-Tree51([IntPtr]$Window) {
        # Windows PowerShell's .NET Framework client, for comparison with pwsh.
        $path = Join-Path ([IO.Path]::GetTempPath()) 'tray-tree51.ps1'
        try {
            Set-Content -LiteralPath $path -Encoding UTF8 -Value @'
param([long]$Window)
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
$walker = [System.Windows.Automation.TreeWalker]::RawViewWalker
function Walk($Node, [int]$Depth) {
    if (!$Node -or $Depth -gt 5) { return }
    $current = $Node.Current
    '{0}{1} name=[{2}] class=[{3}] rect={4}' -f (' ' * $Depth), $current.ControlType.ProgrammaticName, $current.Name, $current.ClassName, $current.BoundingRectangle
    $child = $walker.GetFirstChild($Node)
    while ($child) { Walk $child ($Depth + 1); $child = $walker.GetNextSibling($child) }
}
Walk ([System.Windows.Automation.AutomationElement]::FromHandle([IntPtr]$Window)) 0
'@
            $start = [System.Diagnostics.ProcessStartInfo]::new('powershell.exe')
            $start.UseShellExecute = $false
            $start.CreateNoWindow = $true
            $start.RedirectStandardOutput = $true
            $start.RedirectStandardError = $true
            foreach ($argument in @('-NoProfile', '-STA', '-ExecutionPolicy', 'Bypass', '-File', $path, "$($Window.ToInt64())")) { $start.ArgumentList.Add($argument) }
            $process = [System.Diagnostics.Process]::Start($start)
            $output = $process.StandardOutput.ReadToEndAsync()
            $errors = $process.StandardError.ReadToEndAsync()
            if (!$process.WaitForExit(30000)) { $process.Kill(); throw 'Windows PowerShell did not answer in 30 s' }
            $output.GetAwaiter().GetResult() -split '\r?\n' | Where-Object { $_ } | ForEach-Object { Say "5.1: $_" }
            $errors.GetAwaiter().GetResult() -split '\r?\n' | Where-Object { $_ } | ForEach-Object { Say "5.1 error: $_" }
        } catch { Say "5.1 probe failed: $($_.Exception.Message)" }
        finally { Remove-Item -LiteralPath $path -ErrorAction SilentlyContinue }
    }
    function Get-Accessible([IntPtr]$Window) {
        $interfaceId = [guid]'618736e0-3c3d-11cf-810c-00aa00389b71'
        $accessible = $null
        $result = [VoicelessTest.Tray]::AccessibleObjectFromWindow($Window, [uint32]4294967292, [ref]$interfaceId, [ref]$accessible)
        if ($result -ne 0 -or !$accessible) { throw ('The window 0x{0:X} exposes no MSAA object (hr 0x{1:X8})' -f $Window.ToInt64(), $result) }
        $accessible
    }
    function Get-AccValue($Accessible, [string]$Member, $Child = 0) {
        # A parameterized MSAA property, through PowerShell's COM adapter or else IDispatch.
        $property = $Accessible.$Member
        if ($property -is [System.Management.Automation.PSParameterizedProperty]) { return $property.Get($Child) }
        try { return $property.Invoke($Child) }
        catch {
            foreach ($flags in 'GetProperty', 'InvokeMethod') {
                try {
                    return $Accessible.GetType().InvokeMember($Member, [System.Reflection.BindingFlags]$flags, $null, $Accessible, [object[]]@($Child))
                } catch { }
            }
            throw "MSAA $Member is not available: $($_.Exception.Message)"
        }
    }
    function Get-AccLocation($Accessible, $Child) {
        $values = [object[]]@(0, 0, 0, 0, $Child)
        try { $Accessible.accLocation([ref]$values[0], [ref]$values[1], [ref]$values[2], [ref]$values[3], $Child) }
        catch { $null = $Accessible.GetType().InvokeMember('accLocation', [System.Reflection.BindingFlags]::InvokeMethod, $null, $Accessible, $values) }
        $values
    }
    function Get-AccChildren($Accessible) {
        for ($child = 1; $child -le $Accessible.accChildCount; $child++) {
            [pscustomobject]@{
                Index = $child
                Name = [string](Get-AccValue $Accessible 'accName' $child)
                Role = Get-AccValue $Accessible 'accRole' $child
                Location = Get-AccLocation $Accessible $child
            }
        }
    }
    function Say-AccChildren([IntPtr]$Window, [string]$Label) {
        try {
            $accessible = Get-Accessible $Window
            Say "$Label MSAA: role=$(Get-AccValue $accessible 'accRole') name=[$(Get-AccValue $accessible 'accName')] children=$($accessible.accChildCount); members: $((Get-Member -InputObject $accessible -MemberType Method, Property, ParameterizedProperty | ForEach-Object { $_.Name + '(' + $_.MemberType + ')' }) -join ' ')"
            foreach ($child in Get-AccChildren $accessible) {
                Say ('{0} MSAA child {1}: name=[{2}] role={3} rect=({4},{5} {6}x{7})' -f $Label, $child.Index, $child.Name, $child.Role, $child.Location[0], $child.Location[1], $child.Location[2], $child.Location[3])
            }
        } catch { Say "$Label MSAA failed: $($_.Exception.Message)" }
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
    function Find-Menu([int[]]$Owners) {
        $root.FindAll($children, (New-Condition $element::ClassNameProperty '#32768')) |
            Where-Object { $_.Current.ProcessId -in $Owners } | Select-Object -First 1
    }
    function Wait-For([scriptblock]$Probe, [string]$Failure = '', [int]$Seconds = 10) {
        # What the probe returns, or nothing when it never does and no failure is given.
        $deadline = [DateTime]::UtcNow.AddSeconds($Seconds)
        do {
            if ($found = & $Probe) { return $found }
            Start-Sleep -Milliseconds 150
        } while ([DateTime]::UtcNow -lt $deadline)
        if ($Failure) { throw $Failure }
        $null
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
    function Send-Escape {
        # keybd_event: VK_ESCAPE 0x1B, then the same key with KEYEVENTF_KEYUP 0x0002.
        [VoicelessTest.Tray]::keybd_event(0x1B, 0, 0, [UIntPtr]::Zero)
        [VoicelessTest.Tray]::keybd_event(0x1B, 0, 2, [UIntPtr]::Zero)
    }
    function Close-Overflow {
        Move-Pointer ([VoicelessTest.Tray]::GetSystemMetrics(0) / 2) ([VoicelessTest.Tray]::GetSystemMetrics(1) / 2)
        $deadline = [DateTime]::UtcNow.AddSeconds(2)
        while ($chevron -and !(& $hidden) -and [DateTime]::UtcNow -lt $deadline) { Start-Sleep -Milliseconds 100 }
        if ($chevron -and !(& $hidden)) { Invoke-Click $chevron Left }
    }
    function Find-ShellIcon {
        # tray-icon numbers its icons from a process-wide counter, so try the first few.
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
    function Test-Covers($Outer, $Inner) {
        $Inner.Left -lt ($Outer.Left + $Outer.Width) -and $Outer.Left -lt $Inner.Right -and $Inner.Top -lt ($Outer.Top + $Outer.Height) -and $Outer.Top -lt $Inner.Bottom
    }

    $taskbar = Find-Class $root $children 'Shell_TrayWnd'
    if (!$taskbar) { throw "Explorer has no taskbar (Shell_TrayWnd) in this session, so the '$Tooltip' icon cannot be observed" }
    $area = Find-Class $taskbar $descendants 'TrayNotifyWnd'
    if (!$area) { throw 'The Explorer taskbar has no notification area (TrayNotifyWnd)' }
    $trays = [VoicelessTest.Tray]::TrayWindows($ProcessId)
    if (!$trays.Count) { throw "The host has no notification-area window of its own (process $ProcessId), so it registered no icon" }
    # The taskbar's leftmost button shows and hides the overflow that holds the hidden icons.
    $chevron = $area.FindAll($children, (New-Condition $element::ClassNameProperty 'Button')) |
        Sort-Object { $_.Current.BoundingRectangle.Left } | Select-Object -First 1
    $cover = if ($chevron) { $chevron.Current.BoundingRectangle } else { [System.Windows.Rect]::Empty }
    try {
        $menu = $null
        for ($attempt = 1; $attempt -le 3 -and !$menu; $attempt++) {
            $shellIcon = Find-ShellIcon
            # While an icon is hidden, the shell reports the button that reveals it, or nothing.
            $behind = [bool]($shellIcon -and (Test-Covers $cover $shellIcon.Rect))
            Say "attempt $attempt: shell icon $(if ($shellIcon) { "id=$($shellIcon.Id) at $(Say-Rect $shellIcon.Rect)" } else { 'none' }); chevron $(Say-ElementRect $cover); behind it: $behind"
            $placed = $null
            if ($shellIcon -and !$behind) {
                $placed = @{ Icon = $shellIcon; Hidden = $false }
            } else {
                if (!$chevron) { throw "The shell has no '$Tooltip' icon and the taskbar has no hidden-icons button" }
                if (& $hidden) { Invoke-Click $chevron Left }
                $null = Wait-For { $flyout = & $overflow; $flyout -and !$flyout.Current.IsOffscreen } 'The hidden-icons button did not open the notification overflow'
                $placed = Wait-For {
                    $found = Find-ShellIcon
                    if (!$found) { return $null }
                    $flyout = & $overflow
                    if ($flyout -and !$flyout.Current.IsOffscreen -and (Test-Inside $found.Rect $flyout.Current.BoundingRectangle)) { return @{ Icon = $found; Hidden = $true } }
                    if (!(Test-Covers $cover $found.Rect)) { return @{ Icon = $found; Hidden = $false } }
                    $null
                } '' 5
                if (!$placed) { throw "The shell has no '$Tooltip' icon for the host's tray window, so the host registered none" }
                if (!$placed.Hidden) { Close-Overflow }
            }
            $shellIcon = $placed.Icon
            $location = if ($placed.Hidden) { 'notification overflow' } else { 'notification area' }
            $iconRect = $shellIcon.Rect
            $x = [int](($iconRect.Left + $iconRect.Right) / 2)
            $y = [int](($iconRect.Top + $iconRect.Bottom) / 2)
            Say "shell icon id=$($shellIcon.Id) at $(Say-Rect $iconRect) in the $location"
            if ($attempt -eq 1) {
                Save-Screenshot '2-icon'
                if ($placed.Hidden) {
                    $flyoutHandle = [IntPtr]((& $overflow).Current.NativeWindowHandle)
                    Say "overflow: $([VoicelessTest.Tray]::Describe($flyoutHandle))"
                    foreach ($window in [VoicelessTest.Tray]::Children($flyoutHandle)) {
                        Say "overflow child $([VoicelessTest.Tray]::Describe($window))"
                        Say-AccChildren $window 'overflow'
                    }
                    Read-Tree51 $flyoutHandle
                }
            }
            Click-At $x $y Right
            $menu = Wait-For { Find-Menu $ProcessId } '' 3
            if (!$menu) {
                # A menu of another program may have opened on the wrong icon.
                if (Find-Class $root $children '#32768') { Send-Escape }
                Close-Overflow
            }
        }
        if (!$menu) { throw "The '$Tooltip' icon did not open a menu owned by process $ProcessId" }
        $menuHandle = [IntPtr]$menu.Current.NativeWindowHandle
        Say "host menu: $([VoicelessTest.Tray]::Describe($menuHandle))"
        Save-Screenshot '3-menu'
        Read-Tree51 $menuHandle
        $accessible = Get-Accessible $menuHandle
        $items = @(Get-AccChildren $accessible)
        Say "menu MSAA: role=$(Get-AccValue $accessible 'accRole') children=$($accessible.accChildCount); offers: $(($items | ForEach-Object { $_.Name }) -join ', ')"
        $command = $items | Where-Object { $_.Name -eq $Item } | Select-Object -First 1
        if (!$command) { throw "The tray menu has no '$Item'; it offers: $(($items | ForEach-Object { $_.Name }) -join ', ')" }
        $bounds = $command.Location
        Say "choosing '$Item' at ($($bounds[0]),$($bounds[1]) $($bounds[2])x$($bounds[3]))"
        Click-At ([int]($bounds[0] + $bounds[2] / 2)) ([int]($bounds[1] + $bounds[3] / 2)) Left
        $null = Wait-For { !(Find-Menu $ProcessId) } "The tray menu stayed open after '$Item' was chosen"
        Close-Overflow
        $null = Wait-For $hidden 'The notification overflow stayed open' 5
        Save-Screenshot '4-after'
        return "Tray icon (shell id $($shellIcon.Id) at $(Say-Rect $iconRect)) observed in the $location; its menu, owned by process $ProcessId, offers [$(($items | ForEach-Object { $_.Name }) -join ', ')]; '$Item' chosen"
    } finally {
        if (Find-Menu @($ProcessId, $taskbar.Current.ProcessId)) { Send-Escape }
        Close-Overflow
    }
}
