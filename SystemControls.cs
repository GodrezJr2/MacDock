using System;
using System.Linq;
using System.Management;
using System.Runtime.InteropServices;
using System.Threading.Tasks;
using Microsoft.Win32;
using Windows.Devices.Radios;

namespace MacDock;

/// <summary>Master volume of the default playback device (Core Audio, IAudioEndpointVolume).</summary>
static class Volume
{
    [ComImport, Guid("BCDE0395-E52F-467C-8E3D-C4579291692E")] class MMDeviceEnumerator { }

    [ComImport, Guid("A95664D2-9614-4F35-A746-DE8DB63617E6"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IMMDeviceEnumerator
    {
        int NotImpl1();
        [PreserveSig] int GetDefaultAudioEndpoint(int dataFlow, int role, out IMMDevice device);
    }

    [ComImport, Guid("D666063F-1587-4E43-81F1-B948E807363F"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IMMDevice
    {
        [PreserveSig] int Activate(ref Guid iid, int clsCtx, IntPtr activationParams, [MarshalAs(UnmanagedType.IUnknown)] out object iface);
    }

    [ComImport, Guid("5CDF2C82-841E-4546-9722-0CF74078229A"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IAudioEndpointVolume
    {
        int RegisterControlChangeNotify(IntPtr notify);
        int UnregisterControlChangeNotify(IntPtr notify);
        int GetChannelCount(out uint count);
        int SetMasterVolumeLevel(float levelDb, ref Guid ctx);
        int SetMasterVolumeLevelScalar(float level, ref Guid ctx);
        int GetMasterVolumeLevel(out float levelDb);
        int GetMasterVolumeLevelScalar(out float level);
        int SetChannelVolumeLevel(uint channel, float levelDb, ref Guid ctx);
        int SetChannelVolumeLevelScalar(uint channel, float level, ref Guid ctx);
        int GetChannelVolumeLevel(uint channel, out float levelDb);
        int GetChannelVolumeLevelScalar(uint channel, out float level);
        int SetMute([MarshalAs(UnmanagedType.Bool)] bool mute, ref Guid ctx);
        int GetMute([MarshalAs(UnmanagedType.Bool)] out bool mute);
    }

    static IAudioEndpointVolume? Endpoint()
    {
        try
        {
            var en = (IMMDeviceEnumerator)new MMDeviceEnumerator();
            if (en.GetDefaultAudioEndpoint(0 /* eRender */, 1 /* eMultimedia */, out var dev) != 0) return null;
            var iid = typeof(IAudioEndpointVolume).GUID;
            return dev.Activate(ref iid, 23 /* CLSCTX_ALL */, IntPtr.Zero, out var o) == 0 ? (IAudioEndpointVolume)o : null;
        }
        catch { return null; }
    }

    /// <summary>0..1, or null when there is no output device.</summary>
    public static double? Get()
    {
        var ep = Endpoint();
        if (ep == null) return null;
        ep.GetMasterVolumeLevelScalar(out float v);
        ep.GetMute(out bool m);
        return m ? 0 : v;
    }

    public static void Set(double level)
    {
        var ep = Endpoint();
        if (ep == null) return;
        var g = Guid.Empty;
        ep.SetMasterVolumeLevelScalar((float)Math.Clamp(level, 0, 1), ref g);
        ep.SetMute(level <= 0.001, ref g);
    }
}

/// <summary>Built-in panel brightness (WMI WmiMonitorBrightness); external monitors are not covered.</summary>
static class Brightness
{
    public static int? Get()
    {
        try
        {
            using var s = new ManagementObjectSearcher(@"root\wmi", "SELECT CurrentBrightness FROM WmiMonitorBrightness");
            foreach (ManagementObject o in s.Get()) return Convert.ToInt32(o["CurrentBrightness"]);
        }
        catch { }
        return null;
    }

    public static void Set(int percent)
    {
        try
        {
            using var s = new ManagementObjectSearcher(@"root\wmi", "SELECT * FROM WmiMonitorBrightnessMethods");
            foreach (ManagementObject o in s.Get())
            {
                o.InvokeMethod("WmiSetBrightness", new object[] { (uint)1, (byte)Math.Clamp(percent, 0, 100) });
                break;
            }
        }
        catch { }
    }
}

static class Bluetooth
{
    static Radio? radio;
    static bool looked;

    public static async Task<Radio?> Radio()
    {
        if (looked) return radio;
        looked = true;
        try
        {
            await Windows.Devices.Radios.Radio.RequestAccessAsync();
            radio = (await Windows.Devices.Radios.Radio.GetRadiosAsync()).FirstOrDefault(r => r.Kind == RadioKind.Bluetooth);
        }
        catch { }
        return radio;
    }

    public static async Task SetOn(bool on)
    {
        if (await Radio() is { } r) { try { await r.SetStateAsync(on ? RadioState.On : RadioState.Off); } catch { } }
    }
}

/// <summary>Windows' light/dark theme for apps and the system.</summary>
static class Theme
{
    const string Key = @"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";

    public static bool Dark => Registry.CurrentUser.OpenSubKey(Key)?.GetValue("AppsUseLightTheme") is int v && v == 0;

    public static void SetDark(bool dark)
    {
        using var k = Registry.CurrentUser.CreateSubKey(Key);
        k.SetValue("AppsUseLightTheme", dark ? 0 : 1, RegistryValueKind.DWord);
        k.SetValue("SystemUsesLightTheme", dark ? 0 : 1, RegistryValueKind.DWord);
        // tell running apps (Explorer, Settings, WinUI apps) to re-read it
        SendMessageTimeoutW((IntPtr)0xFFFF, 0x1A /* WM_SETTINGCHANGE */, IntPtr.Zero, "ImmersiveColorSet", 2, 200, out _);
    }

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    static extern IntPtr SendMessageTimeoutW(IntPtr h, uint msg, IntPtr w, string l, uint flags, uint timeout, out IntPtr result);
}

/// <summary>Windows' power mode slider (Settings › Power): Best power efficiency / Balanced / Best performance.</summary>
static class PowerMode
{
    public static readonly (string Name, Guid Id)[] Modes =
    {
        ("Efficiency", new Guid("961cc777-2547-4f9d-8174-7d86181b8a7a")),
        ("Balanced", Guid.Empty),
        ("Performance", new Guid("ded574b5-45a0-4f42-8737-46345c09c238")),
    };

    [DllImport("powrprof.dll")] static extern uint PowerGetEffectiveOverlayScheme(out Guid id);
    [DllImport("powrprof.dll")] static extern uint PowerSetActiveOverlayScheme(Guid id);

    public static int Current()
    {
        try
        {
            if (PowerGetEffectiveOverlayScheme(out var g) != 0) return 1;
            int i = Array.FindIndex(Modes, m => m.Id == g);
            return i < 0 ? 1 : i;
        }
        catch { return 1; }
    }

    public static void Set(int index)
    {
        try { PowerSetActiveOverlayScheme(Modes[Math.Clamp(index, 0, Modes.Length - 1)].Id); } catch { }
    }
}
