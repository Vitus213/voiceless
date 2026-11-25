using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;

// Register the test's root-enumerated media device after its signed package
// has been staged by PnPUtil. Device Manager uses the same SetupAPI mechanism.
public static class RootDevice {
  [StructLayout(LayoutKind.Sequential)]
  struct DevInfoData {
    public uint cbSize;
    public Guid ClassGuid;
    public uint DevInst;
    public UIntPtr Reserved;
  }

  [DllImport("setupapi.dll", SetLastError = true, CharSet = CharSet.Unicode)]
  static extern IntPtr SetupDiCreateDeviceInfoList(IntPtr classGuid, IntPtr parent);
  [DllImport("setupapi.dll", SetLastError = true)]
  static extern bool SetupDiDestroyDeviceInfoList(IntPtr set);
  [DllImport("setupapi.dll", SetLastError = true, CharSet = CharSet.Unicode)]
  static extern bool SetupDiCreateDeviceInfoW(IntPtr set, string deviceName, ref Guid classGuid, string description, IntPtr parent, uint flags, ref DevInfoData data);
  [DllImport("setupapi.dll", SetLastError = true)]
  static extern bool SetupDiSetDeviceRegistryPropertyW(IntPtr set, ref DevInfoData data, uint property, byte[] buffer, uint size);
  [DllImport("setupapi.dll", SetLastError = true)]
  static extern bool SetupDiRegisterDeviceInfo(IntPtr set, ref DevInfoData data, uint flags, IntPtr installParams, IntPtr callback, IntPtr context);
  [DllImport("cfgmgr32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
  static extern uint CM_Get_Device_IDW(uint devInst, IntPtr buffer, uint size, uint flags);

  // SPDRP_HARDWAREID and DICD_GENERATE_ID from setupapi.h.
  const uint HardwareIdProperty = 1;
  const uint GenerateId = 0x00000001;
  static readonly IntPtr InvalidSet = new IntPtr(-1);

  static void Check(bool ok, string call) {
    if (!ok) throw new Win32Exception(Marshal.GetLastWin32Error(), call);
  }

  /// <summary>Returns the instance ID of the new device node.</summary>
  public static string Create(string hardwareId, string classGuid, string baseName) {
    Guid guid = new Guid(classGuid);
    IntPtr set = SetupDiCreateDeviceInfoList(IntPtr.Zero, IntPtr.Zero);
    if (set == IntPtr.Zero || set == InvalidSet) {
      throw new Win32Exception(Marshal.GetLastWin32Error(), "SetupDiCreateDeviceInfoList");
    }
    try {
      DevInfoData data = new DevInfoData();
      data.cbSize = (uint)Marshal.SizeOf(typeof(DevInfoData));
      Check(SetupDiCreateDeviceInfoW(set, baseName, ref guid, null, IntPtr.Zero, GenerateId, ref data), "SetupDiCreateDeviceInfo");
      // The hardware ID list is a REG_MULTI_SZ, so it ends with a second terminator.
      byte[] ids = Encoding.Unicode.GetBytes(hardwareId + "\0\0");
      Check(SetupDiSetDeviceRegistryPropertyW(set, ref data, HardwareIdProperty, ids, (uint)ids.Length), "SetupDiSetDeviceRegistryProperty");
      Check(SetupDiRegisterDeviceInfo(set, ref data, 0, IntPtr.Zero, IntPtr.Zero, IntPtr.Zero), "SetupDiRegisterDeviceInfo");
      IntPtr buffer = Marshal.AllocHGlobal(1024);
      try {
        if (CM_Get_Device_IDW(data.DevInst, buffer, 512, 0) != 0) return "<id unavailable>";
        return Marshal.PtrToStringUni(buffer);
      } finally {
        Marshal.FreeHGlobal(buffer);
      }
    } finally {
      SetupDiDestroyDeviceInfoList(set);
    }
  }
}
