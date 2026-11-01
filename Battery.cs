using System;
using System.Runtime.InteropServices;

namespace MacDock;

/// <summary>
/// Battery state read straight from the battery class driver — the same IOCTLs Windows' own battery UI
/// uses. Laptop embedded controllers normally report power only on the battery side, so <see cref="Reading.Watts"/>
/// is signed: positive while the battery charges, negative while it discharges. On AC with a full or
/// charge-limited battery the rate is 0 and no adapter-side wattage exists to read.
/// </summary>
static class Battery
{
    public readonly record struct Reading(
        bool Present, int Percent, bool OnAc, bool Charging, bool Discharging,
        double Watts, int CapacityMwh, int FullMwh, int DesignMwh, int Cycles, int VoltageMv, int SecondsLeft)
    {
        public bool Full => OnAc && !Charging && !Discharging && Percent >= 95;
        /// <summary>Measured system draw while running on the battery, null on AC (the EC does not report it).</summary>
        public double? DrawW => Discharging ? -Watts : null;
        public int WearPercent => FullMwh > 0 && DesignMwh > 0
            ? (int)Math.Round(100.0 * (DesignMwh - FullMwh) / DesignMwh)
            : -1;
        public double CapacityWh => CapacityMwh / 1000.0;
        public double FullWh => FullMwh / 1000.0;
    }

    const uint DIGCF_PRESENT = 0x2, DIGCF_DEVICEINTERFACE = 0x10;
    const uint GENERIC_READ = 0x80000000, GENERIC_WRITE = 0x40000000;
    const uint FILE_SHARE_READ = 1, FILE_SHARE_WRITE = 2, OPEN_EXISTING = 3;
    const uint IOCTL_QUERY_TAG = 0x294040, IOCTL_QUERY_INFO = 0x294044, IOCTL_QUERY_STATUS = 0x29404C;

    // BATTERY_STATUS.PowerState
    const uint POWER_ON_LINE = 0x1, DISCHARGING = 0x2, CHARGING = 0x4;

    [StructLayout(LayoutKind.Sequential)]
    struct SP_DEVICE_INTERFACE_DATA { public int cbSize; public Guid ClassGuid; public int Flags; public IntPtr Reserved; }

    [StructLayout(LayoutKind.Sequential)] struct QueryInfo { public uint Tag, Level; public int AtRate; }
    [StructLayout(LayoutKind.Sequential)] struct WaitStatus { public uint Tag, Timeout, PowerState, LowCapacity, HighCapacity; }
    [StructLayout(LayoutKind.Sequential)] struct BatteryStatus { public uint PowerState, Capacity, Voltage; public int Rate; }

    [StructLayout(LayoutKind.Sequential)]
    struct BatteryInformation
    {
        public uint Capabilities;
        public byte Technology, R1, R2, R3, C1, C2, C3, C4;
        public uint DesignedCapacity, FullChargedCapacity, DefaultAlert1, DefaultAlert2, CriticalBias, CycleCount;
    }

    [DllImport("setupapi.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern IntPtr SetupDiGetClassDevsW(ref Guid guid, IntPtr enumerator, IntPtr hwnd, uint flags);
    [DllImport("setupapi.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool SetupDiEnumDeviceInterfaces(IntPtr set, IntPtr devInfo, ref Guid guid, uint index, ref SP_DEVICE_INTERFACE_DATA data);
    [DllImport("setupapi.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool SetupDiGetDeviceInterfaceDetailW(IntPtr set, ref SP_DEVICE_INTERFACE_DATA data, IntPtr detail, int size, out int needed, IntPtr devInfo);
    [DllImport("setupapi.dll")] static extern bool SetupDiDestroyDeviceInfoList(IntPtr set);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern IntPtr CreateFileW(string path, uint access, uint share, IntPtr security, uint disposition, uint flags, IntPtr template);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool DeviceIoControl(IntPtr h, uint code, ref uint input, int inSize, out uint output, int outSize, out int bytes, IntPtr overlapped);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool DeviceIoControl(IntPtr h, uint code, ref QueryInfo input, int inSize, IntPtr output, int outSize, out int bytes, IntPtr overlapped);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool DeviceIoControl(IntPtr h, uint code, ref WaitStatus input, int inSize, IntPtr output, int outSize, out int bytes, IntPtr overlapped);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr h);

    static readonly Guid BatteryClassGuid = new("72631e54-78a4-11d0-bcf7-00aa00b7b32a");
    static string? devicePath;

    public static Reading Read()
    {
        Native.GetSystemPowerStatus(out var ps);
        bool onboard = ps.BatteryFlag != 128 && ps.BatteryFlag != 255;
        if (!onboard)
            return new Reading(false, 100, true, false, false, 0, 0, 0, 0, 0, 0, -1);

        int percent = Math.Clamp((int)ps.BatteryLifePercent, 0, 100);
        bool onAc = ps.ACLineStatus == 1;

        uint state = 0;
        int rateMw = 0, capacityMwh = 0, voltageMv = 0, fullMwh = 0, designMwh = 0, cycles = 0;
        bool device = TryReadDevice(ref state, ref rateMw, ref capacityMwh, ref voltageMv, ref fullMwh, ref designMwh, ref cycles);

        bool charging = (state & CHARGING) != 0;
        bool discharging = (state & DISCHARGING) != 0;
        if (!charging && !discharging && rateMw != 0)
        {
            charging = onAc;
            discharging = !onAc;
        }
        // the EC signs the rate inconsistently; the state flags are the reliable part
        double watts = rateMw / 1000.0;
        watts = charging ? Math.Abs(watts) : discharging ? -Math.Abs(watts) : 0;

        if (!device)
        {
            // no IOCTL support: GetSystemPowerStatus still knows the percentage and whether AC is in
            fullMwh = designMwh = capacityMwh = 0;
        }
        else if (fullMwh > 0 && percent > 0)
        {
            capacityMwh = (int)Math.Round(fullMwh * percent / 100.0);
        }

        int secondsLeft = ps.BatteryLifeTime > 0 ? ps.BatteryLifeTime : -1;
        return new Reading(true, percent, onAc, charging, discharging, watts,
            capacityMwh, fullMwh, designMwh, cycles, voltageMv, secondsLeft);
    }

    static bool TryReadDevice(ref uint state, ref int rateMw, ref int capacityMwh, ref int voltageMv,
        ref int fullMwh, ref int designMwh, ref int cycles)
    {
        if (devicePath == null)
        {
            devicePath = FindDevicePath();
            if (devicePath == null) return false;
        }

        var h = CreateFileW(devicePath, GENERIC_READ | GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE,
            IntPtr.Zero, OPEN_EXISTING, 0, IntPtr.Zero);
        if (h == new IntPtr(-1))
        {
            devicePath = null; // device went away (undocked, driver reload); look again next time
            return false;
        }
        try
        {
            uint timeout = 0;
            if (!DeviceIoControl(h, IOCTL_QUERY_TAG, ref timeout, 4, out uint tag, 4, out _, IntPtr.Zero))
                return false;

            var qi = new QueryInfo { Tag = tag, Level = 0 };
            int infoSize = Marshal.SizeOf<BatteryInformation>();
            IntPtr info = Marshal.AllocHGlobal(infoSize);
            try
            {
                if (!DeviceIoControl(h, IOCTL_QUERY_INFO, ref qi, Marshal.SizeOf<QueryInfo>(), info, infoSize, out _, IntPtr.Zero))
                    return false;
                var bi = Marshal.PtrToStructure<BatteryInformation>(info);
                designMwh = (int)bi.DesignedCapacity;
                fullMwh = (int)bi.FullChargedCapacity;
                cycles = (int)bi.CycleCount;
            }
            finally { Marshal.FreeHGlobal(info); }

            var ws = new WaitStatus { Tag = tag };
            int statusSize = Marshal.SizeOf<BatteryStatus>();
            IntPtr status = Marshal.AllocHGlobal(statusSize);
            try
            {
                if (!DeviceIoControl(h, IOCTL_QUERY_STATUS, ref ws, Marshal.SizeOf<WaitStatus>(), status, statusSize, out _, IntPtr.Zero))
                    return false;
                var bs = Marshal.PtrToStructure<BatteryStatus>(status);
                state = bs.PowerState;
                capacityMwh = (int)bs.Capacity;
                voltageMv = (int)bs.Voltage;
                rateMw = bs.Rate;
            }
            finally { Marshal.FreeHGlobal(status); }
            return true;
        }
        finally { CloseHandle(h); }
    }

    static string? FindDevicePath()
    {
        var guid = BatteryClassGuid;
        var set = SetupDiGetClassDevsW(ref guid, IntPtr.Zero, IntPtr.Zero, DIGCF_PRESENT | DIGCF_DEVICEINTERFACE);
        if (set == new IntPtr(-1)) return null;
        try
        {
            var did = new SP_DEVICE_INTERFACE_DATA { cbSize = Marshal.SizeOf<SP_DEVICE_INTERFACE_DATA>() };
            if (!SetupDiEnumDeviceInterfaces(set, IntPtr.Zero, ref guid, 0, ref did)) return null;
            SetupDiGetDeviceInterfaceDetailW(set, ref did, IntPtr.Zero, 0, out int needed, IntPtr.Zero);
            IntPtr buffer = Marshal.AllocHGlobal(needed);
            try
            {
                Marshal.WriteInt32(buffer, 0, IntPtr.Size == 8 ? 8 : 6);
                return SetupDiGetDeviceInterfaceDetailW(set, ref did, buffer, needed, out _, IntPtr.Zero)
                    ? Marshal.PtrToStringUni(buffer + 4)
                    : null;
            }
            finally { Marshal.FreeHGlobal(buffer); }
        }
        finally { SetupDiDestroyDeviceInfoList(set); }
    }
}
