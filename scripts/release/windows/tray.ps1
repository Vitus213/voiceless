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
private static extern int AccessibleObjectFromWindow(IntPtr window, uint objectId, ref Guid interfaceId, [MarshalAs(UnmanagedType.IUnknown)] out object accessible);
[DllImport("kernel32.dll", SetLastError = true)]
private static extern IntPtr OpenProcess(uint access, bool inherit, int processId);
[DllImport("kernel32.dll")]
private static extern bool CloseHandle(IntPtr handle);
[DllImport("advapi32.dll", SetLastError = true)]
private static extern bool OpenProcessToken(IntPtr process, uint access, out IntPtr token);
[DllImport("advapi32.dll", SetLastError = true)]
private static extern bool GetTokenInformation(IntPtr token, int informationClass, IntPtr information, int length, out int returned);
[DllImport("advapi32.dll")]
private static extern IntPtr GetSidSubAuthorityCount(IntPtr sid);
[DllImport("advapi32.dll")]
private static extern IntPtr GetSidSubAuthority(IntPtr sid, uint index);

// IAccessible, the documented MSAA interface of Win32 menus and toolbars.
[ComImport, Guid("618736E0-3C3D-11CF-810C-00AA00389B71"), InterfaceType(ComInterfaceType.InterfaceIsIDispatch)]
public interface IDispatchAccessible {
    [DispId(-5001)] int accChildCount { get; }
    [DispId(-5003)] string accName(object child);
    [DispId(-5006)] object accRole(object child);
    [DispId(-5015)] void accLocation(out int left, out int top, out int width, out int height, object child);
}
[ComImport, Guid("618736E0-3C3D-11CF-810C-00AA00389B71"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
public interface IVtableAccessible {
    [PreserveSig] int GetTypeInfoCount(out int count);
    [PreserveSig] int GetTypeInfo(int info, int lcid, out IntPtr typeInfo);
    [PreserveSig] int GetIDsOfNames(in Guid iid, IntPtr names, int count, int lcid, IntPtr ids);
    [PreserveSig] int Invoke(int id, in Guid iid, int lcid, short flags, IntPtr parameters, IntPtr result, IntPtr exceptionInfo, IntPtr argumentError);
    object accParent { get; }
    int accChildCount { get; }
    [return: MarshalAs(UnmanagedType.IDispatch)] object accChild(object child);
    string accName(object child);
    string accValue(object child);
    string accDescription(object child);
    object accRole(object child);
    object accState(object child);
    string accHelp(object child);
    int accHelpTopic(out string helpFile, object child);
    string accKeyboardShortcut(object child);
    object accFocus { get; }
    object accSelection { get; }
    string accDefaultAction(object child);
    void accSelect(int flags, object child);
    void accLocation(out int left, out int top, out int width, out int height, object child);
    object accNavigate(int direction, object start);
    object accHitTest(int left, int top);
    void accDoDefaultAction(object child);
}
public sealed class AccItem {
    public int Index; public string Name; public int Role; public int Left; public int Top; public int Width; public int Height;
    public string At { get { return "(" + Left + "," + Top + " " + Width + "x" + Height + ")"; } }
}

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
public static string Integrity(int processId) {
    // The mandatory integrity level of another process, as UIPI sees it.
    IntPtr process = OpenProcess(0x1000, false, processId);
    if (process == IntPtr.Zero) return "unavailable";
    try {
        IntPtr token;
        if (!OpenProcessToken(process, 0x0008, out token)) return "unavailable";
        try {
            int size;
            GetTokenInformation(token, 25, IntPtr.Zero, 0, out size);
            IntPtr buffer = Marshal.AllocHGlobal(size);
            try {
                if (!GetTokenInformation(token, 25, buffer, size, out size)) return "unavailable";
                IntPtr sid = Marshal.ReadIntPtr(buffer);
                int count = Marshal.ReadByte(GetSidSubAuthorityCount(sid));
                int level = Marshal.ReadInt32(GetSidSubAuthority(sid, (uint)(count - 1)));
                return "0x" + level.ToString("X");
            } finally { Marshal.FreeHGlobal(buffer); }
        } finally { CloseHandle(token); }
    } finally { CloseHandle(process); }
}
public static AccItem[] Items(IntPtr window, bool dispatch) {
    Guid iid = typeof(IDispatchAccessible).GUID;
    object accessible;
    int result = AccessibleObjectFromWindow(window, 0xFFFFFFFC, ref iid, out accessible);
    if (result != 0 || accessible == null) throw new System.ComponentModel.Win32Exception(result, "AccessibleObjectFromWindow failed");
    AccItem[] items = new AccItem[(dispatch ? ((IDispatchAccessible)accessible).accChildCount : ((IVtableAccessible)accessible).accChildCount)];
    for (int index = 0; index < items.Length; index++) {
        AccItem item = new AccItem();
        item.Index = index + 1;
        try {
            object role;
            if (dispatch) {
                IDispatchAccessible a = (IDispatchAccessible)accessible;
                item.Name = a.accName(item.Index) ?? "";
                role = a.accRole(item.Index);
                a.accLocation(out item.Left, out item.Top, out item.Width, out item.Height, item.Index);
            } else {
                IVtableAccessible a = (IVtableAccessible)accessible;
                item.Name = a.accName(item.Index) ?? "";
                role = a.accRole(item.Index);
                a.accLocation(out item.Left, out item.Top, out item.Width, out item.Height, item.Index);
            }
            item.Role = role is int ? (int)role : -1;
        } catch (System.Runtime.InteropServices.COMException error) {
            item.Role = -1;
            item.Name = "<" + error.ErrorCode.ToString("X8") + ">";
        }
        items[index] = item;
    }
    return items;
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
    function Get-Items([IntPtr]$Window, [string]$Label) {
        # MSAA children of a window, through IDispatch or else through the vtable.
        foreach ($dispatch in $true, $false) {
            try {
                $items = [VoicelessTest.Tray]::Items($Window, $dispatch)
                Say "$Label MSAA by $(if ($dispatch) { 'IDispatch' } else { 'vtable' }): $(($items | ForEach-Object { '#' + $_.Index + ' [' + $_.Name + '] role=' + $_.Role + ' ' + $_.At }) -join '; ')"
                return $items
            } catch { Say "$Label MSAA by $(if ($dispatch) { 'IDispatch' } else { 'vtable' }) failed: $($_.Exception.Message)" }
        }
        $null
    }

    function Show-Windows([IntPtr]$Parent, [int]$Depth, [string]$Label) {
        # Child windows with their MSAA children, two levels deep.
        foreach ($window in [VoicelessTest.Tray]::Children($Parent)) {
            Say (('  ' * $Depth) + "$Label window $([VoicelessTest.Tray]::Describe($window))")
            $null = Get-Items $window $Label
            if ($Depth -lt 1) { Show-Windows $window ($Depth + 1) $Label }
        }
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
        Say "integrity: this process $([VoicelessTest.Tray]::Integrity($PID)), Explorer $([VoicelessTest.Tray]::Integrity($taskbar.Current.ProcessId)), host $([VoicelessTest.Tray]::Integrity($ProcessId)); host tray window $([VoicelessTest.Tray]::Describe($trays[0]))"
        Get-ChildItem 'HKCU:\Control Panel\NotifyIconSettings' -ErrorAction SilentlyContinue | ForEach-Object {
            $entry = Get-ItemProperty $_.PSPath
            if ("$($entry.ExecutableName)$($entry.InitialTooltip)" -match 'voiceless') { Say "NotifyIconSettings $($_.PSChildName): ExecutableName=$($entry.ExecutableName) IsPromoted=$($entry.IsPromoted) ShowInLegend=$($entry.ShowInLegend) InitialTooltip=$($entry.InitialTooltip) UID=$($entry.UID)" }
        }
        $menu = $null
        for ($attempt = 1; $attempt -le 3 -and !$menu; $attempt++) {
            $shellIcon = Find-ShellIcon
            # While an icon is hidden, the shell reports the button that reveals it, or nothing.
            $behind = [bool]($shellIcon -and (Test-Covers $cover $shellIcon.Rect))
            Say "attempt ${attempt}: shell icon $(if ($shellIcon) { "id=$($shellIcon.Id) at $(Say-Rect $shellIcon.Rect)" } else { 'none' }); chevron $(Say-ElementRect $cover); behind it: $behind"
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
                Show-Windows ([IntPtr]$area.Current.NativeWindowHandle) 0 'area'
                if ($placed.Hidden) { Show-Windows ([IntPtr]((& $overflow).Current.NativeWindowHandle)) 0 'overflow' }
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
        $menuRect = $menu.Current.BoundingRectangle
        Say "host menu: $([VoicelessTest.Tray]::Describe($menuHandle))"
        Save-Screenshot '3-menu'
        $items = Get-Items $menuHandle 'menu'
        $bounds = $null
        $how = ''
        $command = $items | Where-Object { $_.Name -eq $Item } | Select-Object -First 1
        if ($command) {
            $bounds = @($command.Left, $command.Top, $command.Width, $command.Height)
            $how = "'$Item' found through MSAA"
        } else {
            Say "MSAA did not name '$Item' (items: $($items.Count)); the product lists it first, so the first row of the popup is used"
            $bounds = @($menuRect.Left, $menuRect.Top, $menuRect.Width, 24)
            $how = 'first row by position; its items were not readable through MSAA'
        }
        Click-At ([int]($bounds[0] + $bounds[2] / 2)) ([int]($bounds[1] + $bounds[3] / 2)) Left
        $null = Wait-For { !(Find-Menu $ProcessId) } "The tray menu stayed open after '$Item' was chosen"
        Close-Overflow
        $null = Wait-For $hidden 'The notification overflow stayed open' 5
        Save-Screenshot '4-after'
        return "Tray icon (shell id $($shellIcon.Id) at $(Say-Rect $iconRect)) observed in the $location; its menu, owned by process $ProcessId, offers [$(($items | ForEach-Object { $_.Name }) -join ', ')]; '$Item' chosen, $how"
    } finally {
        if (Find-Menu @($ProcessId, $taskbar.Current.ProcessId)) { Send-Escape }
        Close-Overflow
    }
}
