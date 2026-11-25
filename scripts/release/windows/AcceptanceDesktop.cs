using System;
using System.Runtime.InteropServices;
using System.Threading;

// Window queries and pointer clicks on the interactive desktop, for the harness
// that stands in for the user's hand. The clicks are synthetic input: no person
// pressed a button or a key.
public static class AcceptanceDesktop {
  [StructLayout(LayoutKind.Sequential)]
  public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }

  [DllImport("user32.dll")]
  public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll", CharSet = CharSet.Unicode)]
  public static extern IntPtr FindWindowExW(IntPtr parent, IntPtr after, string className, string title);
  [DllImport("user32.dll")]
  public static extern uint GetWindowThreadProcessId(IntPtr window, out uint owner);
  [DllImport("user32.dll")]
  public static extern bool IsWindowVisible(IntPtr window);
  [DllImport("user32.dll")]
  public static extern bool GetWindowRect(IntPtr window, out RECT rect);
  [DllImport("user32.dll")]
  public static extern bool PostMessageW(IntPtr window, uint message, IntPtr wParam, IntPtr lParam);
  [DllImport("user32.dll")]
  public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")]
  public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extra);
  [DllImport("user32.dll")]
  public static extern bool SetForegroundWindow(IntPtr window);
  [DllImport("user32.dll")]
  static extern void keybd_event(byte key, byte scan, uint flags, UIntPtr extra);

  /// <summary>The visible top-level window titled "Voiceless" that the process
  /// owns, or zero.</summary>
  public static IntPtr VisibleWindow(uint processId) {
    IntPtr window = IntPtr.Zero;
    while ((window = FindWindowExW(IntPtr.Zero, window, null, "Voiceless")) != IntPtr.Zero) {
      uint owner;
      GetWindowThreadProcessId(window, out owner);
      if (owner == processId && IsWindowVisible(window)) return window;
    }
    return IntPtr.Zero;
  }

  /// <summary>The process of the window in front, or zero.</summary>
  public static uint ForegroundOwner() {
    uint owner;
    GetWindowThreadProcessId(GetForegroundWindow(), out owner);
    return owner;
  }

  /// <summary>Alt pressed and released lets this process hand the foreground
  /// to the window, as happens when a person gives a window its attention.</summary>
  public static void BringForward(IntPtr window) {
    // keybd_event: VK_MENU 0x12, then the same key with KEYEVENTF_KEYUP 0x0002.
    keybd_event(0x12, 0, 0, UIntPtr.Zero);
    keybd_event(0x12, 0, 2, UIntPtr.Zero);
    SetForegroundWindow(window);
  }

  public static bool RequestClose(IntPtr window) {
    // WM_CLOSE
    return PostMessageW(window, 0x0010, IntPtr.Zero, IntPtr.Zero);
  }

  /// <summary>One left click in the middle of the window, where a person clicks
  /// to give it the focus.</summary>
  public static void Click(IntPtr window) {
    RECT rect;
    if (!GetWindowRect(window, out rect)) throw new InvalidOperationException("The window has no place on screen.");
    SetCursorPos((rect.Left + rect.Right) / 2, (rect.Top + rect.Bottom) / 2);
    Thread.Sleep(150);
    // mouse_event flags: LEFTDOWN 0x0002, LEFTUP 0x0004.
    mouse_event(0x0002, 0, 0, 0, UIntPtr.Zero);
    mouse_event(0x0004, 0, 0, 0, UIntPtr.Zero);
  }
}
