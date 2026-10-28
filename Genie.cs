using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Linq;
using System.Runtime.InteropServices;
using System.Windows;
using System.Windows.Interop;
using System.Windows.Media;
using System.Windows.Media.Imaging;
using System.Windows.Media.Media3D;
using System.Windows.Threading;
using static MacDock.Native;
using Controls = System.Windows.Controls;

namespace MacDock;

/// <summary>Transparent overlay that draws one window snapshot as a strip mesh and bends it into the dock.</summary>
class GenieWindow : Window
{
    const int Rows = 48;
    readonly MeshGeometry3D mesh = new();
    readonly ImageBrush brush = new() { ViewportUnits = BrushMappingMode.Absolute, Viewport = new Rect(0, 0, 1, 1) };
    readonly OrthographicCamera cam = new() { LookDirection = new Vector3D(0, 0, -1), UpDirection = new Vector3D(0, 1, 0) };
    readonly Stopwatch clock = new();

    Rect win, icon;           // overlay-local DIPs
    double duration;
    bool restore;
    Action? done;
    public bool Busy { get; private set; }

    [StructLayout(LayoutKind.Sequential)] struct MARGINS { public int l, r, t, b; }
    [DllImport("dwmapi.dll")] static extern int DwmExtendFrameIntoClientArea(IntPtr h, ref MARGINS m);
    [DllImport("dwmapi.dll")] static extern int DwmSetWindowAttribute(IntPtr h, int attr, ref int v, int size);

    // Not a layered window: those are read back from the GPU every frame, which made the genie stutter.
    // A normal full-screen window with its frame extended over the client lets DWM compose our alpha directly.
    public GenieWindow()
    {
        WindowStyle = WindowStyle.None;
        AllowsTransparency = false;
        Background = Brushes.Transparent;
        Topmost = true;
        ShowInTaskbar = false;
        ShowActivated = false;
        ResizeMode = ResizeMode.NoResize;
        IsHitTestVisible = false;
        Left = 0; Top = 0;
        // one pixel short of the screen, or Explorer reports a fullscreen app and the dock hides itself
        Width = SystemParameters.PrimaryScreenWidth; Height = SystemParameters.PrimaryScreenHeight - 1;

        var tex = new PointCollection();
        var idx = new Int32Collection();
        for (int i = 0; i <= Rows; i++)
        {
            double v = (double)i / Rows;
            tex.Add(new Point(0, v)); tex.Add(new Point(1, v));
            if (i < Rows)
            {
                int a = i * 2;
                idx.Add(a); idx.Add(a + 2); idx.Add(a + 1);
                idx.Add(a + 1); idx.Add(a + 2); idx.Add(a + 3);
            }
        }
        mesh.TextureCoordinates = tex;
        mesh.TriangleIndices = idx;
        RenderOptions.SetBitmapScalingMode(brush, BitmapScalingMode.Linear);
        var mat = new DiffuseMaterial(brush);
        var group = new Model3DGroup();
        group.Children.Add(new AmbientLight(Colors.White));
        group.Children.Add(new GeometryModel3D(mesh, mat) { BackMaterial = mat });
        var vp = new Controls.Viewport3D { Camera = cam, IsHitTestVisible = false };
        vp.Children.Add(new ModelVisual3D { Content = group });
        Content = vp;

        SourceInitialized += (_, _) =>
        {
            var h = new WindowInteropHelper(this).Handle;
            var src = HwndSource.FromHwnd(h);
            src.CompositionTarget.BackgroundColor = Colors.Transparent;
            var m = new MARGINS { l = -1, r = -1, t = -1, b = -1 };
            DwmExtendFrameIntoClientArea(h, ref m);
            int none = 1, noRound = 1, noBorder = unchecked((int)0xFFFFFFFE);
            DwmSetWindowAttribute(h, 38, ref none, 4);     // DWMWA_SYSTEMBACKDROP_TYPE = none
            DwmSetWindowAttribute(h, 33, ref noRound, 4);  // no rounded corners
            DwmSetWindowAttribute(h, 34, ref noBorder, 4); // no border
            MakeToolWindow(h, clickThrough: true);
            // let clicks fall through while an animation is on screen
            src.AddHook((IntPtr hw, int msg, IntPtr w, IntPtr l, ref bool handled) =>
            {
                if (msg == 0x84) { handled = true; return (IntPtr)(-1); } // WM_NCHITTEST → HTTRANSPARENT
                return IntPtr.Zero;
            });
        };
    }

    public bool Play(ImageSource img, Rect windowDip, Rect iconDip, bool restore, Action? done)
    {
        if (Busy) return false;
        // fixed full-screen overlay: resizing the swap chain per animation risks a stale first frame
        win = windowDip;
        icon = iconDip;
        cam.Position = new Point3D(Width / 2, -Height / 2, 10);
        cam.Width = Width;
        brush.ImageSource = img;

        this.restore = restore;
        this.done = done;
        duration = restore ? 0.5 : 0.56;
        Busy = true;
        Shape(restore ? 1 : 0);
        Visibility = Visibility.Visible;
        clock.Restart();
        CompositionTarget.Rendering += OnFrame;
        return true;
    }

    void OnFrame(object? s, EventArgs e)
    {
        double t = Math.Min(1, clock.Elapsed.TotalSeconds / duration);
        Shape(restore ? 1 - t : t);
        if (t < 1) return;

        CompositionTarget.Rendering -= OnFrame;
        done?.Invoke();
        // keep the last frame up a moment so the real window has painted, then render an empty frame
        // before hiding so the next Show doesn't flash this one
        After(restore ? 90 : 0, () =>
        {
            mesh.Positions = new Point3DCollection();
            brush.ImageSource = null;
            After(40, () => { Visibility = Visibility.Hidden; Busy = false; });
        });
    }

    static void After(int ms, Action a)
    {
        if (ms <= 0) { a(); return; }
        var t = new DispatcherTimer { Interval = TimeSpan.FromMilliseconds(ms) };
        t.Tick += (_, _) => { t.Stop(); a(); };
        t.Start();
    }

    static double Smooth(double x) { x = Math.Clamp(x, 0, 1); return x * x * (3 - 2 * x); }

    /// <summary>
    /// Genie: first the sides bend into a funnel towards the icon (squeeze), then every row slides down and
    /// piles up at the icon's bottom edge (slide). p = 0 is the window, p = 1 is fully inside the icon.
    /// </summary>
    void Shape(double p)
    {
        double squeeze = Smooth(p / 0.45);
        double slide = Smooth((p - 0.22) / 0.78);
        double travel = icon.Bottom - win.Top;
        double funnelSpan = Math.Max(1, icon.Top - win.Top);

        var pts = new Point3DCollection((Rows + 1) * 2);
        for (int i = 0; i <= Rows; i++)
        {
            double v = (double)i / Rows;
            double y = Math.Min(win.Top + win.Height * v + travel * slide, icon.Bottom);
            double f = Smooth((y - win.Top) / funnelSpan);
            double l = win.Left + (icon.Left - win.Left) * f * squeeze;
            double r = win.Right + (icon.Right - win.Right) * f * squeeze;
            pts.Add(new Point3D(l, -y, 0));
            pts.Add(new Point3D(r, -y, 0));
        }
        mesh.Positions = pts;
    }
}

/// <summary>Keeps recent snapshots of app windows and plays the genie when they minimize or come back.</summary>
class Genie
{
    delegate void WinEventProc(IntPtr hook, uint ev, IntPtr hwnd, int obj, int child, uint thread, uint time);
    [DllImport("user32.dll")] static extern IntPtr SetWinEventHook(uint min, uint max, IntPtr mod, WinEventProc proc, uint pid, uint tid, uint flags);
    [DllImport("user32.dll")] static extern bool UnhookWinEvent(IntPtr h);
    [DllImport("user32.dll")] static extern IntPtr GetDC(IntPtr h);
    [DllImport("user32.dll")] static extern int ReleaseDC(IntPtr h, IntPtr dc);
    [DllImport("gdi32.dll")] static extern IntPtr CreateCompatibleDC(IntPtr dc);
    [DllImport("gdi32.dll")] static extern IntPtr CreateCompatibleBitmap(IntPtr dc, int w, int h);
    [DllImport("gdi32.dll")] static extern IntPtr SelectObject(IntPtr dc, IntPtr o);
    [DllImport("gdi32.dll")] static extern bool BitBlt(IntPtr dst, int x, int y, int w, int h, IntPtr src, int sx, int sy, uint rop);
    [DllImport("gdi32.dll")] static extern bool DeleteDC(IntPtr dc);
    [DllImport("gdi32.dll")] static extern bool DeleteObject(IntPtr o);
    [DllImport("dwmapi.dll")] static extern int DwmGetWindowAttribute(IntPtr h, int attr, out RECT r, int size);
    [StructLayout(LayoutKind.Sequential)] struct ANIMATIONINFO { public uint cbSize; public int iMinAnimate; }
    [DllImport("user32.dll")] static extern bool SystemParametersInfoW(uint action, uint param, ref ANIMATIONINFO info, uint flags);

    const uint EVENT_SYSTEM_FOREGROUND = 0x3, EVENT_SYSTEM_MOVESIZEEND = 0xB, EVENT_SYSTEM_MINIMIZESTART = 0x16;
    const int MaxSnaps = 8;

    record Snap(BitmapSource Img, Rect RectPx, DateTime At);

    readonly GenieWindow overlay = new();
    readonly Func<IntPtr, Rect?> iconRect;
    readonly double dpi;
    readonly Dictionary<IntPtr, Snap> snaps = new();
    readonly WinEventProc proc;
    readonly List<IntPtr> hooks = new();
    readonly DispatcherTimer timer = new() { Interval = TimeSpan.FromSeconds(6) };
    int originalMinAnimate = -1;

    public Genie(Func<IntPtr, Rect?> iconRect, double dpi)
    {
        this.iconRect = iconRect;
        this.dpi = dpi;
        proc = OnEvent;
        timer.Tick += (_, _) => Capture(GetForegroundWindow());
    }

    public void Start()
    {
        // Windows' own minimize animation would fight ours. Not written to the profile, so a reboot undoes it.
        var ai = new ANIMATIONINFO { cbSize = (uint)Marshal.SizeOf<ANIMATIONINFO>() };
        if (SystemParametersInfoW(0x48, ai.cbSize, ref ai, 0)) originalMinAnimate = ai.iMinAnimate;
        ai.iMinAnimate = 0;
        SystemParametersInfoW(0x49, ai.cbSize, ref ai, 2);

        foreach (var ev in new[] { EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MOVESIZEEND, EVENT_SYSTEM_MINIMIZESTART })
            hooks.Add(SetWinEventHook(ev, ev, IntPtr.Zero, proc, 0, 0, 0x2)); // out of context, skip own process
        overlay.Show();
        overlay.Visibility = Visibility.Hidden;
        timer.Start();
    }

    public void Stop()
    {
        timer.Stop();
        foreach (var h in hooks) UnhookWinEvent(h);
        hooks.Clear();
        if (originalMinAnimate >= 0)
        {
            var ai = new ANIMATIONINFO { cbSize = (uint)Marshal.SizeOf<ANIMATIONINFO>(), iMinAnimate = originalMinAnimate };
            SystemParametersInfoW(0x49, ai.cbSize, ref ai, 2);
        }
    }

    void OnEvent(IntPtr hook, uint ev, IntPtr h, int obj, int child, uint thread, uint time)
    {
        if (obj != 0 || h == IntPtr.Zero) return;
        if (ev == EVENT_SYSTEM_MINIMIZESTART) PlayMinimize(h);
        else
        {
            // let the window finish painting after it comes to front or stops moving
            var d = new DispatcherTimer { Interval = TimeSpan.FromMilliseconds(300) };
            d.Tick += (_, _) => { d.Stop(); if (GetForegroundWindow() == h) Capture(h); };
            d.Start();
        }
    }

    [StructLayout(LayoutKind.Sequential)]
    struct BITMAPINFOHEADER { public int biSize, biWidth, biHeight; public short biPlanes, biBitCount; public int biCompression, biSizeImage, biXPelsPerMeter, biYPelsPerMeter, biClrUsed, biClrImportant; }
    [DllImport("gdi32.dll")] static extern IntPtr CreateDIBSection(IntPtr dc, ref BITMAPINFOHEADER bi, uint usage, out IntPtr bits, IntPtr section, uint offset);
    [DllImport("gdi32.dll")] static extern bool StretchBlt(IntPtr dst, int x, int y, int w, int h, IntPtr src, int sx, int sy, int sw, int sh, uint rop);
    [DllImport("gdi32.dll")] static extern int SetStretchBltMode(IntPtr dc, int mode);
    [DllImport("gdi32.dll")] static extern bool SetBrushOrgEx(IntPtr dc, int x, int y, IntPtr old);
    [DllImport("gdi32.dll")] static extern bool GdiFlush();

    bool capturing;

    /// <summary>
    /// Copies the window's pixels straight off the screen (the window in front is what's visible there),
    /// shrunk to at most 1280 px wide by GDI so no full-size bitmap is ever allocated. Runs off the UI thread
    /// unless <paramref name="sync"/> (the dock is about to minimize it).
    /// </summary>
    public void Capture(IntPtr h, bool sync = false)
    {
        if (h == IntPtr.Zero || overlay.Busy || (capturing && !sync) || !IsWindowVisible(h) || IsIconic(h)) return;
        GetWindowThreadProcessId(h, out uint pid);
        if (pid == (uint)Environment.ProcessId) return;
        if (DwmGetWindowAttribute(h, 9, out RECT r, Marshal.SizeOf<RECT>()) != 0) GetWindowRect(h, out r); // extended frame bounds
        int sw = (int)Math.Round(SystemParameters.PrimaryScreenWidth * dpi), sh = (int)Math.Round(SystemParameters.PrimaryScreenHeight * dpi);
        r.Left = Math.Max(0, r.Left); r.Top = Math.Max(0, r.Top);
        r.Right = Math.Min(sw, r.Right); r.Bottom = Math.Min(sh, r.Bottom);
        int w = r.Right - r.Left, ht = r.Bottom - r.Top;
        if (w < 80 || ht < 60) return;
        var rect = new Rect(r.Left, r.Top, w, ht);

        if (sync) { if (Grab(r) is BitmapSource now) Store(h, now, rect); return; }
        capturing = true;
        System.Threading.Tasks.Task.Run(() => Grab(r)).ContinueWith(t =>
        {
            capturing = false;
            if (t.Status == System.Threading.Tasks.TaskStatus.RanToCompletion && t.Result is BitmapSource img) Store(h, img, rect);
        }, System.Threading.Tasks.TaskScheduler.FromCurrentSynchronizationContext());
    }

    static BitmapSource? Grab(RECT r)
    {
        int w = r.Right - r.Left, ht = r.Bottom - r.Top;
        int dw = Math.Min(w, 1280), dh = (int)Math.Round(ht * (double)dw / w);
        IntPtr sdc = GetDC(IntPtr.Zero), mdc = CreateCompatibleDC(sdc);
        var bi = new BITMAPINFOHEADER { biSize = 40, biWidth = dw, biHeight = -dh, biPlanes = 1, biBitCount = 32 };
        IntPtr dib = CreateDIBSection(sdc, ref bi, 0, out IntPtr bits, IntPtr.Zero, 0);
        try
        {
            if (dib == IntPtr.Zero) return null;
            var old = SelectObject(mdc, dib);
            SetStretchBltMode(mdc, 4); // HALFTONE: proper filtering when shrinking
            SetBrushOrgEx(mdc, 0, 0, IntPtr.Zero);
            StretchBlt(mdc, 0, 0, dw, dh, sdc, r.Left, r.Top, w, ht, 0x00CC0020); // SRCCOPY
            GdiFlush();
            SelectObject(mdc, old);
            var img = BitmapSource.Create(dw, dh, 96, 96, PixelFormats.Bgr32, null, bits, dw * dh * 4, dw * 4);
            img.Freeze();
            return img;
        }
        catch { return null; }
        finally { if (dib != IntPtr.Zero) DeleteObject(dib); DeleteDC(mdc); ReleaseDC(IntPtr.Zero, sdc); }
    }

    void Store(IntPtr h, BitmapSource img, Rect rect)
    {
        snaps[h] = new Snap(img, rect, DateTime.Now);
        if (snaps.Count > MaxSnaps)
            foreach (var k in snaps.Where(kv => !IsWindow(kv.Key)).Select(kv => kv.Key).Concat(snaps.OrderBy(kv => kv.Value.At).Select(kv => kv.Key)).Distinct().Take(snaps.Count - MaxSnaps).ToList())
                snaps.Remove(k);
    }

    Rect ToDip(Rect px) => new(px.X / dpi, px.Y / dpi, px.Width / dpi, px.Height / dpi);

    void PlayMinimize(IntPtr h)
    {
        if (!snaps.TryGetValue(h, out var s) || iconRect(h) is not Rect ic) return;
        overlay.Play(s.Img, ToDip(s.RectPx), ic, restore: false, null);
    }

    /// <summary>Plays the window coming out of the dock, then calls <paramref name="done"/> to actually restore it.</summary>
    public bool PlayRestore(IntPtr h, Action done)
    {
        if (!snaps.TryGetValue(h, out var s) || iconRect(h) is not Rect ic) return false;
        return overlay.Play(s.Img, ToDip(s.RectPx), ic, restore: true, done);
    }
}
