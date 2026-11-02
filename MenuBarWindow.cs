using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Globalization;
using System.Linq;
using System.Runtime.InteropServices;
using System.Threading.Tasks;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Controls.Primitives;
using System.Windows.Input;
using System.Windows.Interop;
using System.Windows.Media;
using System.Windows.Shapes;
using System.Windows.Threading;
using Microsoft.Win32;
using static MacDock.Native;

namespace MacDock;

/// <summary>macOS-style menu bar: system menu, active app, generic app menus that send real shortcuts, status and clock.</summary>
class MenuBarWindow : Window
{
    delegate void WinEventProc(IntPtr hook, uint ev, IntPtr hwnd, int obj, int child, uint thread, uint time);
    [DllImport("user32.dll")] static extern IntPtr SetWinEventHook(uint min, uint max, IntPtr mod, WinEventProc proc, uint pid, uint tid, uint flags);
    [DllImport("user32.dll")] static extern bool UnhookWinEvent(IntPtr h);
    [DllImport("powrprof.dll")] static extern bool SetSuspendState(bool hibernate, bool force, bool disableWake);

    const double H = 30;
    double screenW = SystemParameters.PrimaryScreenWidth, screenH = SystemParameters.PrimaryScreenHeight; // primary screen in DIPs, refreshed on display changes
    readonly WinEventProc fgProc;
    IntPtr hook, hwnd;
    AppBar? appBar;
    double dpi = 1;

    IntPtr target;                 // last foreground window that isn't ours
    string appTitle = "File Explorer";
    readonly Dictionary<uint, string> nameCache = new();
    Flyout? flyout;            // the open menu bar extra (battery, Wi-Fi, background apps, control center)
    Type? flyoutType;
    Type? lastClosedType;
    int popupClosedAt;
    readonly TextBlock ethGlyph;
    readonly WifiGlyph wifiGlyph = new() { Width = 17, Height = 12.5, VerticalAlignment = VerticalAlignment.Center };

    readonly TextBlock appName, clock, battPct;
    readonly BatteryGlyph battGlyph = new() { Width = 26, Height = 12.5, VerticalAlignment = VerticalAlignment.Center, Margin = new Thickness(0, 1, 0, 0) };
    PowerMeter? powerMeter; // kept across flyout openings so its smoothing and "rest" learning carry on
    readonly FontFamily font = new("Segoe UI Variable Text, Segoe UI");
    readonly FontFamily icons = new("Segoe Fluent Icons, Segoe MDL2 Assets");

    public MenuBarWindow()
    {
        WindowStyle = WindowStyle.None;
        AllowsTransparency = true;
        Background = new SolidColorBrush(Color.FromArgb(1, 0, 0, 0));
        Topmost = true;
        ShowInTaskbar = false;
        ShowActivated = false;
        ResizeMode = ResizeMode.NoResize;
        Left = 0; Top = 0; Width = screenW; Height = H;
        Title = "MacDock Menu Bar";

        var left = new StackPanel { Orientation = Orientation.Horizontal, Margin = new Thickness(10, 0, 0, 0) };
        var right = new StackPanel { Orientation = Orientation.Horizontal, HorizontalAlignment = HorizontalAlignment.Right, Margin = new Thickness(0, 0, 10, 0) };

        left.Children.Add(Button(WindowsLogo(), b => ShowMenu(b, SystemMenu())));
        appName = Text("File Explorer", FontWeights.Bold);
        left.Children.Add(Button(appName, b => ShowMenu(b, AppMenu())));
        foreach (var (name, build) in new (string, Func<List<object>>)[] { ("File", FileMenu), ("Edit", EditMenu), ("View", ViewMenu), ("Window", WindowMenu), ("Help", HelpMenu) })
            left.Children.Add(Button(Text(name), b => ShowMenu(b, build())));

        battPct = Text("", FontWeights.Normal);
        battPct.Margin = new Thickness(0, 0, 6, 0);
        var batt = new StackPanel { Orientation = Orientation.Horizontal };
        batt.Children.Add(battPct); batt.Children.Add(battGlyph);
        if (Tray.Enabled)
        {
            var chevron = Glyph("");
            chevron.FontSize = 11;
            right.Children.Add(Button(chevron, b => Toggle(b, () => new TrayFlyout())));
        }
        var battButton = Button(batt, b => Toggle(b, () => new BatteryPopup(powerMeter ??= new PowerMeter())));
        battButton.MouseRightButtonUp += (_, _) => Shell.Launch("ms-settings:batterysaver");
        right.Children.Add(battButton);
        ethGlyph = Glyph("");
        ethGlyph.Visibility = Visibility.Collapsed;
        var net = new Grid { Children = { wifiGlyph, ethGlyph } };
        var wifiButton = Button(net, b => Toggle(b, () => new WifiFlyout()));
        wifiButton.MouseRightButtonUp += (_, _) => Shell.Launch("ms-settings:network-wifi");
        right.Children.Add(wifiButton);
        right.Children.Add(Button(Glyph(""), _ => Key(VK_LWIN, (byte)'S')));
        Border? ccButton = null;
        ccButton = Button(ControlCenterIcon(), b => Toggle(b, () => new ControlCenter(() => Toggle(wifiButton, () => new WifiFlyout(), force: true))));
        right.Children.Add(ccButton);
        clock = Text("", FontWeights.Normal);
        right.Children.Add(Button(clock, _ => Key(VK_LWIN, (byte)'N')));

        var grid = new Grid();
        grid.Children.Add(left);
        grid.Children.Add(right);
        Content = grid;

        fgProc = OnForeground;
        SourceInitialized += OnSourceInitialized;

        var t = new DispatcherTimer { Interval = TimeSpan.FromSeconds(1) };
        t.Tick += (_, _) => Tick();
        t.Start();
        Tick();
        IsVisibleChanged += (_, _) => { if (!IsVisible) flyout?.Close(); };
    }

    void OnSourceInitialized(object? s, EventArgs e)
    {
        hwnd = new WindowInteropHelper(this).Handle;
        MakeToolWindow(hwnd);
        EnableBlur(hwnd, 4, 0x6A1E1C1C);
        dpi = VisualTreeHelper.GetDpi(this).DpiScaleX;
        appBar = new AppBar(hwnd, ABE_TOP);
        appBar.Register((int)Math.Round(H * dpi), (int)Math.Round(screenW * dpi), (int)Math.Round(screenH * dpi));
        HwndSource.FromHwnd(hwnd).AddHook(WndProc);
        displayTimer.Tick += (_, _) => ApplyDisplay();
        hook = SetWinEventHook(3, 3, IntPtr.Zero, fgProc, 0, 0, 0); // EVENT_SYSTEM_FOREGROUND, out of context
        OnForeground(IntPtr.Zero, 3, GetForegroundWindow(), 0, 0, 0, 0);
    }

    IntPtr WndProc(IntPtr h, int msg, IntPtr w, IntPtr l, ref bool handled)
    {
        if (appBar != null && msg == appBar.CallbackMessage)
        {
            if ((int)w == ABN_POSCHANGED) appBar.Reposition((int)Math.Round(screenW * dpi), (int)Math.Round(screenH * dpi));
            else if ((int)w == ABN_FULLSCREENAPP) App.Fullscreen(l != IntPtr.Zero);
        }
        else if (msg == WM_DISPLAYCHANGE || msg == WM_DPICHANGED) { displayTimer.Stop(); displayTimer.Start(); }
        return IntPtr.Zero;
    }

    /// <summary>
    /// A monitor came or went (or resolution/scale changed): once Windows settles, stretch the bar across the
    /// primary screen again and reserve its strip at the new DPI.
    /// </summary>
    readonly DispatcherTimer displayTimer = new() { Interval = TimeSpan.FromMilliseconds(400) };

    void ApplyDisplay()
    {
        displayTimer.Stop();
        flyout?.Close();
        (screenW, screenH, dpi) = PrimaryScreen();
        PlaceWindow(hwnd, 0, 0, screenW, H, dpi);
        appBar?.Register((int)Math.Round(H * dpi), (int)Math.Round(screenW * dpi), (int)Math.Round(screenH * dpi));
    }

    public void Shutdown()
    {
        flyout?.Close();
        powerMeter?.Dispose();
        if (hook != IntPtr.Zero) UnhookWinEvent(hook);
        appBar?.Remove();
    }

    /// <summary>
    /// Opens a menu bar extra under its item, or closes it when it is the one already open. The click that
    /// closed a flyout (it lost focus to this very click) must not reopen it.
    /// </summary>
    void Toggle<T>(Border anchor, Func<T> make, bool force = false) where T : Flyout
    {
        // decide before building: some flyouts start work in their constructor (TrayFlyout hosts the tray)
        var type = typeof(T);
        if (!force)
        {
            if (flyout != null && flyoutType == type) { flyout.Close(); return; }
            if (lastClosedType == type && Environment.TickCount - popupClosedAt < 300) return;
        }
        flyout?.Close();
        var f = make();
        f.PlaceUnder(anchor, screenW);
        // stamp on Closing, before any Closed handler: those can pump messages (Tray.Stop) and let the very click
        // that closed us reach this method first
        f.Closing += (_, _) =>
        {
            lastClosedType = type;
            popupClosedAt = Environment.TickCount;
        };
        f.Closed += (_, _) => { if (flyout == f) { flyout = null; flyoutType = null; } };
        flyout = f;
        flyoutType = type;
        f.Show();
        f.Activate();
    }

    void OnForeground(IntPtr hk, uint ev, IntPtr h, int obj, int child, uint thread, uint time)
    {
        if (h == IntPtr.Zero) return;
        GetWindowThreadProcessId(h, out uint pid);
        if (pid == (uint)Environment.ProcessId) return;
        target = h;
        if (!nameCache.TryGetValue(pid, out var name))
        {
            var (exe, aumid) = Shell.ProcessInfo(pid);
            aumid = Shell.WindowAumid(h) ?? aumid;
            name = exe != null && exe.EndsWith(@"\explorer.exe", StringComparison.OrdinalIgnoreCase)
                ? "File Explorer"
                : Shell.FriendlyName(exe, aumid, "File Explorer");
            if (!exe?.EndsWith("ApplicationFrameHost.exe", StringComparison.OrdinalIgnoreCase) ?? true) nameCache[pid] = name;
        }
        appTitle = name;
        appName.Text = name;
    }

    int ticks;

    void Tick()
    {
        var now = DateTime.Now;
        clock.Text = now.ToString("ddd d MMM   HH:mm", CultureInfo.GetCultureInfo("en-GB"));
        if (GetSystemPowerStatus(out var ps) && ps.BatteryFlag != 128 && ps.BatteryLifePercent <= 100)
        {
            int pct = ps.BatteryLifePercent;
            bool onAc = ps.ACLineStatus == 1;
            battGlyph.Set(pct, onAc, onAc && (ps.BatteryFlag & 8) != 0);
            battPct.Text = pct + "%";
        }
        if (++ticks % 5 == 1)
        {
            var cur = Wifi.Current();
            bool wired = cur == null && Wifi.Wired();
            wifiGlyph.Set(cur?.Bars ?? 0, cur == null);
            wifiGlyph.Visibility = wired ? Visibility.Collapsed : Visibility.Visible;
            ethGlyph.Visibility = wired ? Visibility.Visible : Visibility.Collapsed;
        }
    }

    // ---------------------------------------------------------------- visuals

    TextBlock Text(string s, FontWeight? weight = null) => new()
    {
        Text = s, FontFamily = font, FontSize = 13.5, Foreground = Brushes.White,
        FontWeight = weight ?? FontWeights.Medium, VerticalAlignment = VerticalAlignment.Center,
    };

    TextBlock Glyph(string g) => new() { Text = g, FontFamily = icons, FontSize = 15, Foreground = Brushes.White, VerticalAlignment = VerticalAlignment.Center };

    static FrameworkElement WindowsLogo()
    {
        var g = new UniformGrid { Rows = 2, Columns = 2, Width = 13, Height = 13, VerticalAlignment = VerticalAlignment.Center };
        for (int i = 0; i < 4; i++) g.Children.Add(new Rectangle { Fill = Brushes.White, Margin = new Thickness(0.6) });
        return g;
    }

    static FrameworkElement ControlCenterIcon()
    {
        var sp = new StackPanel { VerticalAlignment = VerticalAlignment.Center };
        for (int i = 0; i < 2; i++)
        {
            var pill = new Border { Width = 17, Height = 7, CornerRadius = new CornerRadius(3.5), BorderBrush = Brushes.White, BorderThickness = new Thickness(1.3), Margin = new Thickness(0, i == 0 ? 0 : 2, 0, 0) };
            pill.Child = new Ellipse { Width = 3.6, Height = 3.6, Fill = Brushes.White, HorizontalAlignment = i == 0 ? HorizontalAlignment.Left : HorizontalAlignment.Right, Margin = new Thickness(1.2, 0, 1.2, 0) };
            sp.Children.Add(pill);
        }
        return sp;
    }

    Border Button(FrameworkElement content, Action<Border> click)
    {
        var b = new Border
        {
            Child = content,
            Padding = new Thickness(9, 0, 9, 0),
            Margin = new Thickness(1, 3, 1, 3),
            CornerRadius = new CornerRadius(5),
            Background = Brushes.Transparent,
        };
        var hover = new SolidColorBrush(Color.FromArgb(0x30, 255, 255, 255));
        b.MouseEnter += (_, _) => b.Background = hover;
        b.MouseLeave += (_, _) => { if (b.Tag == null) b.Background = Brushes.Transparent; };
        b.MouseLeftButtonUp += (_, _) => click(b);
        return b;
    }

    void ShowMenu(Border anchor, List<object> entries)
    {
        var cm = new ContextMenu();
        foreach (var e in entries) cm.Items.Add(e);
        cm.PlacementTarget = anchor;
        cm.Placement = PlacementMode.Bottom;
        cm.HorizontalOffset = -14;
        cm.VerticalOffset = -10;
        anchor.Tag = cm;
        anchor.Background = new SolidColorBrush(Color.FromArgb(0x40, 255, 255, 255));
        cm.Closed += (_, _) => { anchor.Tag = null; anchor.Background = Brushes.Transparent; };
        OpenMenu(this, cm);
    }

    // ---------------------------------------------------------------- menus

    static MenuItem Item(string text, Action act, string gesture = "", bool enabled = true, bool check = false)
    {
        var mi = new MenuItem { Header = text, InputGestureText = gesture, IsEnabled = enabled, IsChecked = check };
        mi.Click += (_, _) => act();
        return mi;
    }

    /// <summary>Hand focus back to the app the user was in, then send it the shortcut.</summary>
    async void Send(params byte[] keys)
    {
        if (target == IntPtr.Zero || !IsWindow(target)) return;
        Shell.Activate(target);
        await Task.Delay(90);
        Key(keys);
    }

    async void OnTarget(Action<IntPtr> act)
    {
        if (target == IntPtr.Zero || !IsWindow(target)) return;
        await Task.Delay(30);
        act(target);
    }

    IEnumerable<AppWindow> TargetAppWindows()
    {
        GetWindowThreadProcessId(target, out uint pid);
        return Shell.AppWindows().Where(w => w.Pid == pid);
    }

    List<object> SystemMenu()
    {
        bool atLogin = Registry.GetValue(@"HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Run", "MacDock", null) != null;
        return new()
        {
            Item("About This PC", () => Shell.Launch("ms-settings:about")),
            new Separator(),
            Item("System Settings…", () => Shell.Launch("ms-settings:")),
            Item("Microsoft Store…", () => Shell.Launch("ms-windows-store:")),
            new Separator(),
            Item("Force Quit…", () => Shell.Launch("taskmgr.exe"), "Ctrl+Shift+Esc"),
            new Separator(),
            Item("Sleep", () => SetSuspendState(false, false, false)),
            Item("Restart…", () => Confirm("Restart now?", () => Process.Start(new ProcessStartInfo("shutdown", "/r /t 0") { CreateNoWindow = true }))),
            Item("Shut Down…", () => Confirm("Shut down now?", () => Process.Start(new ProcessStartInfo("shutdown", "/s /t 0") { CreateNoWindow = true }))),
            new Separator(),
            Item("Lock Screen", () => LockWorkStation(), "Win+L"),
            new Separator(),
            Item("Automatically Hide Dock", () => App.Dock?.SetAutoHide(!App.Dock.AutoHide), check: App.Dock?.AutoHide == true),
            Item("Open MacDock at Login", ToggleLogin, check: atLogin),
            Item("Edit Dock Settings…", () => Shell.Launch(System.IO.Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData), "MacDock", "config.json"))),
            Item("Quit MacDock", App.Quit),
        };
    }

    static void Confirm(string q, Action yes)
    {
        if (MessageBox.Show(q, "MacDock", MessageBoxButton.OKCancel, MessageBoxImage.Question) == MessageBoxResult.OK) yes();
    }

    static void ToggleLogin()
    {
        using var k = Registry.CurrentUser.OpenSubKey(@"Software\Microsoft\Windows\CurrentVersion\Run", true)!;
        if (k.GetValue("MacDock") != null) k.DeleteValue("MacDock");
        else k.SetValue("MacDock", $"\"{Environment.ProcessPath}\"");
    }

    List<object> AppMenu() => new()
    {
        Item($"Hide {appTitle}", () => OnTarget(_ => { foreach (var w in TargetAppWindows()) Shell.Minimize(w.Hwnd); })),
        Item("Hide Others", () => OnTarget(_ =>
        {
            GetWindowThreadProcessId(target, out uint pid);
            foreach (var w in Shell.AppWindows().Where(w => w.Pid != pid)) Shell.Minimize(w.Hwnd);
        })),
        new Separator(),
        Item($"Quit {appTitle}", () => OnTarget(_ => { foreach (var w in TargetAppWindows()) Shell.Close(w.Hwnd); })),
    };

    List<object> FileMenu() => new()
    {
        Item("New Window", () => Send(VK_CONTROL, (byte)'N'), "Ctrl+N"),
        Item("New Tab", () => Send(VK_CONTROL, (byte)'T'), "Ctrl+T"),
        Item("Open…", () => Send(VK_CONTROL, (byte)'O'), "Ctrl+O"),
        new Separator(),
        Item("Save", () => Send(VK_CONTROL, (byte)'S'), "Ctrl+S"),
        Item("Save As…", () => Send(VK_CONTROL, VK_SHIFT, (byte)'S'), "Ctrl+Shift+S"),
        Item("Print…", () => Send(VK_CONTROL, (byte)'P'), "Ctrl+P"),
        new Separator(),
        Item("Close Window", () => OnTarget(Shell.Close), "Alt+F4"),
    };

    List<object> EditMenu() => new()
    {
        Item("Undo", () => Send(VK_CONTROL, (byte)'Z'), "Ctrl+Z"),
        Item("Redo", () => Send(VK_CONTROL, (byte)'Y'), "Ctrl+Y"),
        new Separator(),
        Item("Cut", () => Send(VK_CONTROL, (byte)'X'), "Ctrl+X"),
        Item("Copy", () => Send(VK_CONTROL, (byte)'C'), "Ctrl+C"),
        Item("Paste", () => Send(VK_CONTROL, (byte)'V'), "Ctrl+V"),
        Item("Select All", () => Send(VK_CONTROL, (byte)'A'), "Ctrl+A"),
        new Separator(),
        Item("Find…", () => Send(VK_CONTROL, (byte)'F'), "Ctrl+F"),
        new Separator(),
        Item("Emoji & Symbols", () => Send(VK_LWIN, 0xBE), "Win+."),
    };

    List<object> ViewMenu() => new()
    {
        Item("Enter Full Screen", () => Send(VK_F11), "F11"),
        new Separator(),
        Item("Zoom In", () => Send(VK_CONTROL, 0xBB), "Ctrl+="),
        Item("Zoom Out", () => Send(VK_CONTROL, 0xBD), "Ctrl+-"),
        Item("Actual Size", () => Send(VK_CONTROL, (byte)'0'), "Ctrl+0"),
    };

    List<object> WindowMenu() => new()
    {
        Item("Minimize", () => OnTarget(Shell.Minimize), "Win+↓"),
        Item("Zoom", () => OnTarget(h => PostMessageW(h, WM_SYSCOMMAND, (IntPtr)(IsZoomed(h) ? SC_RESTORE : SC_MAXIMIZE), IntPtr.Zero))),
        new Separator(),
        Item("Move to Left Half", () => Send(VK_LWIN, 0x25), "Win+←"),
        Item("Move to Right Half", () => Send(VK_LWIN, 0x27), "Win+→"),
        new Separator(),
        Item("Mission Control", () => Key(VK_LWIN, 0x09), "Win+Tab"),
    };

    List<object> HelpMenu() => new()
    {
        Item($"{appTitle} Help", () => Send(0x70), "F1"),
        Item("Windows Tips", () => Shell.Launch("ms-get-started:")),
    };
}
