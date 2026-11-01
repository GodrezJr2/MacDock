using System;
using System.Collections.Generic;
using static MacDock.Native;

namespace MacDock;

/// <summary>Hides the Windows taskbar while the dock runs and puts it back exactly as it was.</summary>
static class Taskbar
{
    static IntPtr originalState = IntPtr.Zero;
    static bool hidden;

    /// <summary>
    /// Explorer's own taskbar. Our tray host (ManagedShell, see <see cref="Tray"/>) also registers a
    /// Shell_TrayWnd, topmost, so a plain FindWindow can return ours instead.
    /// </summary>
    static IntPtr Main()
    {
        IntPtr h = IntPtr.Zero;
        uint self = (uint)Environment.ProcessId;
        while ((h = FindWindowExW(IntPtr.Zero, h, "Shell_TrayWnd", null)) != IntPtr.Zero)
        {
            GetWindowThreadProcessId(h, out uint pid);
            if (pid != self) return h;
        }
        return IntPtr.Zero;
    }

    static IEnumerable<IntPtr> Bars()
    {
        var main = Main();
        if (main != IntPtr.Zero) yield return main;
        IntPtr h = IntPtr.Zero;
        while ((h = FindWindowExW(IntPtr.Zero, h, "Shell_SecondaryTrayWnd", null)) != IntPtr.Zero) yield return h;
    }

    public static void Hide()
    {
        var main = Main();
        if (main == IntPtr.Zero) return;
        var d = new APPBARDATA { cbSize = System.Runtime.InteropServices.Marshal.SizeOf<APPBARDATA>(), hWnd = main };
        if (!hidden) originalState = (IntPtr)(long)SHAppBarMessage(ABM_GETSTATE, ref d);
        d.lParam = (IntPtr)1; // ABS_AUTOHIDE so the taskbar stops reserving space
        SHAppBarMessage(ABM_SETSTATE, ref d);
        foreach (var b in Bars()) ShowWindow(b, SW_HIDE);
        hidden = true;
    }

    /// <summary>Explorer re-shows the taskbar after restarts or display changes.</summary>
    public static void KeepHidden()
    {
        if (!hidden) return;
        foreach (var b in Bars())
            if (IsWindowVisible(b)) { Hide(); return; }
    }

    public static void Restore(bool force = false)
    {
        if (!hidden && !force) return;
        var main = Main();
        if (main == IntPtr.Zero) return;
        var d = new APPBARDATA { cbSize = System.Runtime.InteropServices.Marshal.SizeOf<APPBARDATA>(), hWnd = main, lParam = force && !hidden ? IntPtr.Zero : originalState };
        SHAppBarMessage(ABM_SETSTATE, ref d);
        foreach (var b in Bars()) ShowWindow(b, SW_SHOW);
        hidden = false;
    }
}

/// <summary>Registers a window as an AppBar so maximized windows stop at it, like the macOS menu bar and dock.</summary>
class AppBar
{
    readonly IntPtr hwnd;
    readonly uint edge;
    public readonly uint CallbackMessage;
    int thicknessPx;
    bool registered;

    public AppBar(IntPtr hwnd, uint edge)
    {
        this.hwnd = hwnd; this.edge = edge;
        CallbackMessage = RegisterWindowMessageW("MacDockAppBar");
    }

    APPBARDATA Data() => new() { cbSize = System.Runtime.InteropServices.Marshal.SizeOf<APPBARDATA>(), hWnd = hwnd, uCallbackMessage = CallbackMessage, uEdge = edge };

    public void Register(int thicknessPx, int screenW, int screenH)
    {
        this.thicknessPx = thicknessPx;
        var d = Data();
        if (!registered) { SHAppBarMessage(ABM_NEW, ref d); registered = true; }
        d.rc = new RECT { Left = 0, Right = screenW, Top = edge == ABE_TOP ? 0 : screenH - thicknessPx, Bottom = edge == ABE_TOP ? thicknessPx : screenH };
        SHAppBarMessage(ABM_QUERYPOS, ref d);
        if (edge == ABE_TOP) d.rc.Bottom = d.rc.Top + thicknessPx; else d.rc.Top = d.rc.Bottom - thicknessPx;
        SHAppBarMessage(ABM_SETPOS, ref d);
    }

    public void Reposition(int screenW, int screenH) { if (registered) Register(thicknessPx, screenW, screenH); }

    public void Remove()
    {
        if (!registered) return;
        var d = Data();
        SHAppBarMessage(ABM_REMOVE, ref d);
        registered = false;
    }
}
