using System;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
using System.Text.Json;

namespace MacDock;

/// <summary>
/// Where the laptop's power goes, as far as an unprivileged process can see it.
///
/// The battery driver only measures the battery side: on the battery its discharge rate is the whole system's
/// draw, but on the adapter (charging or full) nothing measures the adapter input. So on AC the total is
/// estimated as CPU package (Windows' energy meter, RAPL) + GPU board power (NVML) + "the rest" (screen, RAM,
/// SSD, fans, chipset, VRM losses). "The rest" is learned on the battery, where the total is measured:
/// rest = battery draw − CPU − GPU, smoothed, and saved for next time.
/// </summary>
sealed class PowerMeter : IDisposable
{
    public readonly record struct Sample(
        Battery.Reading Battery,
        double? CpuW, double? GpuW, bool GpuAsleep, double RestW, bool RestLearned,
        double TotalW, bool TotalMeasured, double DcInW, double BatteryFlowW);

    const double DefaultRestW = 14; // a 16" 165 Hz panel + board, before anything has been learned
    static readonly string StorePath = Path.Combine(
        Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "MacDock", "power.json");

    // Opened on the first Read, which runs off the UI thread: nvmlInit alone takes ~0.6 s.
    EnergyMeter? cpu;
    Nvml? gpu;
    bool opened;
    readonly object gate = new();

    // On an Optimus laptop the dGPU sleeps when idle and every NVML query wakes it (~0.5 s, and battery).
    // A slow query means it was asleep: report it as off and only look again now and then.
    const int GpuSlowMs = 150, GpuAwakeEveryMs = 1000, GpuAsleepEveryMs = 30000;
    long nextGpuAt;
    bool gpuAsleep;

    Sample? last;
    long lastAt;

    /// <summary>The latest sample if it is only a few seconds old, so a reopened flyout can show numbers at once
    /// without flashing an old charge level.</summary>
    public Sample? Recent => last is Sample s && Environment.TickCount64 - lastAt < 5000 ? s : null;
    double restW = DefaultRestW;
    bool restLearned;
    int learnedSamples;
    double? cpuAvg, gpuAvg;

    public PowerMeter()
    {
        try
        {
            if (File.Exists(StorePath))
            {
                var doc = JsonDocument.Parse(File.ReadAllText(StorePath)).RootElement;
                restW = doc.GetProperty("RestW").GetDouble();
                restLearned = true;
            }
        }
        catch { }
    }

    /// <summary>Blocking (battery IOCTLs, NVML): call it off the UI thread.</summary>
    public Sample Read()
    {
        lock (gate)
        {
            var s = ReadLocked();
            last = s;
            lastAt = Environment.TickCount64;
            return s;
        }
    }

    Sample ReadLocked()
    {
        if (!opened)
        {
            opened = true;
            cpu = EnergyMeter.Open();
            gpu = Nvml.Open();
        }
        var b = global::MacDock.Battery.Read();
        // the battery driver averages its rate over a few seconds, so the sensors are smoothed to the same pace
        cpuAvg = Smooth(cpuAvg, cpu?.ReadWatts());
        ReadGpu();

        double parts = (cpuAvg ?? 0) + (gpuAvg ?? 0);
        bool measured = b.Present && !b.OnAc && b.Discharging;
        if (measured && cpuAvg != null && b.DrawW is double draw)
        {
            double rest = Math.Clamp(draw - parts, 2, 60);
            learnedSamples++;
            // the first seconds after unplugging still carry AC-era averages; let them settle before trusting
            if (learnedSamples > 5)
            {
                restW = restLearned ? restW + (rest - restW) * 0.05 : rest;
                restLearned = true;
                if (learnedSamples % 30 == 0) Save();
            }
        }
        else learnedSamples = 0;

        double total = measured && b.DrawW is double d ? d : parts + restW;
        // battery flow is signed: + into the battery, − out of it; the adapter covers the system plus the charge
        double flow = b.Present ? b.Watts : 0;
        double dcIn = b.OnAc || !b.Present ? Math.Max(0, total + flow) : 0;
        return new Sample(b, cpuAvg, gpuAvg, gpuAsleep, restW, restLearned, total, measured, dcIn, flow);
    }

    void ReadGpu()
    {
        if (gpu == null) return;
        long now = Environment.TickCount64;
        if (now < nextGpuAt) return;
        var sw = System.Diagnostics.Stopwatch.StartNew();
        double? w = gpu.ReadWatts();
        bool slept = sw.ElapsedMilliseconds > GpuSlowMs;
        if (slept)
        {
            // the value read is the wake-up spike, not what the sleeping card draws (≈0)
            gpuAsleep = true;
            gpuAvg = 0;
        }
        else
        {
            gpuAsleep = false;
            gpuAvg = Smooth(gpuAvg, w);
        }
        nextGpuAt = Environment.TickCount64 + (slept ? GpuAsleepEveryMs : GpuAwakeEveryMs);
    }

    static double? Smooth(double? avg, double? now) =>
        now is not double n ? avg : avg is double a ? a + (n - a) * 0.35 : n;

    void Save()
    {
        try
        {
            Directory.CreateDirectory(Path.GetDirectoryName(StorePath)!);
            File.WriteAllText(StorePath, JsonSerializer.Serialize(new { RestW = Math.Round(restW, 2) }));
        }
        catch { }
    }

    public void Dispose()
    {
        lock (gate)
        {
            if (learnedSamples > 5) Save();
            cpu?.Dispose();
            cpu = null;
            gpu = null;
        }
    }

    /// <summary>
    /// Windows' Energy Meter Interface (EMI v2) on the CPU's PPM device: RAPL counters readable without admin.
    /// Counters are in picowatt-hours with a 100 ns timestamp.
    /// </summary>
    sealed class EnergyMeter : IDisposable
    {
        const uint IOCTL_EMI_GET_VERSION = 0x224000, IOCTL_EMI_GET_METADATA_SIZE = 0x224004,
            IOCTL_EMI_GET_METADATA = 0x224008, IOCTL_EMI_GET_MEASUREMENT = 0x22400C;
        static readonly Guid EmiGuid = new("45BD8344-7ED6-49cf-A440-C276C933B053");

        readonly IntPtr handle;
        readonly int channels, pkgIndex;
        ulong lastEnergy, lastTime;

        EnergyMeter(IntPtr h, int count, int pkg) { handle = h; channels = count; pkgIndex = pkg; }

        public static EnergyMeter? Open()
        {
            try
            {
                foreach (var path in DeviceInterfaces.Paths(EmiGuid))
                {
                    var h = DeviceInterfaces.CreateFileW(path, 0x80000000, 3, IntPtr.Zero, 3, 0, IntPtr.Zero);
                    if (h == new IntPtr(-1)) continue;
                    var buf = new byte[4];
                    if (!Ioctl(h, IOCTL_EMI_GET_VERSION, buf) || BitConverter.ToUInt16(buf, 0) < 2
                        || !Ioctl(h, IOCTL_EMI_GET_METADATA_SIZE, buf))
                    { DeviceInterfaces.CloseHandle(h); continue; }
                    var md = new byte[BitConverter.ToInt32(buf, 0)];
                    if (md.Length < 68 || !Ioctl(h, IOCTL_EMI_GET_METADATA, md)) { DeviceInterfaces.CloseHandle(h); continue; }
                    // EMI_METADATA_V2: OEM[16 wchar], Model[16 wchar], Revision u16, ChannelCount u16, then
                    // EMI_CHANNEL_V2 { Unit u32, NameSize u16, Name[] }
                    int count = BitConverter.ToUInt16(md, 66), off = 68, pkg = -1;
                    for (int c = 0; c < count && off + 6 <= md.Length; c++)
                    {
                        int size = BitConverter.ToUInt16(md, off + 4);
                        var name = Encoding.Unicode.GetString(md, off + 6, Math.Min(size, md.Length - off - 6)).TrimEnd('\0');
                        if (pkg < 0 && name.EndsWith("_PKG", StringComparison.OrdinalIgnoreCase)) pkg = c;
                        off += 6 + size;
                    }
                    if (pkg >= 0) return new EnergyMeter(h, count, pkg);
                    DeviceInterfaces.CloseHandle(h);
                }
            }
            catch { }
            return null;
        }

        public double? ReadWatts()
        {
            var m = new byte[16 * channels];
            if (!Ioctl(handle, IOCTL_EMI_GET_MEASUREMENT, m)) return null;
            ulong e = BitConverter.ToUInt64(m, pkgIndex * 16), t = BitConverter.ToUInt64(m, pkgIndex * 16 + 8);
            ulong pe = lastEnergy, pt = lastTime;
            lastEnergy = e; lastTime = t;
            if (pt == 0 || t <= pt || e < pe) return null;
            return (e - pe) * 3600.0 / 1e12 / ((t - pt) / 1e7);
        }

        static bool Ioctl(IntPtr h, uint code, byte[] output) =>
            DeviceInterfaces.DeviceIoControl(h, code, IntPtr.Zero, 0, output, output.Length, out _, IntPtr.Zero);

        public void Dispose() => DeviceInterfaces.CloseHandle(handle);
    }

    /// <summary>NVIDIA's management library (ships with the driver): board power of GPU 0.</summary>
    sealed class Nvml
    {
        [DllImport("nvml.dll")] static extern int nvmlInit_v2();
        [DllImport("nvml.dll")] static extern int nvmlDeviceGetHandleByIndex_v2(uint index, out IntPtr device);
        [DllImport("nvml.dll")] static extern int nvmlDeviceGetPowerUsage(IntPtr device, out uint milliwatts);

        readonly IntPtr device;
        Nvml(IntPtr d) => device = d;

        public static Nvml? Open()
        {
            try
            {
                if (nvmlInit_v2() != 0 || nvmlDeviceGetHandleByIndex_v2(0, out var d) != 0) return null;
                return new Nvml(d);
            }
            catch { return null; } // no NVIDIA driver
        }

        public double? ReadWatts() => nvmlDeviceGetPowerUsage(device, out uint mw) == 0 ? mw / 1000.0 : null;
    }
}

static class DeviceInterfaces
{
    [StructLayout(LayoutKind.Sequential)]
    struct SP_DEVICE_INTERFACE_DATA { public int cbSize; public Guid ClassGuid; public int Flags; public IntPtr Reserved; }

    [DllImport("setupapi.dll", CharSet = CharSet.Unicode)]
    static extern IntPtr SetupDiGetClassDevsW(ref Guid guid, IntPtr enumerator, IntPtr hwnd, uint flags);
    [DllImport("setupapi.dll", CharSet = CharSet.Unicode)]
    static extern bool SetupDiEnumDeviceInterfaces(IntPtr set, IntPtr devInfo, ref Guid guid, uint index, ref SP_DEVICE_INTERFACE_DATA data);
    [DllImport("setupapi.dll", CharSet = CharSet.Unicode)]
    static extern bool SetupDiGetDeviceInterfaceDetailW(IntPtr set, ref SP_DEVICE_INTERFACE_DATA data, IntPtr detail, int size, out int needed, IntPtr devInfo);
    [DllImport("setupapi.dll")] static extern bool SetupDiDestroyDeviceInfoList(IntPtr set);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    public static extern IntPtr CreateFileW(string path, uint access, uint share, IntPtr security, uint disposition, uint flags, IntPtr template);
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool DeviceIoControl(IntPtr h, uint code, IntPtr input, int inSize, byte[] output, int outSize, out int bytes, IntPtr overlapped);
    [DllImport("kernel32.dll")] public static extern bool CloseHandle(IntPtr h);

    public static System.Collections.Generic.List<string> Paths(Guid guid)
    {
        var list = new System.Collections.Generic.List<string>();
        var set = SetupDiGetClassDevsW(ref guid, IntPtr.Zero, IntPtr.Zero, 0x12); // PRESENT | DEVICEINTERFACE
        if (set == new IntPtr(-1)) return list;
        try
        {
            for (uint i = 0; ; i++)
            {
                var did = new SP_DEVICE_INTERFACE_DATA { cbSize = Marshal.SizeOf<SP_DEVICE_INTERFACE_DATA>() };
                if (!SetupDiEnumDeviceInterfaces(set, IntPtr.Zero, ref guid, i, ref did)) break;
                SetupDiGetDeviceInterfaceDetailW(set, ref did, IntPtr.Zero, 0, out int needed, IntPtr.Zero);
                IntPtr buffer = Marshal.AllocHGlobal(needed);
                try
                {
                    Marshal.WriteInt32(buffer, IntPtr.Size == 8 ? 8 : 6);
                    if (SetupDiGetDeviceInterfaceDetailW(set, ref did, buffer, needed, out _, IntPtr.Zero))
                        list.Add(Marshal.PtrToStringUni(buffer + 4)!);
                }
                finally { Marshal.FreeHGlobal(buffer); }
            }
        }
        finally { SetupDiDestroyDeviceInfoList(set); }
        return list;
    }
}
