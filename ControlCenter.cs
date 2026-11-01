using System;
using System.IO;
using System.Threading.Tasks;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Input;
using System.Windows.Media;
using System.Windows.Media.Imaging;
using System.Windows.Threading;
using Windows.Devices.Radios;
using Windows.Media.Control;

namespace MacDock;

/// <summary>A macOS slider: a thick rounded track, white fill, the icon sitting inside on the left.</summary>
class MacSlider : Border
{
    readonly Border fill;
    readonly TextBlock glyph;
    double value;
    public event Action<double>? Changed;   // while dragging
    public event Action<double>? Committed; // on release

    public MacSlider(string icon)
    {
        Height = 26;
        CornerRadius = new CornerRadius(13);
        Background = new SolidColorBrush(Color.FromArgb(0x30, 255, 255, 255));
        ClipToBounds = true;
        Cursor = Cursors.Hand;
        fill = new Border { HorizontalAlignment = HorizontalAlignment.Left, Background = Brushes.White, CornerRadius = new CornerRadius(13) };
        glyph = Flyout.Icon(icon, 12, Color.FromRgb(0x55, 0x55, 0x5A));
        glyph.HorizontalAlignment = HorizontalAlignment.Left;
        glyph.Margin = new Thickness(8, 0, 0, 0);
        Child = new Grid { Children = { fill, glyph } };
        SizeChanged += (_, _) => Layout();
        SizeChanged += (_, e) => Clip = new RectangleGeometry(new Rect(e.NewSize), 13, 13);

        MouseLeftButtonDown += (_, e) => { CaptureMouse(); Drag(e); e.Handled = true; };
        MouseMove += (_, e) => { if (IsMouseCaptured) Drag(e); };
        MouseLeftButtonUp += (_, e) => { if (IsMouseCaptured) { ReleaseMouseCapture(); Committed?.Invoke(value); } };
    }

    public double Value { get => value; set { this.value = Math.Clamp(value, 0, 1); Layout(); } }
    public string Glyph { set => glyph.Text = value; }

    void Drag(MouseEventArgs e)
    {
        double x = e.GetPosition(this).X;
        Value = (x - 6) / Math.Max(1, ActualWidth - 12);
        Changed?.Invoke(value);
    }

    // the fill never shrinks below a circle so the icon always sits on white, as on macOS
    void Layout() => fill.Width = Math.Max(Height, Height + (ActualWidth - Height) * value);
}

/// <summary>The control-center item: connectivity, appearance, power mode, display, sound and now playing.</summary>
class ControlCenter : Flyout
{
    static readonly Color Tile = Color.FromArgb(0x14, 255, 255, 255);
    static readonly Color Off = Color.FromArgb(0x30, 255, 255, 255);

    readonly Action openWifi;
    readonly Border wifiDot, btDot, darkDot, powerDot;
    readonly TextBlock wifiSub, btSub, powerSub, darkSub;
    readonly MacSlider brightness = new(""), volume = new("");
    readonly TextBlock songTitle, songArtist;
    readonly Image art = new() { Width = 40, Height = 40, Stretch = Stretch.UniformToFill };
    readonly TextBlock playGlyph;
    readonly Border media;
    readonly DispatcherTimer poll = new() { Interval = TimeSpan.FromSeconds(2) };
    GlobalSystemMediaTransportControlsSessionManager? mediaManager;
    bool wifiOn, btOn;

    public ControlCenter(Action openWifi) : base(330)
    {
        this.openWifi = openWifi;
        Title = "MacDock Control Center";

        // ---- connectivity tile (two rows) and two square tiles beside it
        (var wifiRow, wifiDot, wifiSub) = Toggle("", "Wi-Fi", () => _ = ToggleWifi(), () => { Close(); this.openWifi(); });
        (var btRow, btDot, btSub) = Toggle("", "Bluetooth", () => _ = ToggleBt(), () => { Close(); Shell.Launch("ms-settings:bluetooth"); });
        var conn = new StackPanel();
        conn.Children.Add(wifiRow);
        conn.Children.Add(new Border { Height = 10 });
        conn.Children.Add(btRow);

        (var darkTile, darkDot, darkSub) = Toggle("", "Dark Mode", () => ToggleDark(), null);
        (var powerTile, powerDot, powerSub) = Toggle("", "Power Mode", () => CyclePower(), () => { Close(); Shell.Launch("ms-settings:powersleep"); });

        var right = new StackPanel();
        right.Children.Add(TileBox(darkTile));
        var gap = new Border { Height = 8 };
        right.Children.Add(gap);
        right.Children.Add(TileBox(powerTile));

        var top = new Grid();
        top.ColumnDefinitions.Add(new ColumnDefinition());
        top.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(8) });
        top.ColumnDefinitions.Add(new ColumnDefinition());
        var connBox = TileBox(conn);
        connBox.VerticalAlignment = VerticalAlignment.Stretch;
        top.Children.Add(connBox);
        Grid.SetColumn(right, 2);
        top.Children.Add(right);

        // ---- display and sound
        brightness.Committed += v => Brightness.Set((int)Math.Round(v * 100));
        brightness.Changed += v => Throttle(ref lastBright, () => Brightness.Set((int)Math.Round(v * 100)));
        volume.Changed += v => { Volume.Set(v); volume.Glyph = SpeakerGlyph(v); };
        var display = SliderTile("Display", brightness);
        var sound = SliderTile("Sound", volume);

        // ---- now playing
        songTitle = Label("", 13, 0xF2, FontWeights.SemiBold);
        songArtist = Label("", 11.5, 0x99);
        var text = new StackPanel { VerticalAlignment = VerticalAlignment.Center, Margin = new Thickness(10, 0, 6, 0) };
        text.Children.Add(songTitle);
        text.Children.Add(songArtist);
        playGlyph = Icon("", 15);
        var controls = new StackPanel { Orientation = Orientation.Horizontal, VerticalAlignment = VerticalAlignment.Center };
        controls.Children.Add(MediaButton(Icon("", 13), s => s.TrySkipPreviousAsync()));
        controls.Children.Add(MediaButton(playGlyph, s => s.TryTogglePlayPauseAsync()));
        controls.Children.Add(MediaButton(Icon("", 13), s => s.TrySkipNextAsync()));
        var artBox = new Border { Width = 40, Height = 40, CornerRadius = new CornerRadius(7), Background = new SolidColorBrush(Off), Child = art, ClipToBounds = true };
        artBox.Loaded += (_, _) => artBox.Clip = new RectangleGeometry(new Rect(0, 0, 40, 40), 7, 7);
        var mp = new DockPanel();
        DockPanel.SetDock(artBox, Dock.Left);
        DockPanel.SetDock(controls, Dock.Right);
        mp.Children.Add(artBox);
        mp.Children.Add(controls);
        mp.Children.Add(text);
        media = TileBox(mp);
        media.Visibility = Visibility.Collapsed;

        var stack = new StackPanel();
        stack.Children.Add(top);
        stack.Children.Add(Gap());
        stack.Children.Add(display);
        stack.Children.Add(Gap());
        stack.Children.Add(sound);
        stack.Children.Add(Gap());
        stack.Children.Add(media);
        Panel.Padding = new Thickness(10);
        Panel.Child = stack;

        Loaded += async (_, _) => await Refresh();
        poll.Tick += async (_, _) => await RefreshMedia();
        poll.Start();
        Closed += (_, _) => poll.Stop();
    }

    long lastBright;
    static void Throttle(ref long last, Action a)
    {
        long now = Environment.TickCount64;
        if (now - last < 120) return;
        last = now;
        Task.Run(a);
    }

    async Task Refresh()
    {
        wifiOn = await Wifi.IsOn();
        SetDot(wifiDot, wifiOn);
        wifiSub.Text = !wifiOn ? "Off" : Wifi.Current()?.Ssid ?? (Wifi.Wired() ? "Not connected · Ethernet" : "Not connected");

        var bt = await Bluetooth.Radio();
        btOn = bt?.State == RadioState.On;
        SetDot(btDot, btOn);
        btSub.Text = bt == null ? "Unavailable" : btOn ? "On" : "Off";

        SetDot(darkDot, Theme.Dark);
        darkSub.Text = Theme.Dark ? "On" : "Off";
        ShowPower();

        brightness.Value = (Brightness.Get() ?? 100) / 100.0;
        double v = Volume.Get() ?? 0;
        volume.Value = v;
        volume.Glyph = SpeakerGlyph(v);
        await RefreshMedia();
    }

    async Task ToggleWifi()
    {
        wifiOn = !wifiOn;
        SetDot(wifiDot, wifiOn);
        wifiSub.Text = wifiOn ? "Turning on…" : "Off";
        await Wifi.SetOn(wifiOn);
        await Task.Delay(wifiOn ? 3000 : 100);
        wifiSub.Text = !wifiOn ? "Off" : Wifi.Current()?.Ssid ?? "Not connected";
    }

    async Task ToggleBt()
    {
        btOn = !btOn;
        SetDot(btDot, btOn);
        btSub.Text = btOn ? "On" : "Off";
        await Bluetooth.SetOn(btOn);
    }

    void ToggleDark()
    {
        bool dark = !Theme.Dark;
        Theme.SetDark(dark);
        SetDot(darkDot, dark);
        darkSub.Text = dark ? "On" : "Off";
    }

    void CyclePower()
    {
        PowerMode.Set((PowerMode.Current() + 1) % PowerMode.Modes.Length);
        ShowPower();
    }

    void ShowPower()
    {
        int m = PowerMode.Current();
        powerSub.Text = PowerMode.Modes[m].Name;
        SetDot(powerDot, m != 1);
        ((TextBlock)powerDot.Child).Text = m == 0 ? "" : m == 2 ? "" : "";
    }

    async Task RefreshMedia()
    {
        try
        {
            mediaManager ??= await GlobalSystemMediaTransportControlsSessionManager.RequestAsync();
            var s = mediaManager.GetCurrentSession();
            if (s == null) { media.Visibility = Visibility.Collapsed; return; }
            var p = await s.TryGetMediaPropertiesAsync();
            if (p == null || string.IsNullOrEmpty(p.Title)) { media.Visibility = Visibility.Collapsed; return; }
            media.Visibility = Visibility.Visible;
            if (songTitle.Text != p.Title)
            {
                songTitle.Text = p.Title;
                art.Source = p.Thumbnail != null ? await Thumbnail(p.Thumbnail) : null;
            }
            songArtist.Text = string.IsNullOrEmpty(p.Artist) ? Shell.FriendlyName(null, s.SourceAppUserModelId, s.SourceAppUserModelId) : p.Artist;
            bool playing = s.GetPlaybackInfo()?.PlaybackStatus == GlobalSystemMediaTransportControlsSessionPlaybackStatus.Playing;
            playGlyph.Text = playing ? "" : "";
        }
        catch { media.Visibility = Visibility.Collapsed; }
    }

    static async Task<ImageSource?> Thumbnail(Windows.Storage.Streams.IRandomAccessStreamReference r)
    {
        try
        {
            using var ras = await r.OpenReadAsync();
            using var stream = ras.AsStreamForRead();
            var ms = new System.IO.MemoryStream();
            await stream.CopyToAsync(ms);
            ms.Position = 0;
            var bmp = new BitmapImage();
            bmp.BeginInit();
            bmp.CacheOption = BitmapCacheOption.OnLoad;
            bmp.DecodePixelWidth = 80;
            bmp.StreamSource = ms;
            bmp.EndInit();
            bmp.Freeze();
            return bmp;
        }
        catch { return null; }
    }

    Border MediaButton(TextBlock glyph, Func<GlobalSystemMediaTransportControlsSession, Windows.Foundation.IAsyncOperation<bool>> act)
    {
        var b = new Border { Width = 28, Height = 28, CornerRadius = new CornerRadius(14), Background = Brushes.Transparent, Child = glyph, Cursor = Cursors.Hand };
        var hover = new SolidColorBrush(Color.FromArgb(0x1C, 255, 255, 255));
        b.MouseEnter += (_, _) => b.Background = hover;
        b.MouseLeave += (_, _) => b.Background = Brushes.Transparent;
        b.MouseLeftButtonUp += async (_, _) =>
        {
            if (mediaManager?.GetCurrentSession() is { } s) { try { await act(s); } catch { } }
            await Task.Delay(250);
            await RefreshMedia();
        };
        return b;
    }

    static string SpeakerGlyph(double v) => v <= 0.001 ? "" : v < 0.34 ? "" : v < 0.67 ? "" : "";

    // ------------------------------------------------------------ building blocks

    static Border Gap() => new() { Height = 8 };

    static Border TileBox(UIElement child) => new()
    {
        CornerRadius = new CornerRadius(12),
        Background = new SolidColorBrush(Tile),
        BorderBrush = new SolidColorBrush(Color.FromArgb(0x14, 255, 255, 255)),
        BorderThickness = new Thickness(1),
        Padding = new Thickness(10),
        Child = child,
    };

    static Border SliderTile(string title, MacSlider slider)
    {
        var sp = new StackPanel();
        var t = Label(title, 12.5, 0xF2, FontWeights.SemiBold);
        t.Margin = new Thickness(2, 0, 0, 8);
        sp.Children.Add(t);
        sp.Children.Add(slider);
        return TileBox(sp);
    }

    /// <summary>Round icon button (toggles) and a title/subtitle beside it (opens more, when given).</summary>
    static (FrameworkElement Row, Border Dot, TextBlock Sub) Toggle(string glyph, string title, Action toggle, Action? more)
    {
        var dot = new Border
        {
            Width = 28, Height = 28, CornerRadius = new CornerRadius(14),
            Background = new SolidColorBrush(Off), Child = Icon(glyph, 13), Cursor = Cursors.Hand,
            Margin = new Thickness(0, 0, 9, 0),
        };
        dot.MouseLeftButtonUp += (_, e) => { toggle(); e.Handled = true; };
        var sub = Label("", 11, 0x99);
        var text = new StackPanel { VerticalAlignment = VerticalAlignment.Center, Background = Brushes.Transparent };
        text.Children.Add(Label(title, 12.5, 0xF2, FontWeights.SemiBold));
        text.Children.Add(sub);
        if (more != null)
        {
            text.Cursor = Cursors.Hand;
            text.MouseLeftButtonUp += (_, _) => more();
        }
        else text.MouseLeftButtonUp += (_, _) => toggle();
        var dp = new DockPanel();
        DockPanel.SetDock(dot, Dock.Left);
        dp.Children.Add(dot);
        dp.Children.Add(text);
        return (dp, dot, sub);
    }

    static void SetDot(Border dot, bool on) => dot.Background = new SolidColorBrush(on ? Accent : Off);
}
