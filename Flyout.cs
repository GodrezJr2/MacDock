using System;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Input;
using System.Windows.Interop;
using System.Windows.Media;
using System.Windows.Media.Effects;
using static MacDock.Native;

namespace MacDock;

/// <summary>
/// A dark rounded panel that drops from a menu bar item, macOS style: takes focus while open (so text boxes and
/// keys work), closes on Esc or when anything else is clicked.
/// </summary>
class Flyout : Window
{
    protected readonly Border Panel;
    public bool StayOpen; // set while an action of ours briefly moves focus away

    protected Flyout(double width)
    {
        WindowStyle = WindowStyle.None;
        AllowsTransparency = true;
        Background = Brushes.Transparent;
        Topmost = true;
        ShowInTaskbar = false;
        ResizeMode = ResizeMode.NoResize;
        SizeToContent = SizeToContent.Height;
        Width = width;
        Left = Math.Max(8, SystemParameters.PrimaryScreenWidth - Width - 40);
        Top = 30;

        Panel = new Border
        {
            CornerRadius = new CornerRadius(16),
            Background = new SolidColorBrush(Color.FromArgb(0xF2, 0x1C, 0x1C, 0x1F)),
            BorderBrush = new SolidColorBrush(Color.FromArgb(0x2A, 255, 255, 255)),
            BorderThickness = new Thickness(1),
            Padding = new Thickness(14),
            Effect = new DropShadowEffect { BlurRadius = 26, ShadowDepth = 6, Direction = 270, Opacity = 0.5, Color = Colors.Black },
        };
        Content = new Grid { Margin = new Thickness(13), Children = { Panel } };

        SourceInitialized += (_, _) =>
        {
            var h = new WindowInteropHelper(this).Handle;
            long ex = (long)GetWindowLongPtr(h, GWL_EXSTYLE);
            ex |= WS_EX_TOOLWINDOW;
            ex &= ~(WS_EX_NOACTIVATE | WS_EX_APPWINDOW);
            SetWindowLongPtr(h, GWL_EXSTYLE, (IntPtr)ex);
        };
        Deactivated += (_, _) => { if (!StayOpen) Close(); };
        PreviewKeyDown += (_, e) => { if (e.Key == System.Windows.Input.Key.Escape) Close(); };
    }

    /// <summary>Right-aligns the flyout under a menu bar item (13 px is the shadow margin around the panel).</summary>
    public void PlaceUnder(FrameworkElement anchor, double screenW)
    {
        var edge = anchor.PointToScreen(new Point(anchor.ActualWidth, anchor.ActualHeight));
        double scale = VisualTreeHelper.GetDpi(anchor).DpiScaleX;
        Left = Math.Max(8, Math.Min(screenW - Width - 4, edge.X / scale - Width + 13));
        Top = edge.Y / scale - 7;
    }

    // ------------------------------------------------------------ shared look

    public static readonly FontFamily UiFont = new("Segoe UI Variable Text, Segoe UI");
    public static readonly FontFamily IconFont = new("Segoe Fluent Icons, Segoe MDL2 Assets");
    public static readonly Color Accent = Color.FromRgb(0x0A, 0x84, 0xFF);

    public static TextBlock Label(string s, double size, byte alpha = 0xF2, FontWeight? weight = null) => new()
    {
        Text = s,
        FontFamily = UiFont,
        FontSize = size,
        Foreground = new SolidColorBrush(Color.FromArgb(alpha, 255, 255, 255)),
        FontWeight = weight ?? FontWeights.Normal,
        VerticalAlignment = VerticalAlignment.Center,
        TextTrimming = TextTrimming.CharacterEllipsis,
    };

    public static TextBlock Icon(string glyph, double size, Color? color = null) => new()
    {
        Text = glyph,
        FontFamily = IconFont,
        FontSize = size,
        Foreground = new SolidColorBrush(color ?? Colors.White),
        VerticalAlignment = VerticalAlignment.Center,
        HorizontalAlignment = HorizontalAlignment.Center,
    };

    public static Border Divider() => new()
    {
        Height = 1,
        Margin = new Thickness(0, 8, 0, 8),
        Background = new SolidColorBrush(Color.FromArgb(0x1A, 255, 255, 255)),
    };

    /// <summary>A row that lights up under the mouse, like the items in macOS menu bar extras.</summary>
    public static Border Row(UIElement content, Action? click, double radius = 8)
    {
        var b = new Border
        {
            Child = content,
            CornerRadius = new CornerRadius(radius),
            Padding = new Thickness(8, 6, 8, 6),
            Background = Brushes.Transparent,
            Cursor = click != null ? Cursors.Hand : null,
        };
        if (click != null)
        {
            var hover = new SolidColorBrush(Color.FromArgb(0x1C, 255, 255, 255));
            b.MouseEnter += (_, _) => b.Background = hover;
            b.MouseLeave += (_, _) => b.Background = Brushes.Transparent;
            b.MouseLeftButtonUp += (_, _) => click();
        }
        return b;
    }

    /// <summary>A macOS switch. <paramref name="changed"/> gets the new state.</summary>
    public static Border Switch(bool on, Action<bool> changed)
    {
        var knob = new Border
        {
            Width = 16, Height = 16, CornerRadius = new CornerRadius(8), Background = Brushes.White,
            HorizontalAlignment = on ? HorizontalAlignment.Right : HorizontalAlignment.Left,
            Margin = new Thickness(2),
            Effect = new DropShadowEffect { BlurRadius = 3, ShadowDepth = 1, Opacity = 0.3 },
        };
        var track = new Border
        {
            Width = 36, Height = 20, CornerRadius = new CornerRadius(10),
            Background = new SolidColorBrush(on ? Accent : Color.FromArgb(0x40, 255, 255, 255)),
            Child = knob, Cursor = Cursors.Hand, VerticalAlignment = VerticalAlignment.Center,
            Tag = on,
        };
        track.MouseLeftButtonUp += (_, e) =>
        {
            bool now = !(bool)track.Tag;
            SetSwitch(track, now);
            changed(now);
            e.Handled = true;
        };
        return track;
    }

    public static void SetSwitch(Border track, bool on)
    {
        track.Tag = on;
        track.Background = new SolidColorBrush(on ? Accent : Color.FromArgb(0x40, 255, 255, 255));
        ((Border)track.Child).HorizontalAlignment = on ? HorizontalAlignment.Right : HorizontalAlignment.Left;
    }
}
