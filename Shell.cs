using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
using System.Windows.Media;
using System.Windows.Media.Imaging;
using static MacDock.Native;

namespace MacDock;

/// <summary>A top-level app window as the dock sees it.</summary>
record AppWindow(IntPtr Hwnd, uint Pid, string? Exe, string? Aumid, string Title);

static class Shell
{
    // ---------- COM interop
    [ComImport, Guid("43826d1e-e718-42ee-bc55-a1e261c37bfe"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IShellItem
    {
        void BindToHandler(IntPtr pbc, ref Guid bhid, ref Guid riid, out IntPtr ppv);
        void GetParent(out IShellItem ppsi);
        void GetDisplayName(uint sigdn, out IntPtr ppszName);
        void GetAttributes(uint mask, out uint attribs);
        void Compare(IShellItem psi, uint hint, out int order);
    }

    [ComImport, Guid("bcc18b79-ba16-442f-80c4-8a59c30c463b"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IShellItemImageFactory
    {
        [PreserveSig] int GetImage(SIZE size, int flags, out IntPtr phbm);
    }

    [ComImport, Guid("886D8EEB-8CF2-4446-8D02-CDBA1DBDCF99"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IPropertyStore
    {
        [PreserveSig] int GetCount(out uint c);
        [PreserveSig] int GetAt(uint i, out PROPERTYKEY k);
        [PreserveSig] int GetValue(ref PROPERTYKEY key, out PROPVARIANT pv);
        [PreserveSig] int SetValue(ref PROPERTYKEY key, ref PROPVARIANT pv);
        [PreserveSig] int Commit();
    }

    [StructLayout(LayoutKind.Sequential)] struct PROPERTYKEY { public Guid fmtid; public uint pid; }
    [StructLayout(LayoutKind.Explicit, Size = 24)] struct PROPVARIANT { [FieldOffset(0)] public ushort vt; [FieldOffset(8)] public IntPtr p; }

    [DllImport("shell32.dll", CharSet = CharSet.Unicode)]
    static extern int SHCreateItemFromParsingName(string path, IntPtr pbc, ref Guid riid, [MarshalAs(UnmanagedType.Interface)] out IShellItem item);
    [DllImport("shell32.dll")]
    static extern int SHGetPropertyStoreForWindow(IntPtr hwnd, ref Guid riid, [MarshalAs(UnmanagedType.Interface)] out IPropertyStore ps);
    [DllImport("shell32.dll", CharSet = CharSet.Unicode)]
    static extern int SHGetPropertyStoreFromParsingName(string path, IntPtr pbc, int flags, ref Guid riid, [MarshalAs(UnmanagedType.Interface)] out IPropertyStore ps);
    [DllImport("ole32.dll")] static extern int PropVariantClear(ref PROPVARIANT pv);
    [DllImport("ole32.dll")] static extern void CoTaskMemFree(IntPtr p);
    [DllImport("gdi32.dll")] static extern bool DeleteObject(IntPtr h);
    [DllImport("gdi32.dll")] static extern int GetObjectW(IntPtr h, int size, out BITMAP bmp);
    [StructLayout(LayoutKind.Sequential)] struct BITMAP { public int bmType, bmWidth, bmHeight, bmWidthBytes; public ushort bmPlanes, bmBitsPixel; public IntPtr bmBits; }

    static Guid IID_IShellItem = new("43826d1e-e718-42ee-bc55-a1e261c37bfe");
    static Guid IID_IPropertyStore = new("886D8EEB-8CF2-4446-8D02-CDBA1DBDCF99");
    static PROPERTYKEY PKEY_AppUserModel_ID = new() { fmtid = new Guid("9F4C2855-9F79-4B39-A8D0-E1D42DE1D5F3"), pid = 5 };
    static PROPERTYKEY PKEY_Link_TargetParsingPath = new() { fmtid = new Guid("B9B4B3FC-2B51-4A42-B5D8-324146AFCF25"), pid = 2 };

    public const string TrashPath = "shell:RecycleBinFolder";

    static string? ReadString(IPropertyStore ps, PROPERTYKEY key)
    {
        if (ps.GetValue(ref key, out var pv) != 0) return null;
        try { return pv.vt == 31 && pv.p != IntPtr.Zero ? Marshal.PtrToStringUni(pv.p) : null; }
        finally { PropVariantClear(ref pv); }
    }

    static string? StoreString(string parsingName, PROPERTYKEY key)
    {
        if (SHGetPropertyStoreFromParsingName(parsingName, IntPtr.Zero, 0, ref IID_IPropertyStore, out var ps) != 0) return null;
        try { return ReadString(ps, key); }
        finally { Marshal.ReleaseComObject(ps); }
    }

    public static string? LinkTarget(string parsingName) => StoreString(parsingName, PKEY_Link_TargetParsingPath);
    public static string? ItemAumid(string parsingName) => StoreString(parsingName, PKEY_AppUserModel_ID);

    public static string? WindowAumid(IntPtr hwnd)
    {
        if (SHGetPropertyStoreForWindow(hwnd, ref IID_IPropertyStore, out var ps) != 0) return null;
        try { return ReadString(ps, PKEY_AppUserModel_ID); }
        finally { Marshal.ReleaseComObject(ps); }
    }

    public static string? DisplayName(string parsingName)
    {
        if (SHCreateItemFromParsingName(parsingName, IntPtr.Zero, ref IID_IShellItem, out var item) != 0) return null;
        try
        {
            item.GetDisplayName(0, out var p);
            var s = Marshal.PtrToStringUni(p);
            CoTaskMemFree(p);
            return s;
        }
        catch { return null; }
        finally { Marshal.ReleaseComObject(item); }
    }

    static readonly Dictionary<string, ImageSource?> iconCache = new(StringComparer.OrdinalIgnoreCase);

    public static ImageSource? Icon(string parsingName, int size = 256, bool cache = true)
    {
        if (cache && iconCache.TryGetValue(parsingName, out var hit)) return hit;
        ImageSource? img = null;
        if (SHCreateItemFromParsingName(parsingName, IntPtr.Zero, ref IID_IShellItem, out var item) == 0)
        {
            try
            {
                var f = (IShellItemImageFactory)item;
                // SIIGBF_ICONONLY (4) | SIIGBF_BIGGERSIZEOK (1)
                if (f.GetImage(new SIZE { cx = size, cy = size }, 0x4 | 0x1, out var hbm) == 0 && hbm != IntPtr.Zero)
                {
                    img = FromHBitmap(hbm);
                    DeleteObject(hbm);
                }
            }
            catch { }
            finally { Marshal.ReleaseComObject(item); }
        }
        if (cache) iconCache[parsingName] = img;
        return img;
    }

    /// <summary>The image factory hands back a premultiplied, bottom-up 32bpp DIB.</summary>
    static unsafe BitmapSource? FromHBitmap(IntPtr hbm)
    {
        if (GetObjectW(hbm, Marshal.SizeOf<BITMAP>(), out var bm) == 0 || bm.bmBits == IntPtr.Zero || bm.bmBitsPixel != 32) return null;
        int w = bm.bmWidth, h = Math.Abs(bm.bmHeight), stride = bm.bmWidthBytes;
        var buf = new byte[stride * h];
        byte* src = (byte*)bm.bmBits;
        bool bottomUp = bm.bmHeight > 0;
        for (int y = 0; y < h; y++)
            Marshal.Copy((IntPtr)(src + (bottomUp ? h - 1 - y : y) * stride), buf, y * stride, stride);
        var bs = BitmapSource.Create(w, h, 96, 96, PixelFormats.Pbgra32, null, buf, stride);
        bs.Freeze();
        return bs;
    }

    // ---------- processes & windows
    static readonly Dictionary<uint, (string? exe, string? aumid)> procCache = new();

    public static (string? exe, string? aumid) ProcessInfo(uint pid)
    {
        if (procCache.TryGetValue(pid, out var c)) return c;
        string? exe = null, aumid = null;
        var h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid);
        if (h != IntPtr.Zero)
        {
            var sb = new StringBuilder(1024); int n = sb.Capacity;
            if (QueryFullProcessImageNameW(h, 0, sb, ref n)) exe = sb.ToString();
            uint len = 512; var ab = new StringBuilder((int)len);
            if (GetApplicationUserModelId(h, ref len, ab) == 0) aumid = ab.ToString();
            CloseHandle(h);
        }
        return procCache[pid] = (exe, aumid);
    }

    static readonly HashSet<string> skipClasses = new() { "Progman", "WorkerW", "Shell_TrayWnd", "Shell_SecondaryTrayWnd", "Windows.UI.Core.CoreWindow" };

    static readonly Dictionary<IntPtr, string?> windowAumidCache = new();

    public static List<AppWindow> AppWindows()
    {
        if (windowAumidCache.Count > 400) windowAumidCache.Clear();
        var list = new List<AppWindow>();
        uint self = (uint)Environment.ProcessId;
        var cls = new StringBuilder(256);
        EnumWindows((h, _) =>
        {
            if (!IsWindowVisible(h)) return true;
            long ex = (long)GetWindowLongPtr(h, GWL_EXSTYLE);
            bool appWin = (ex & WS_EX_APPWINDOW) != 0;
            if (!appWin && ((ex & WS_EX_TOOLWINDOW) != 0 || GetWindow(h, GW_OWNER) != IntPtr.Zero)) return true;
            if (DwmGetWindowAttribute(h, DWMWA_CLOAKED, out int cloaked, 4) == 0 && cloaked != 0) return true;
            int tl = GetWindowTextLengthW(h);
            if (tl == 0) return true;
            cls.Clear(); GetClassNameW(h, cls, cls.Capacity);
            if (skipClasses.Contains(cls.ToString())) return true;
            GetWindowThreadProcessId(h, out uint pid);
            if (pid == self) return true;

            var title = new StringBuilder(tl + 1); GetWindowTextW(h, title, title.Capacity);
            var (exe, paumid) = ProcessInfo(pid);
            if (!windowAumidCache.TryGetValue(h, out var waumid))
            {
                waumid = WindowAumid(h);
                // UWP frames get their AUMID a moment after they appear, so don't remember a miss for them
                if (waumid != null || cls.ToString() != "ApplicationFrameWindow") windowAumidCache[h] = waumid;
            }
            string? aumid = waumid ?? paumid;
            if (cls.ToString() == "ApplicationFrameWindow")
            {
                // UWP frame: the real app lives in a child CoreWindow from another process.
                uint inner = 0;
                EnumChildWindows(h, (c, _) => { GetWindowThreadProcessId(c, out uint cp); if (cp != pid) { inner = cp; return false; } return true; }, IntPtr.Zero);
                if (inner != 0) { var ip = ProcessInfo(inner); exe = ip.exe; aumid ??= ip.aumid; }
                if (aumid == null) return true;
            }
            list.Add(new AppWindow(h, pid, exe, aumid, title.ToString()));
            return true;
        }, IntPtr.Zero);
        return list;
    }

    public static string FriendlyName(string? exe, string? aumid, string fallback)
    {
        if (aumid != null) { var n = DisplayName(@"shell:AppsFolder\" + aumid); if (!string.IsNullOrWhiteSpace(n)) return n; }
        if (exe != null)
        {
            try { var d = FileVersionInfo.GetVersionInfo(exe).FileDescription; if (!string.IsNullOrWhiteSpace(d)) return d.Trim(); } catch { }
            return Path.GetFileNameWithoutExtension(exe);
        }
        return fallback;
    }

    public static void Activate(IntPtr h)
    {
        if (IsIconic(h)) ShowWindow(h, SW_RESTORE);
        if (SetForegroundWindow(h)) { BringWindowToTop(h); return; }
        // The click on the dock gave us the last input, but after the genie animation the mouse may have moved
        // over another app and taken that right away. An empty mouse input makes us the last input again.
        SendInput(1, new[] { new INPUT() }, Marshal.SizeOf<INPUT>());
        if (SetForegroundWindow(h)) { BringWindowToTop(h); return; }
        // fall back to borrowing the foreground thread
        uint fgThread = GetWindowThreadProcessId(GetForegroundWindow(), out _);
        uint me = GetCurrentThreadId();
        AttachThreadInput(me, fgThread, true);
        SetForegroundWindow(h);
        BringWindowToTop(h);
        AttachThreadInput(me, fgThread, false);
        // a window restored from minimized sits at the bottom of the z-order; at least lift it above the rest
        if (GetForegroundWindow() != h)
        {
            SetWindowPos(h, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            SetWindowPos(h, HWND_NOTOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }

    public static void Minimize(IntPtr h) => PostMessageW(h, WM_SYSCOMMAND, (IntPtr)SC_MINIMIZE, IntPtr.Zero);
    public static void Close(IntPtr h) => PostMessageW(h, WM_CLOSE, IntPtr.Zero, IntPtr.Zero);

    public static void Launch(string target)
    {
        try
        {
            if (target.StartsWith("shell:", StringComparison.OrdinalIgnoreCase))
                Process.Start(new ProcessStartInfo("explorer.exe", target) { UseShellExecute = true });
            else
                Process.Start(new ProcessStartInfo(target) { UseShellExecute = true });
        }
        catch (Exception e) { Debug.WriteLine(e); }
    }
}
