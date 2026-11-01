using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading.Tasks;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Input;
using System.Windows.Media;
using Windows.Devices.Radios;
using Windows.Devices.WiFi;
using Windows.Networking.Connectivity;
using Windows.Security.Credentials;

namespace MacDock;

/// <summary>Wi-Fi bars drawn as arcs (macOS style); missing bars are dimmed.</summary>
class WifiGlyph : FrameworkElement
{
    int bars = 3;
    bool off;

    public void Set(int signalBars, bool radioOff)
    {
        signalBars = Math.Clamp(signalBars, 0, 3);
        if (signalBars == bars && radioOff == off) return;
        bars = signalBars; off = radioOff;
        InvalidateVisual();
    }

    protected override Size MeasureOverride(Size available) => new(Width, Height);

    protected override void OnRender(DrawingContext dc)
    {
        double w = ActualWidth, h = ActualHeight;
        if (w <= 0 || h <= 0) return;
        var on = new SolidColorBrush(Colors.White);
        var dim = new SolidColorBrush(Color.FromArgb(0x4D, 255, 255, 255));
        var center = new Point(w / 2, h * 0.95);
        double stroke = Math.Max(1.2, h * 0.14);
        // a dot and two arcs: bar 1 = dot, 2 = inner arc, 3 = outer arc
        for (int i = 1; i <= 2; i++)
        {
            double r = h * (0.08 + 0.40 * i);
            var g = new StreamGeometry();
            using (var c = g.Open())
            {
                double s45 = Math.Sin(Math.PI / 4);
                c.BeginFigure(new Point(center.X - r * s45, center.Y - r * s45), false, false);
                c.ArcTo(new Point(center.X + r * s45, center.Y - r * s45), new Size(r, r), 0, false, SweepDirection.Clockwise, true, false);
            }
            var pen = new Pen(!off && bars >= i + 1 ? on : dim, stroke) { StartLineCap = PenLineCap.Round, EndLineCap = PenLineCap.Round };
            dc.DrawGeometry(null, pen, g);
        }
        double dot = Math.Max(1.2, h * 0.12);
        dc.DrawEllipse(!off && bars >= 1 ? on : dim, null, new Point(center.X, center.Y - dot), dot, dot);
    }
}

/// <summary>Wi-Fi state shared by the menu bar icon, the Wi-Fi flyout and Control Center.</summary>
static class Wifi
{
    static WiFiAdapter? adapter;
    static Radio? radio;
    static bool accessAsked;

    public static async Task<WiFiAdapter?> Adapter()
    {
        if (adapter != null) return adapter;
        try
        {
            if (!accessAsked)
            {
                accessAsked = true;
                if (await WiFiAdapter.RequestAccessAsync() != WiFiAccessStatus.Allowed) return null;
            }
            var all = await WiFiAdapter.FindAllAdaptersAsync();
            adapter = all.FirstOrDefault();
        }
        catch { }
        return adapter;
    }

    public static async Task<Radio?> Radio()
    {
        if (radio != null) return radio;
        try
        {
            await Windows.Devices.Radios.Radio.RequestAccessAsync();
            radio = (await Windows.Devices.Radios.Radio.GetRadiosAsync()).FirstOrDefault(r => r.Kind == RadioKind.WiFi);
        }
        catch { }
        return radio;
    }

    public static async Task<bool> IsOn() => (await Radio())?.State != RadioState.Off;

    public static async Task SetOn(bool on)
    {
        if (await Radio() is { } r) { try { await r.SetStateAsync(on ? RadioState.On : RadioState.Off); } catch { } }
    }

    /// <summary>Connected SSID and its signal bars (0-3), or null. Cheap: no scan.</summary>
    public static (string Ssid, int Bars)? Current()
    {
        try
        {
            var p = NetworkInformation.GetInternetConnectionProfile();
            if (p == null || !p.IsWlanConnectionProfile) return null;
            var ssid = p.WlanConnectionProfileDetails.GetConnectedSsid();
            byte? sig = p.GetSignalBars(); // 0-5
            int bars = sig is byte b ? (b >= 4 ? 3 : b >= 2 ? 2 : b >= 1 ? 1 : 0) : 3;
            return string.IsNullOrEmpty(ssid) ? null : (ssid, bars);
        }
        catch { return null; }
    }

    /// <summary>The PC is on a wired (or other non-Wi-Fi) connection.</summary>
    public static bool Wired()
    {
        try
        {
            var p = NetworkInformation.GetInternetConnectionProfile();
            return p != null && !p.IsWlanConnectionProfile && !p.IsWwanConnectionProfile
                && p.GetNetworkConnectivityLevel() != NetworkConnectivityLevel.None;
        }
        catch { return false; }
    }

    public static int Bars(byte signalBars) => signalBars >= 4 ? 3 : signalBars >= 2 ? 2 : signalBars >= 1 ? 1 : 0;
}

/// <summary>macOS-style Wi-Fi menu: switch, the joined network, nearby networks, join with a password.</summary>
class WifiFlyout : Flyout
{
    readonly StackPanel known = new(), others = new();
    readonly TextBlock status;
    readonly Border toggle;
    readonly TextBlock othersHeader;
    WiFiAvailableNetwork? expanded;

    public WifiFlyout() : base(310)
    {
        Title = "MacDock Wi-Fi";
        status = Label("", 11.5, 0x80);
        toggle = Switch(true, on => _ = Toggle(on));
        var header = new DockPanel { Margin = new Thickness(8, 0, 4, 6) };
        DockPanel.SetDock(toggle, Dock.Right);
        header.Children.Add(toggle);
        header.Children.Add(Label("Wi-Fi", 13.5, 0xF2, FontWeights.SemiBold));

        othersHeader = Label("Other Networks", 11.5, 0x80, FontWeights.SemiBold);
        othersHeader.Margin = new Thickness(8, 4, 0, 2);

        var stack = new StackPanel();
        stack.Children.Add(header);
        stack.Children.Add(status);
        status.Margin = new Thickness(8, 0, 0, 4);
        stack.Children.Add(known);
        stack.Children.Add(othersHeader);
        stack.Children.Add(new ScrollViewer
        {
            Content = others,
            MaxHeight = 330,
            VerticalScrollBarVisibility = ScrollBarVisibility.Hidden, // wheel still scrolls; WPF's bar is light-themed
        });
        stack.Children.Add(Divider());
        stack.Children.Add(Row(Label("Wi-Fi Settings…", 13), () => { Close(); Shell.Launch("ms-settings:network-wifi"); }));
        Panel.Padding = new Thickness(6, 12, 6, 6);
        Panel.Child = stack;

        Loaded += async (_, _) => await Reload(scan: true);
    }

    async Task Toggle(bool on)
    {
        status.Text = on ? "Turning on…" : "";
        await Wifi.SetOn(on);
        await Task.Delay(on ? 1500 : 200);
        await Reload(scan: on);
    }

    async Task Reload(bool scan)
    {
        bool on = await Wifi.IsOn();
        SetSwitch(toggle, on);
        known.Children.Clear();
        others.Children.Clear();
        othersHeader.Visibility = Visibility.Collapsed;
        if (!on) { status.Text = "Wi-Fi is off"; return; }

        var cur = Wifi.Current();
        if (cur is { } c) known.Children.Add(NetworkRow(c.Ssid, c.Bars, secured: true, connected: true, null));
        else if (Wifi.Wired()) status.Text = "Not connected · using Ethernet";

        var ad = await Wifi.Adapter();
        if (ad == null)
        {
            status.Text = "No access to Wi-Fi. Allow location for desktop apps in Settings › Privacy › Location.";
            status.TextWrapping = TextWrapping.Wrap;
            return;
        }
        if (scan)
        {
            if (known.Children.Count == 0 && string.IsNullOrEmpty(status.Text)) status.Text = "Searching…";
            try { await ad.ScanAsync(); } catch { }
        }
        if (status.Text == "Searching…") status.Text = "";

        var nets = ad.NetworkReport.AvailableNetworks
            .Where(n => !string.IsNullOrEmpty(n.Ssid) && n.Ssid != cur?.Ssid)
            .GroupBy(n => n.Ssid).Select(g => g.OrderByDescending(n => n.NetworkRssiInDecibelMilliwatts).First())
            .OrderByDescending(n => n.SignalBars).ThenBy(n => n.Ssid, StringComparer.OrdinalIgnoreCase)
            .ToList();
        if (nets.Count > 0) othersHeader.Visibility = Visibility.Visible;
        foreach (var n in nets)
        {
            bool secured = n.SecuritySettings.NetworkAuthenticationType is not (NetworkAuthenticationType.Open80211 or NetworkAuthenticationType.None);
            others.Children.Add(NetworkRow(n.Ssid, Wifi.Bars(n.SignalBars), secured, false, n));
            if (ReferenceEquals(n, expanded) || n.Ssid == expanded?.Ssid) others.Children.Add(PasswordRow(n));
        }
        if (known.Children.Count == 0 && nets.Count == 0 && string.IsNullOrEmpty(status.Text)) status.Text = "No networks found";
    }

    FrameworkElement NetworkRow(string ssid, int bars, bool secured, bool connected, WiFiAvailableNetwork? net)
    {
        var glyph = new WifiGlyph { Width = 15, Height = 11 };
        glyph.Set(bars, false);
        var bubble = new Border
        {
            Width = 26, Height = 26, CornerRadius = new CornerRadius(13),
            Background = new SolidColorBrush(connected ? Accent : Color.FromArgb(0x26, 255, 255, 255)),
            Child = glyph, Margin = new Thickness(0, 0, 10, 0),
        };
        var name = Label(ssid, 13, 0xF2, connected ? FontWeights.SemiBold : FontWeights.Normal);
        var right = new StackPanel { Orientation = Orientation.Horizontal };
        if (secured) right.Children.Add(Icon("", 11, Color.FromArgb(0x80, 255, 255, 255)));
        var dp = new DockPanel();
        DockPanel.SetDock(bubble, Dock.Left);
        DockPanel.SetDock(right, Dock.Right);
        dp.Children.Add(bubble);
        dp.Children.Add(right);
        if (connected)
        {
            var col = new StackPanel { VerticalAlignment = VerticalAlignment.Center };
            col.Children.Add(name);
            col.Children.Add(Label("Connected", 11, 0x80));
            dp.Children.Add(col);
        }
        else dp.Children.Add(name);

        Action click = connected
            ? () => _ = Disconnect()
            : () => _ = Join(net!, secured);
        var row = Row(dp, click);
        row.Padding = new Thickness(8, 5, 8, 5);
        if (connected) row.ToolTip = "Click to disconnect";
        return row;
    }

    async Task Disconnect()
    {
        (await Wifi.Adapter())?.Disconnect();
        await Task.Delay(800);
        await Reload(scan: false);
    }

    async Task Join(WiFiAvailableNetwork net, bool secured)
    {
        var ad = await Wifi.Adapter();
        if (ad == null) return;
        status.Text = $"Joining {net.Ssid}…";
        // a saved profile connects without asking; otherwise a secured network needs the password row
        var r = await ad.ConnectAsync(net, WiFiReconnectionKind.Automatic);
        if (r.ConnectionStatus == WiFiConnectionStatus.Success) { expanded = null; status.Text = ""; await Task.Delay(500); await Reload(false); return; }
        if (secured && r.ConnectionStatus is WiFiConnectionStatus.InvalidCredential or WiFiConnectionStatus.UnspecifiedFailure or WiFiConnectionStatus.AccessRevoked)
        {
            status.Text = "";
            expanded = net;
            await Reload(false);
            return;
        }
        status.Text = $"Couldn't join {net.Ssid} ({r.ConnectionStatus}).";
    }

    FrameworkElement PasswordRow(WiFiAvailableNetwork net)
    {
        var box = new PasswordBox
        {
            Height = 28, FontSize = 13, Padding = new Thickness(8, 4, 8, 4),
            Background = new SolidColorBrush(Color.FromArgb(0x1A, 255, 255, 255)),
            Foreground = Brushes.White, CaretBrush = Brushes.White,
            BorderBrush = new SolidColorBrush(Color.FromArgb(0x40, 255, 255, 255)),
            VerticalContentAlignment = VerticalAlignment.Center,
        };
        var join = new Button
        {
            Content = "Join", Width = 56, Height = 28, Margin = new Thickness(6, 0, 0, 0),
            Foreground = Brushes.White, Background = new SolidColorBrush(Accent), BorderThickness = new Thickness(0),
            Cursor = Cursors.Hand,
        };
        async void Go()
        {
            var ad = await Wifi.Adapter();
            if (ad == null || box.Password.Length == 0) return;
            status.Text = $"Joining {net.Ssid}…";
            var r = await ad.ConnectAsync(net, WiFiReconnectionKind.Automatic, new PasswordCredential { Password = box.Password });
            if (r.ConnectionStatus == WiFiConnectionStatus.Success) { expanded = null; status.Text = ""; await Task.Delay(500); await Reload(false); }
            else { status.Text = r.ConnectionStatus == WiFiConnectionStatus.InvalidCredential ? "Wrong password." : $"Couldn't join ({r.ConnectionStatus})."; box.SelectAll(); }
        }
        join.Click += (_, _) => Go();
        box.KeyDown += (_, e) => { if (e.Key == Key.Enter) Go(); };
        var dp = new DockPanel { Margin = new Thickness(44, 2, 8, 8) };
        DockPanel.SetDock(join, Dock.Right);
        dp.Children.Add(join);
        dp.Children.Add(box);
        Dispatcher.BeginInvoke(() => box.Focus(), System.Windows.Threading.DispatcherPriority.Input);
        return dp;
    }
}
