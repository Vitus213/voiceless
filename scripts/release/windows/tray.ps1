# Reopen the hidden Voiceless window through the installed host's real notification
# icon, the way a user does. The shell is asked where it put the icon; while the
# icon is hidden, the taskbar's leftmost button shows the overflow that holds it.
# The pointer then opens the icon's context menu and chooses one of its commands,
# which MSAA reads. Dot-source it once window.ps1 has hidden the window:
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
[DllImport("user32.dll")]
private static extern uint GetWindowThreadProcessId(IntPtr window, out uint owner);
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

// IAccessible in the order oleacc.h declares it, after IDispatch: it is what Win32 popup menus expose.
[ComImport, Guid("618736E0-3C3D-11CF-810C-00AA00389B71"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
public interface IAccessible {
    [PreserveSig] int GetTypeInfoCount(out int count);
    [PreserveSig] int GetTypeInfo(int info, int lcid, out IntPtr typeInfo);
    [PreserveSig] int GetIDsOfNames(in Guid iid, IntPtr names, int count, int lcid, IntPtr ids);
    [PreserveSig] int Invoke(int id, in Guid iid, int lcid, short flags, IntPtr parameters, IntPtr result, IntPtr exceptionInfo, IntPtr argumentError);
    object accParent { get; }
    int accChildCount { get; }
    object accChild(object child);
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
}

// The hidden window tray-icon keeps for the host's notification icon.
public static IntPtr TrayWindow(uint processId) {
    IntPtr window = IntPtr.Zero;
    while ((window = FindWindowExW(IntPtr.Zero, window, "tray_icon_app", null)) != IntPtr.Zero) {
        GetWindowThreadProcessId(window, out uint owner);
        if (owner == processId) return window;
    }
    return IntPtr.Zero;
}
// Where the shell put one of the host's icons; null when the host has none with that number.
public static RECT? IconRect(IntPtr tray, uint id) {
    NOTIFYICONIDENTIFIER identifier = new NOTIFYICONIDENTIFIER();
    identifier.cbSize = (uint)Marshal.SizeOf(typeof(NOTIFYICONIDENTIFIER));
    identifier.hWnd = tray;
    identifier.uID = id;
    RECT rect;
    return Shell_NotifyIconGetRect(ref identifier, out rect) == 0 ? rect : (RECT?)null;
}
// The items of a popup menu window as MSAA reports them. A field MSAA does not give stays empty.
public static AccItem[] MenuItems(IntPtr menu) {
    Guid iid = typeof(IAccessible).GUID;
    object accessible;
    int result = AccessibleObjectFromWindow(menu, 0xFFFFFFFC, ref iid, out accessible);
    if (result != 0 || accessible == null) throw new System.ComponentModel.Win32Exception(result, "The menu exposes no MSAA object");
    IAccessible popup = (IAccessible)accessible;
    AccItem[] items = new AccItem[popup.accChildCount];
    for (int index = 0; index < items.Length; index++) {
        AccItem item = new AccItem();
        item.Index = index + 1;
        try {
            item.Name = popup.accName(item.Index) ?? "";
            item.Role = (int)popup.accRole(item.Index);
            popup.accLocation(out item.Left, out item.Top, out item.Width, out item.Height, item.Index);
        } catch (System.Runtime.InteropServices.COMException) { }
        items[index] = item;
    }
    return items;
}
'@
    }
    $element = [System.Windows.Automation.AutomationElement]
    $children = [System.Windows.Automation.TreeScope]::Children
    $descendants = [System.Windows.Automation.TreeScope]::Descendants
    $root = $element::RootElement
    # The window Explorer shows for the hidden icons; it exists only while shown.
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
    function Format-Rect($Rect) {
        '({0},{1})-({2},{3})' -f $Rect.Left, $Rect.Top, $Rect.Right, $Rect.Bottom
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
        if ($rect.IsEmpty) { throw 'The target is not on screen' }
        Click-At ([int]($rect.Left + $rect.Width / 2)) ([int]($rect.Top + $rect.Height / 2)) $Button
    }
    function Send-Escape {
        # keybd_event: VK_ESCAPE 0x1B, then the same key with KEYEVENTF_KEYUP 0x0002.
        [VoicelessTest.Tray]::keybd_event(0x1B, 0, 0, [UIntPtr]::Zero)
        [VoicelessTest.Tray]::keybd_event(0x1B, 0, 2, [UIntPtr]::Zero)
    }
    function Close-Overflow {
        # Explorer closes the overflow once the pointer has left it or another window took
        # focus. If it is still open, the taskbar's button, which toggles it, closes it.
        Move-Pointer ([VoicelessTest.Tray]::GetSystemMetrics(0) / 2) ([VoicelessTest.Tray]::GetSystemMetrics(1) / 2)
        $deadline = [DateTime]::UtcNow.AddSeconds(2)
        while ($chevron -and !(& $hidden) -and [DateTime]::UtcNow -lt $deadline) { Start-Sleep -Milliseconds 100 }
        if ($chevron -and !(& $hidden)) { Invoke-Click $chevron Left }
    }
    function Find-ShellIcon {
        # tray-icon numbers its icons from a process-wide counter, so try the first few.
        for ($id = 0; $id -lt 16; $id++) {
            $rect = [VoicelessTest.Tray]::IconRect($tray, $id)
            if ($rect) { return [pscustomobject]@{ Id = $id; Rect = $rect } }
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
    if (!$taskbar) { throw "Explorer has no taskbar (Shell_TrayWnd) in this session, so the host's notification icon cannot be observed" }
    $area = Find-Class $taskbar $descendants 'TrayNotifyWnd'
    if (!$area) { throw 'The Explorer taskbar has no notification area (TrayNotifyWnd)' }
    $tray = [VoicelessTest.Tray]::TrayWindow($ProcessId)
    if ($tray -eq [IntPtr]::Zero) { throw "The host has no notification-area window of its own (process $ProcessId), so it built no tray" }
    $chevron = $area.FindAll($children, (New-Condition $element::ClassNameProperty 'Button')) |
        Sort-Object { $_.Current.BoundingRectangle.Left } | Select-Object -First 1
    # Where the taskbar's hidden-icons button is; it moves when an icon is promoted.
    $cover = { if ($chevron) { $chevron.Current.BoundingRectangle } else { [System.Windows.Rect]::Empty } }
    try {
        # Where the shell puts the icon can change while this runs, so each miss is judged again.
        $menu = $null
        for ($attempt = 1; $attempt -le 3 -and !$menu; $attempt++) {
            $placed = $null
            $shellIcon = Find-ShellIcon
            if ($shellIcon -and !(Test-Covers (& $cover) $shellIcon.Rect)) {
                # While an icon is hidden, the shell reports the button that reveals it, or nothing.
                $placed = @{ Icon = $shellIcon; Hidden = $false }
            } else {
                if (!$chevron) { throw "The shell has no notification icon of the host and the taskbar has no hidden-icons button" }
                if (& $hidden) { Invoke-Click $chevron Left }
                $null = Wait-For { $flyout = & $overflow; $flyout -and !$flyout.Current.IsOffscreen } 'The hidden-icons button did not open the notification overflow'
                $placed = Wait-For {
                    $found = Find-ShellIcon
                    if (!$found) { return $null }
                    $flyout = & $overflow
                    if ($flyout -and !$flyout.Current.IsOffscreen -and (Test-Inside $found.Rect $flyout.Current.BoundingRectangle)) { return @{ Icon = $found; Hidden = $true } }
                    if (!(Test-Covers (& $cover) $found.Rect)) { return @{ Icon = $found; Hidden = $false } }
                    $null
                } '' 5
                if (!$placed) { throw "The shell has no notification icon for the host's tray window, so the host registered none" }
                if (!$placed.Hidden) { Close-Overflow }
            }
            $iconRect = $placed.Icon.Rect
            $location = if ($placed.Hidden) { 'notification overflow' } else { 'notification area' }
            Click-At ([int](($iconRect.Left + $iconRect.Right) / 2)) ([int](($iconRect.Top + $iconRect.Bottom) / 2)) Right
            $menu = Wait-For { Find-Menu $ProcessId } '' 3
            if (!$menu) {
                # A menu of another program may have opened on the wrong icon.
                if (Find-Class $root $children '#32768') { Send-Escape }
                Close-Overflow
            }
        }
        if (!$menu) { throw "The host's notification icon did not open a menu owned by process $ProcessId" }
        $items = @()
        $note = ''
        try { $items = [VoicelessTest.Tray]::MenuItems([IntPtr]$menu.Current.NativeWindowHandle) }
        catch { $note = "; MSAA could not read the menu ($($_.Exception.Message))" }
        $command = $items | Where-Object { $_.Name -eq $Item } | Select-Object -First 1
        $how = 'chosen by the name MSAA reports'
        if ($command) {
            $x = [int]($command.Left + $command.Width / 2)
            $y = [int]($command.Top + $command.Height / 2)
        } else {
            # The product lists the command first, and a popup's first row is its topmost.
            $bounds = $menu.Current.BoundingRectangle
            $x = [int]($bounds.Left + $bounds.Width / 2)
            $y = [int]($bounds.Top + 12)
            $how = "chosen on the menu's first row because MSAA did not name it"
        }
        Click-At $x $y Left
        $null = Wait-For { !(Find-Menu $ProcessId) } "The tray menu stayed open after '$Item' was chosen"
        Close-Overflow
        $null = Wait-For $hidden 'The notification overflow stayed open' 5
        return "Tray icon (shell id $($placed.Icon.Id) at $(Format-Rect $iconRect)) observed in the $location; its menu, owned by process $ProcessId, offers [$(($items | ForEach-Object { $_.Name }) -join ', ')]; '$Item' $how$note"
    } finally {
        # Leave the shell as found, whatever went wrong above.
        if (Find-Menu @($ProcessId, $taskbar.Current.ProcessId)) { Send-Escape }
        Close-Overflow
    }
}
