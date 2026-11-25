using System;
using System.ComponentModel;
using System.Runtime.InteropServices;

// Read-only token queries: which account a process runs as and at which
// integrity level Windows runs it.
public static class ProcessToken {
  const uint QueryLimited = 0x1000, TokenQuery = 0x0008;
  // TOKEN_INFORMATION_CLASS values for TOKEN_USER and TOKEN_MANDATORY_LABEL.
  const int TokenUser = 1, TokenIntegrityLevel = 25;

  [DllImport("kernel32.dll", SetLastError = true)]
  static extern IntPtr OpenProcess(uint access, bool inherit, uint processId);
  [DllImport("advapi32.dll", SetLastError = true)]
  static extern bool OpenProcessToken(IntPtr process, uint access, out IntPtr token);
  [DllImport("advapi32.dll", SetLastError = true)]
  static extern bool GetTokenInformation(IntPtr token, int kind, IntPtr information, int size, out int length);
  [DllImport("advapi32.dll")]
  static extern uint GetLengthSid(IntPtr sid);
  [DllImport("advapi32.dll")]
  static extern IntPtr GetSidSubAuthorityCount(IntPtr sid);
  [DllImport("advapi32.dll")]
  static extern IntPtr GetSidSubAuthority(IntPtr sid, uint index);
  [DllImport("advapi32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
  static extern bool ConvertSidToStringSidW(IntPtr sid, out IntPtr text);
  [DllImport("kernel32.dll")]
  static extern IntPtr LocalFree(IntPtr memory);
  [DllImport("kernel32.dll")]
  static extern bool CloseHandle(IntPtr handle);

  static IntPtr Token(uint processId) {
    IntPtr process = OpenProcess(QueryLimited, false, processId);
    if (process == IntPtr.Zero) throw new Win32Exception(Marshal.GetLastWin32Error(), "OpenProcess");
    try {
      IntPtr token;
      if (!OpenProcessToken(process, TokenQuery, out token)) {
        throw new Win32Exception(Marshal.GetLastWin32Error(), "OpenProcessToken");
      }
      return token;
    } finally {
      CloseHandle(process);
    }
  }

  /// <summary>A private copy of the SID that TOKEN_USER or TOKEN_MANDATORY_LABEL
  /// points to; the caller frees it. Both structures begin with that pointer.</summary>
  static IntPtr CopiedSid(IntPtr token, int kind) {
    IntPtr information = Marshal.AllocHGlobal(64);
    try {
      int length;
      if (!GetTokenInformation(token, kind, information, 64, out length)) {
        throw new Win32Exception(Marshal.GetLastWin32Error(), "GetTokenInformation");
      }
      IntPtr sid = Marshal.ReadIntPtr(information);
      uint size = GetLengthSid(sid);
      IntPtr copy = Marshal.AllocHGlobal((IntPtr)size);
      for (uint i = 0; i < size; i++) {
        Marshal.WriteByte(copy, (int)i, Marshal.ReadByte(sid, (int)i));
      }
      return copy;
    } finally {
      Marshal.FreeHGlobal(information);
    }
  }

  /// <summary>The SID of the account the process runs as.</summary>
  public static string Account(uint processId) {
    IntPtr token = Token(processId);
    try {
      IntPtr sid = CopiedSid(token, TokenUser);
      try {
        IntPtr text;
        if (!ConvertSidToStringSidW(sid, out text)) {
          throw new Win32Exception(Marshal.GetLastWin32Error(), "ConvertSidToStringSidW");
        }
        try {
          return Marshal.PtrToStringUni(text);
        } finally {
          LocalFree(text);
        }
      } finally {
        Marshal.FreeHGlobal(sid);
      }
    } finally {
      CloseHandle(token);
    }
  }

  /// <summary>The integrity level as the RID of the token's mandatory label:
  /// 0x2000 is medium, 0x3000 high.</summary>
  public static uint Integrity(uint processId) {
    IntPtr token = Token(processId);
    try {
      IntPtr sid = CopiedSid(token, TokenIntegrityLevel);
      try {
        byte count = Marshal.ReadByte(GetSidSubAuthorityCount(sid));
        if (count == 0) throw new InvalidOperationException("The mandatory label names no level.");
        return unchecked((uint)Marshal.ReadInt32(GetSidSubAuthority(sid, (uint)(count - 1))));
      } finally {
        Marshal.FreeHGlobal(sid);
      }
    } finally {
      CloseHandle(token);
    }
  }
}
