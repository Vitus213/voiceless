using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;

// Puts plain text on the clipboard the way a native text copy does: only
// CF_UNICODETEXT, without the private formats an OLE data object adds.
public static class ClipboardText {
  const uint Movable = 0x0002, ZeroInit = 0x0040, UnicodeText = 13;

  [DllImport("user32.dll", SetLastError = true)]
  static extern bool OpenClipboard(IntPtr owner);
  [DllImport("user32.dll", SetLastError = true)]
  static extern bool EmptyClipboard();
  [DllImport("user32.dll", SetLastError = true)]
  static extern bool CloseClipboard();
  [DllImport("user32.dll", SetLastError = true)]
  static extern IntPtr SetClipboardData(uint format, IntPtr data);
  [DllImport("kernel32.dll", SetLastError = true)]
  static extern IntPtr GlobalAlloc(uint flags, UIntPtr bytes);
  [DllImport("kernel32.dll", SetLastError = true)]
  static extern IntPtr GlobalLock(IntPtr memory);
  [DllImport("kernel32.dll")]
  static extern bool GlobalUnlock(IntPtr memory);
  [DllImport("kernel32.dll")]
  static extern IntPtr GlobalFree(IntPtr memory);

  /// <summary>Replaces the clipboard with the text.</summary>
  public static void Set(string text) {
    byte[] bytes = Encoding.Unicode.GetBytes(text + "\0");
    IntPtr memory = GlobalAlloc(Movable | ZeroInit, (UIntPtr)bytes.Length);
    if (memory == IntPtr.Zero) throw new Win32Exception(Marshal.GetLastWin32Error(), "GlobalAlloc");
    IntPtr target = GlobalLock(memory);
    if (target == IntPtr.Zero) {
      GlobalFree(memory);
      throw new Win32Exception(Marshal.GetLastWin32Error(), "GlobalLock");
    }
    try {
      Marshal.Copy(bytes, 0, target, bytes.Length);
    } finally {
      GlobalUnlock(memory);
    }
    if (!OpenClipboard(IntPtr.Zero)) {
      GlobalFree(memory);
      throw new Win32Exception(Marshal.GetLastWin32Error(), "OpenClipboard");
    }
    try {
      if (!EmptyClipboard()) throw new Win32Exception(Marshal.GetLastWin32Error(), "EmptyClipboard");
      // From here on the system owns the memory.
      if (SetClipboardData(UnicodeText, memory) == IntPtr.Zero) {
        throw new Win32Exception(Marshal.GetLastWin32Error(), "SetClipboardData");
      }
    } finally {
      CloseClipboard();
    }
  }
}
