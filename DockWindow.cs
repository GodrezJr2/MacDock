using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Linq;
using System.Runtime.InteropServices;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Controls.Primitives;
using System.Windows.Input;
using System.Windows.Interop;
using System.Windows.Media;
using System.Windows.Shapes;
using System.Windows.Threading;
using static MacDock.Native;

namespace MacDock;

class DockWindow : Window
{
    [DllImport("shell32.dll", CharSet = CharSet.Unicode)] static extern int SHEmptyRecycleBinW(IntPtr hwnd, string? root, uint flags);

    readonly Config cfg;
    readonly Canvas canvas = new();
    readonly Border plate;
    readonly Border frost;            // blurred slice of the wallpaper behind the plate
    readonly ImageBrush frostBrush = new() { ViewboxUnits = BrushMappingMode.Absolute, Stretch = Stretch.Fill };
    double frostW = 1, frostH = 1;    // size of the downsampled wallpaper
    readonly Border label;
    readonly TextBlock labelText;
    readonly Rectangle separator;
    readonly List<DockItem> items = new();
    readonly Dictionary<string, DockItem> pinnedCache = new();
    readonly DockItem trash;

    IntPtr hwnd;
    AppBar? appBar;
    Genie? genie;
    double dpi = 1;
    readonly double screenW = SystemParameters.PrimaryScreenWidth, screenH = SystemParameters.PrimaryScreenHeight;

    double B => cfg.IconSize;
    double Gap => Math.Round(B * 0.14);
    const double Pad = 7, BottomGap = 6, Radius = 20;
    double PlateH => B + Pad * 2;
    double PadX => Pad + 1;

    readonly Stopwatch clock = Stopwatch.StartNew();
    double lastT;
    double plateLeft, plateW;
    double labelAlpha;
    DockItem? hovered;
    bool menuOpen;
    int refreshCount;

    DockItem? pressed;
    Point pressPt;
    bool dragging, dragOut;
    double dragX;

    public DockWindow(Config cfg)
    {
        this.cfg = cfg;
        WindowStyle = WindowStyle.None;
        AllowsTransparency = true;
        Background = null;
        Topmost = true;
        ShowInTaskbar = false;
        ShowActivated = false;
        ResizeMode = ResizeMode.NoResize;
        Width = screenW;
        Height = BottomGap + PlateH + B * (cfg.Magnify - 1) + 64;
        Left = 0;
        Top = screenH - Height;
        Title = "MacDock";

        plate = new Border
        {
            CornerRadius = new CornerRadius(Radius),
            BorderThickness = new Thickness(1),
            Background = new LinearGradientBrush(Color.FromArgb(0x3A, 255, 255, 255), Color.FromArgb(0x1C, 255, 255, 255), 90),
            BorderBrush = new LinearGradientBrush(Color.FromArgb(0x9A, 255, 255, 255), Color.FromArgb(0x30, 255, 255, 255), 90),
        };
        separator = new Rectangle { Width = 1, Fill = new SolidColorBrush(Color.FromArgb(0x70, 255, 255, 255)) };
        labelText = new TextBlock
        {
            Foreground = Brushes.White,
            FontFamily = new FontFamily("Segoe UI Variable Text, Segoe UI"),
            FontSize = 13,
        };
        label = new Border
        {
            Child = labelText,
            Padding = new Thickness(11, 4, 11, 5),
            CornerRadius = new CornerRadius(7),
            Background = new SolidColorBrush(Color.FromArgb(0xD8, 0x26, 0x26, 0x2A)),
            BorderBrush = new SolidColorBrush(Color.FromArgb(0x40, 255, 255, 255)),
            BorderThickness = new Thickness(0.8),
            IsHitTestVisible = false,
            Opacity = 0,
        };
        frost = new Border { CornerRadius = new CornerRadius(Radius), Background = frostBrush };
        RenderOptions.SetBitmapScalingMode(frost, BitmapScalingMode.Linear);
        canvas.Children.Add(frost);
        canvas.Children.Add(plate);
        canvas.Children.Add(separator);
        canvas.Children.Add(label);
        Content = canvas;

        trash = new DockItem { Key = "trash", Name = "Recycle Bin", IsTrash = true, Launch = Shell.TrashPath, Icon = Shell.Icon(Shell.TrashPath, cache: false), Presence = 1 };

        SourceInitialized += OnSourceInitialized;
        MouseLeftButtonDown += OnLeftDown;
        MouseMove += OnMouseMove;
        MouseEnter += (_, _) => Wake();
        MouseLeave += (_, _) => Wake();
        MouseLeftButtonUp += OnLeftUp;
        MouseRightButtonUp += OnRightUp;
    }

    public void Start()
    {
        LoadWallpaper();
        Microsoft.Win32.SystemEvents.UserPreferenceChanged += (_, e) =>
        {
            if (e.Category == Microsoft.Win32.UserPreferenceCategory.Desktop) Dispatcher.BeginInvoke(LoadWallpaper);
        };
        Show();
        Refresh(initial: true);
        Wake();
        if (cfg.Genie)
        {
            genie = new Genie(IconRectFor, dpi);
            genie.Start();
        }
        var t = new DispatcherTimer { Interval = TimeSpan.FromMilliseconds(700) };
        t.Tick += (_, _) => { Refresh(); if (cfg.HideTaskbar) Taskbar.KeepHidden(); };
        t.Start();
    }

    public void Shutdown()
    {
        Sleep();
        appBar?.Remove();
        genie?.Stop();
    }

    public void SetFullscreenHidden(bool hide)
    {
        Visibility = hide ? Visibility.Hidden : Visibility.Visible;
        if (!hide) Wake();
    }

    /// <summary>
    /// Real DWM blur can't be clipped to a rounded plate on a layered window, so fake it: the dock is an
    /// AppBar, so what sits behind it is almost always the wallpaper. Downsample it hard (that is the blur)
    /// and paint the slice under the plate.
    /// </summary>
    void LoadWallpaper()
    {
        try
        {
            string? path = Microsoft.Win32.Registry.GetValue(@"HKEY_CURRENT_USER\Control Panel\Desktop", "WallPaper", null) as string;
            string cached = System.IO.Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData), @"Microsoft\Windows\Themes\TranscodedWallpaper");
            if (string.IsNullOrEmpty(path) || !System.IO.File.Exists(path)) path = cached;
            if (!System.IO.File.Exists(path)) return;

            var src = new System.Windows.Media.Imaging.BitmapImage();
            using (var fs = System.IO.File.OpenRead(path))
            {
                src.BeginInit();
                src.CacheOption = System.Windows.Media.Imaging.BitmapCacheOption.OnLoad;
                src.DecodePixelWidth = 480;
                src.StreamSource = fs;
                src.EndInit();
            }
            // "Fill" fit: cover the screen, centred
            frostW = 96; frostH = Math.Round(96 * screenH / screenW);
            double s = Math.Max(frostW / src.PixelWidth, frostH / src.PixelHeight);
            double dw = src.PixelWidth * s, dh = src.PixelHeight * s;
            var dv = new DrawingVisual();
            RenderOptions.SetBitmapScalingMode(dv, BitmapScalingMode.HighQuality);
            using (var dc = dv.RenderOpen())
                dc.DrawImage(src, new Rect((frostW - dw) / 2, (frostH - dh) / 2, dw, dh));
            var rtb = new System.Windows.Media.Imaging.RenderTargetBitmap((int)frostW, (int)frostH, 96, 96, PixelFormats.Pbgra32);
            rtb.Render(dv);
            rtb.Freeze();
            frostBrush.ImageSource = rtb;
            Wake();
        }
        catch { frostBrush.ImageSource = null; }
    }

    void OnSourceInitialized(object? s, EventArgs e)
    {
        hwnd = new WindowInteropHelper(this).Handle;
        MakeToolWindow(hwnd);
        dpi = VisualTreeHelper.GetDpi(this).DpiScaleX;
        appBar = new AppBar(hwnd, ABE_BOTTOM);
        appBar.Register((int)Math.Round((PlateH + BottomGap * 2) * dpi), (int)Math.Round(screenW * dpi), (int)Math.Round(screenH * dpi));
        HwndSource.FromHwnd(hwnd).AddHook(WndProc);
    }

    IntPtr WndProc(IntPtr h, int msg, IntPtr w, IntPtr l, ref bool handled)
    {
        if (appBar != null && msg == appBar.CallbackMessage)
        {
            if ((int)w == ABN_POSCHANGED) appBar.Reposition((int)Math.Round(screenW * dpi), (int)Math.Round(screenH * dpi));
            else if ((int)w == ABN_FULLSCREENAPP) App.Fullscreen(l != IntPtr.Zero);
        }
        return IntPtr.Zero;
    }

    // ---------------------------------------------------------------- model

    DockItem Pinned(string spec)
    {
        if (pinnedCache.TryGetValue(spec, out var it)) return it;
        it = new DockItem { Key = "pin:" + spec.ToLowerInvariant(), Launch = spec, Pinned = true };
        const string apps = @"shell:AppsFolder\";
        if (spec.StartsWith(apps, StringComparison.OrdinalIgnoreCase))
        {
            it.Aumid = spec[apps.Length..];
            var target = Shell.LinkTarget(spec);
            if (target != null && target.EndsWith(".exe", StringComparison.OrdinalIgnoreCase)) it.Exe = target;
            if (it.Aumid.Equals("Microsoft.Windows.Explorer", StringComparison.OrdinalIgnoreCase))
                it.Exe = System.IO.Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.Windows), "explorer.exe");
        }
        else if (spec.EndsWith(".lnk", StringComparison.OrdinalIgnoreCase))
        {
            it.Aumid = Shell.ItemAumid(spec);
            it.Exe = Shell.LinkTarget(spec);
        }
        else it.Exe = spec;
        it.Name = Shell.DisplayName(spec) ?? Shell.FriendlyName(it.Exe, it.Aumid, spec);
        if (it.Exe != null && it.Name.EndsWith(".exe", StringComparison.OrdinalIgnoreCase)) it.Name = Shell.FriendlyName(it.Exe, null, it.Name);
        it.Icon = Shell.Icon(spec) ?? (it.Exe != null ? Shell.Icon(it.Exe) : null);
        return pinnedCache[spec] = it;
    }

    DockItem Running(string key, AppWindow w)
    {
        var old = items.FirstOrDefault(i => i.Key == key);
        if (old != null) return old;
        string? launch = w.Aumid != null ? @"shell:AppsFolder\" + w.Aumid : w.Exe;
        return new DockItem
        {
            Key = key,
            Aumid = w.Aumid,
            Exe = w.Exe,
            Launch = launch,
            Name = Shell.FriendlyName(w.Exe, w.Aumid, w.Title),
            Icon = (launch != null ? Shell.Icon(launch) : null) ?? (w.Exe != null ? Shell.Icon(w.Exe) : null),
        };
    }

    void Refresh(bool initial = false)
    {
        var wins = Shell.AppWindows();
        var next = new List<DockItem>();
        foreach (var spec in cfg.Pinned) { var p = Pinned(spec); p.Windows.Clear(); if (!next.Contains(p)) next.Add(p); }

        var running = new List<DockItem>();
        foreach (var w in wins)
        {
            var owner = next.FirstOrDefault(i => i.Owns(w)) ?? running.FirstOrDefault(i => i.Owns(w));
            if (owner == null)
            {
                string key = "run:" + (w.Aumid ?? w.Exe ?? w.Pid.ToString()).ToLowerInvariant();
                owner = Running(key, w);
                owner.Windows.Clear();
                running.Add(owner);
            }
            owner.Windows.Add(w);
        }
        // keep running apps in the order they first appeared instead of reshuffling by focus
        running = running.OrderBy(r => { int i = items.IndexOf(r); return i < 0 ? int.MaxValue : i; }).ToList();
        next.AddRange(running);

        if (++refreshCount % 40 == 0) { var ic = Shell.Icon(Shell.TrashPath, cache: false); if (ic != null) trash.Icon = ic; if (trash.Img != null) trash.Img.Source = trash.Icon; }
        trash.Windows.Clear();
        next.Add(trash);

        foreach (var it in next)
        {
            it.Leaving = false;
            if (it.Launching && (it.Windows.Count > it.WindowsAtLaunch || (DateTime.Now - it.LaunchStarted).TotalSeconds > 10)) it.Launching = false;
            if (it.Img == null) CreateVisuals(it, initial);
        }
        // items that vanished shrink away in place before they are dropped
        for (int i = 0; i < items.Count; i++)
        {
            var old = items[i];
            if (next.Contains(old)) continue;
            old.Leaving = true;
            next.Insert(Math.Min(i, next.Count - 1), old);
        }
        items.Clear();
        items.AddRange(next);

        string sig = string.Join("|", items.Select(i => i.Key + (i.Running ? "*" : "") + (i.Leaving ? "-" : "") + (i.Launching ? "^" : "")));
        if (sig != lastSig) { lastSig = sig; FitWidth(); Wake(); }
        if (items.Any(i => i.Launching)) Wake();
    }
    string lastSig = "";

    void CreateVisuals(DockItem it, bool instant)
    {
        it.Img = new Image { Source = it.Icon, Stretch = Stretch.Uniform, IsHitTestVisible = false };
        RenderOptions.SetBitmapScalingMode(it.Img, BitmapScalingMode.HighQuality);
        it.Dot = new Ellipse { Width = 5, Height = 5, Fill = new SolidColorBrush(Color.FromArgb(0xD0, 0x1C, 0x1C, 0x22)), Opacity = 0, IsHitTestVisible = false };
        int at = canvas.Children.IndexOf(label);
        canvas.Children.Insert(at, it.Dot);
        canvas.Children.Insert(at, it.Img);
        it.Presence = instant ? 1 : 0;
        it.Scale = 1;
    }

    // ---------------------------------------------------------------- frame

    /// <summary>Lays items out left to right; returns content width. Item i gets (x, size).</summary>
    double Layout(Func<DockItem, double> scale, double left, Action<DockItem, double, double>? place, Action<double>? sep)
    {
        double x = left;
        foreach (var it in items)
        {
            if (it.IsTrash)
            {
                sep?.Invoke(x + Gap * 0.5);
                x += Gap + 1;
            }
            double size = B * scale(it) * it.Presence;
            place?.Invoke(it, x, size);
            x += (B * scale(it) + Gap) * it.Presence;
        }
        return x - left - Gap;
    }

    void OnFrame(object? s, EventArgs e)
    {
        double t = clock.Elapsed.TotalSeconds;
        double dt = Math.Clamp(t - lastT, 0, 0.05);
        lastT = t;

        GetCursorPos(out var cp);
        double mx = cp.X / dpi - Left, my = cp.Y / dpi - Top;
        double plateBottom = Height - BottomGap, plateTop = plateBottom - PlateH;
        double tallest = items.Count > 0 ? items.Max(i => i.Size) : B;
        bool hovering = !menuOpen && IsVisible &&
            mx >= plateLeft && mx <= plateLeft + plateW &&
            my >= plateBottom - Pad - tallest - 6 && my <= Height + 2;
        if (dragging) hovering = my >= plateTop - 70;

        // magnification targets come from the resting (unmagnified) layout so they don't feed back on themselves
        double restW = Layout(_ => 1, 0, null, null) + PadX * 2;
        double restLeft = (Width - restW) / 2 + PadX;
        var centers = new Dictionary<DockItem, double>();
        Layout(_ => 1, restLeft, (it, x, size) => centers[it] = x + B * it.Presence / 2, null);

        double R = B * 2.9, k = 1 - Math.Exp(-dt * 17), kp = 1 - Math.Exp(-dt * 9);
        bool settled = !dragging && pressed == null;
        foreach (var it in items)
        {
            double target = 1;
            if (hovering)
            {
                double d = Math.Abs(mx - centers[it]);
                if (d < R) target = 1 + (cfg.Magnify - 1) * (Math.Cos(Math.PI * d / R) + 1) / 2;
            }
            it.Scale = Approach(it.Scale, target, k);
            it.Presence = Approach(it.Presence, it.Leaving ? 0 : 1, kp);
            if (it.Scale != target || (!it.Leaving && it.Presence != 1) || it.Launching || it.BounceT > 0) settled = false;
        }
        foreach (var gone in items.Where(i => i.Leaving && i.Presence < 0.02).ToList())
        {
            canvas.Children.Remove(gone.Img);
            canvas.Children.Remove(gone.Dot);
            gone.Img = null; gone.Dot = null;
            items.Remove(gone);
        }

        double contentW = Layout(i => i.Scale, 0, null, null);
        plateW = contentW + PadX * 2;
        plateLeft = (Width - plateW) / 2;

        Canvas.SetLeft(plate, plateLeft); Canvas.SetTop(plate, plateTop);
        plate.Width = plateW; plate.Height = PlateH;

        hovered = null;
        Layout(i => i.Scale, plateLeft + PadX, (it, x, size) =>
        {
            it.X = x; it.Size = size;
            double bounce = 0;
            if (it.Launching || it.BounceT > 0)
            {
                const double period = 0.62;
                double prev = it.BounceT % period;
                it.BounceT += dt;
                double cur = it.BounceT % period;
                if (!it.Launching && cur < prev) it.BounceT = 0;
                else bounce = B * 0.55 * Math.Abs(Math.Sin(Math.PI * cur / period));
            }
            double ix = it == pressed && dragging ? dragX - size / 2 : x;
            Canvas.SetLeft(it.Img!, ix);
            Canvas.SetTop(it.Img!, plateBottom - Pad - size - bounce);
            it.Img!.Width = it.Img.Height = Math.Max(0.1, size);
            it.Img.Opacity = it == pressed ? (dragging ? (dragOut ? 0.45 : 0.9) : 0.6) : Math.Min(1, it.Presence * 1.3);

            it.DotAlpha = Approach(it.DotAlpha, it.Running ? 1 : 0, kp);
            if (it.DotAlpha != (it.Running ? 1 : 0)) settled = false;
            it.Dot!.Opacity = it.DotAlpha * it.Presence;
            Canvas.SetLeft(it.Dot, ix + size / 2 - 2.5);
            Canvas.SetTop(it.Dot, plateBottom - 5.5);

            if (hovering && !dragging && mx >= x - Gap / 2 && mx < x + size + Gap / 2) hovered = it;
        }, sx =>
        {
            Canvas.SetLeft(separator, sx);
            Canvas.SetTop(separator, plateTop + 11);
            separator.Height = PlateH - 22;
        });

        // tooltip label above the hovered icon
        labelAlpha = Approach(labelAlpha, hovered != null ? 1 : 0, 1 - Math.Exp(-dt * 22));
        if (labelAlpha != (hovered != null ? 1 : 0)) settled = false;
        if (hovered != null)
        {
            if (labelText.Text != hovered.Name) { labelText.Text = hovered.Name; label.Measure(new Size(double.PositiveInfinity, double.PositiveInfinity)); }
            var ls = label.DesiredSize;
            double lx = Math.Clamp(hovered.X + hovered.Size / 2 - ls.Width / 2, 4, Width - ls.Width - 4);
            Canvas.SetLeft(label, lx);
            Canvas.SetTop(label, plateBottom - Pad - hovered.Size - ls.Height - 10);
        }
        label.Opacity = labelAlpha;

        Canvas.SetLeft(frost, plateLeft); Canvas.SetTop(frost, plateTop);
        frost.Width = plateW; frost.Height = PlateH;
        double fx = frostW / screenW, fy = frostH / screenH;
        frostBrush.Viewbox = new Rect((Left + plateLeft) * fx, (Top + plateTop) * fy, plateW * fx, PlateH * fy);

        // nothing moving and the pointer is away: stop ticking until something wakes us
        if (settled && !hovering && items.All(i => !i.Leaving)) Sleep();
    }

    static double Approach(double v, double target, double k)
    {
        v += (target - v) * k;
        return Math.Abs(target - v) < 0.002 ? target : v;
    }

    bool awake;
    void Wake()
    {
        if (awake) return;
        awake = true;
        lastT = clock.Elapsed.TotalSeconds;
        CompositionTarget.Rendering += OnFrame;
    }
    void Sleep()
    {
        if (!awake) return;
        awake = false;
        CompositionTarget.Rendering -= OnFrame;
    }

    /// <summary>The window only needs to be as wide as a fully magnified dock plus room for labels.</summary>
    void FitWidth()
    {
        double w = Math.Min(screenW, items.Count * (B * cfg.Magnify + Gap) + PadX * 2 + 2 * Gap + 320);
        if (Math.Abs(w - Width) < 1) return;
        Width = w;
        Left = (screenW - w) / 2;
    }

    // ---------------------------------------------------------------- input

    DockItem? ItemAt(double x) => items.FirstOrDefault(i => !i.Leaving && x >= i.X - Gap / 2 && x < i.X + i.Size + Gap / 2);

    void OnLeftDown(object s, MouseButtonEventArgs e)
    {
        pressPt = e.GetPosition(canvas);
        pressed = ItemAt(pressPt.X);
        dragging = false; dragOut = false;
        if (pressed != null) CaptureMouse();
    }

    void OnMouseMove(object s, MouseEventArgs e)
    {
        Wake();
        if (pressed == null || e.LeftButton != MouseButtonState.Pressed) return;
        var p = e.GetPosition(canvas);
        if (!dragging && pressed.Pinned && (Math.Abs(p.X - pressPt.X) > 8 || Math.Abs(p.Y - pressPt.Y) > 8)) dragging = true;
        if (!dragging) return;
        dragX = p.X;
        dragOut = p.Y < Height - BottomGap - PlateH - 60;

        // reorder pinned items as the dragged icon passes a neighbour's centre
        int from = items.IndexOf(pressed);
        var pinnedItems = items.Where(i => i.Pinned && !i.Leaving).ToList();
        foreach (var other in pinnedItems)
        {
            if (other == pressed) continue;
            double c = other.X + other.Size / 2;
            int oi = items.IndexOf(other);
            if ((oi > from && dragX > c) || (oi < from && dragX < c))
            {
                items.RemoveAt(from);
                items.Insert(oi, pressed);
                break;
            }
        }
    }

    void OnLeftUp(object s, MouseButtonEventArgs e)
    {
        ReleaseMouseCapture();
        var it = pressed;
        pressed = null;
        if (it == null) return;
        if (dragging)
        {
            dragging = false;
            if (dragOut) Unpin(it);
            else
            {
                cfg.Pinned = items.Where(i => i.Pinned && !i.Leaving).Select(i => i.Launch!).ToList();
                cfg.Save();
            }
            return;
        }
        if (ItemAt(e.GetPosition(canvas).X) == it) Click(it);
    }

    void Click(DockItem it)
    {
        if (it.IsTrash) { Shell.Launch(Shell.TrashPath); return; }
        if (!it.Running)
        {
            if (it.Launch == null) return;
            Shell.Launch(it.Launch);
            it.Launching = true;
            it.LaunchStarted = DateTime.Now;
            it.WindowsAtLaunch = 0;
            Wake();
            return;
        }
        var fg = GetForegroundWindow();
        if (it.Windows.Any(w => w.Hwnd == fg))
        {
            void MinimizeAll() { foreach (var w in it.Windows) Shell.Minimize(w.Hwnd); }
            if (genie != null) genie.MinimizeFromDock(fg, MinimizeAll);
            else MinimizeAll();
            return;
        }
        var minimized = it.Windows.Where(w => IsIconic(w.Hwnd)).ToList();
        var front = minimized.Count > 0 ? minimized[0].Hwnd : it.Windows[0].Hwnd;
        void Restore()
        {
            foreach (var w in Enumerable.Reverse(minimized)) ShowWindow(w.Hwnd, SW_RESTORE);
            Shell.Activate(front);
        }
        if (minimized.Count > 0 && genie != null && genie.PlayRestore(front, Restore)) return;
        Restore();
    }

    /// <summary>Screen rect (DIPs) of the dock icon that owns a window, for the genie to aim at.</summary>
    Rect? IconRectFor(IntPtr hwnd)
    {
        var it = items.FirstOrDefault(i => !i.Leaving && i.Windows.Any(w => w.Hwnd == hwnd));
        double plateBottom = Height - BottomGap;
        if (it == null) return new Rect(Left + Width / 2 - B / 2, Top + plateBottom - Pad - B, B, B);
        return new Rect(Left + it.X, Top + plateBottom - Pad - it.Size, it.Size, it.Size);
    }

    void Unpin(DockItem it)
    {
        cfg.Pinned.RemoveAll(p => string.Equals(p, it.Launch, StringComparison.OrdinalIgnoreCase));
        cfg.Save();
        pinnedCache.Remove(it.Launch!);
        it.Leaving = true;
        Refresh();
    }

    void OnRightUp(object s, MouseButtonEventArgs e)
    {
        var it = ItemAt(e.GetPosition(canvas).X);
        if (it == null || it.Img == null) return;
        e.Handled = true;

        var cm = new ContextMenu();
        void Add(string text, Action act, bool enabled = true)
        {
            var mi = new MenuItem { Header = text, IsEnabled = enabled };
            mi.Click += (_, _) => act();
            cm.Items.Add(mi);
        }

        if (it.IsTrash)
        {
            Add("Open", () => Shell.Launch(Shell.TrashPath));
            Add("Empty Recycle Bin", () => { SHEmptyRecycleBinW(IntPtr.Zero, null, 0); Refresh(); });
        }
        else
        {
            foreach (var w in it.Windows.Take(12))
            {
                var win = w;
                Add(win.Title.Length > 48 ? win.Title[..47] + "…" : win.Title, () => Shell.Activate(win.Hwnd));
            }
            if (it.Running) cm.Items.Add(new Separator());
            if (it.Launch != null) Add(it.Running ? "New Window" : "Open", () => Shell.Launch(it.Launch));
            if (it.Pinned) Add("Remove from Dock", () => Unpin(it));
            else if (it.Launch != null) Add("Keep in Dock", () =>
            {
                cfg.Pinned.Add(it.Launch);
                cfg.Save();
                items.Remove(it);
                canvas.Children.Remove(it.Img); canvas.Children.Remove(it.Dot);
                it.Img = null; it.Dot = null;
                Refresh(initial: true);
            });
            if (it.Exe != null && System.IO.File.Exists(it.Exe))
                Add("Show in Explorer", () => Process.Start("explorer.exe", $"/select,\"{it.Exe}\""));
            if (it.Running)
            {
                cm.Items.Add(new Separator());
                Add("Quit", () => { foreach (var w in it.Windows) Shell.Close(w.Hwnd); });
            }
        }

        cm.PlacementTarget = it.Img;
        cm.Placement = PlacementMode.Custom;
        cm.CustomPopupPlacementCallback = (popup, target, _) =>
            new[] { new CustomPopupPlacement(new Point((target.Width - popup.Width) / 2, -popup.Height + 8), PopupPrimaryAxis.Horizontal) };
        cm.Opened += (_, _) => menuOpen = true;
        cm.Closed += (_, _) => { menuOpen = false; Wake(); };
        OpenMenu(this, cm);
    }
}
