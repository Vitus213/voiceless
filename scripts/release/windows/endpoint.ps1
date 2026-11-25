param([Parameter(Mandatory)][ValidateSet('enable','disable')][string]$State)
$ErrorActionPreference = 'Stop'
if ($env:RUNNER_ENVIRONMENT -ne 'github-hosted' -or $env:VOICELESS_VAC_MIC -notlike 'wasapi:{0.0.1.*') {
    throw 'The virtual microphone on the hosted evaluation guest is required'
}
# The Sound control panel changes endpoint visibility. Disabling the software
# device in Plug and Play leaves it active in the WASAPI endpoint enumerator.
Add-Type @'
using System;
using System.Runtime.InteropServices;

[ComImport, Guid("F8679F50-850A-41CF-9C72-430F290290C8"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IPolicyConfig {
    void GetMixFormat();
    void GetDeviceFormat();
    void ResetDeviceFormat();
    void SetDeviceFormat();
    void GetProcessingPeriod();
    void SetProcessingPeriod();
    void GetShareMode();
    void SetShareMode();
    void GetPropertyValue();
    void SetPropertyValue();
    void SetDefaultEndpoint();
    void SetEndpointVisibility([MarshalAs(UnmanagedType.LPWStr)] string endpoint, short visible);
}

public static class EndpointSwitch {
    public static void Set(string id, bool visible) {
        var cls = Type.GetTypeFromCLSID(new Guid("870AF99C-171D-4F9E-AF0D-E63DF40C2BC9"), true);
        var client = (IPolicyConfig)Activator.CreateInstance(cls);
        try { client.SetEndpointVisibility(id, (short)(visible ? 1 : 0)); }
        finally { Marshal.FinalReleaseComObject(client); }
    }
}
'@
[EndpointSwitch]::Set($env:VOICELESS_VAC_MIC.Substring(7), $State -eq 'enable')
Write-Output "Core Audio endpoint $State $env:VOICELESS_VAC_MIC"
