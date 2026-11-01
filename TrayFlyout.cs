using System;
using System.Linq;
using System.Runtime.InteropServices;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Input;
using System.Windows.Media;
using ManagedShell.WindowsTray;

namespace MacDock;

/// <summary>
/// The ^ item: every notification-area icon (apps running in the background) in a grid. Left click and right
/// click go to the app exactly like clicks on the Windows tray, so Discord opens, Steam shows its menu, etc.
/// </summary>
class TrayFlyout : Flyout
{
    const double Cell = 44;
    readonly WrapPanel grid = new() { Margin = new Thickness(-2, 4, -2, 0) };
    readonly TextBlock count;

    [DllImport("user32.dll")] static extern bool GetCursorPos(out POINT p);
    [DllImport("user32.dll")] static extern uint GetDoubleClickTime();
    [StructLayout(LayoutKind.Sequential)] struct POINT { public int X, Y; }

    public TrayFlyout() : base(5 * Cell + 2 * 14 + 2 * 13 + 4)
    {
        Title = "MacDock Background Apps";
        count = Label("", 11.5, 0x80);
        var header = new DockPanel { Margin = new Thickness(2, 0, 2, 0) };
        DockPanel.SetDock(count, Dock.Right);
        header.Children.Add(count);
        header.Children.Add(Label("Background apps", 13, 0xF2, FontWeights.SemiBold));

        var stack = new StackPanel();
        stack.Children.Add(header);
        stack.Children.Add(grid);
        Panel.Child = stack;

        // the icons only come in while we host the tray (see Tray): start it now, apps answer TaskbarCreated
        // within a moment and fill the grid live
        Tray.Start();
        openCount++;
        opened = DateTime.Now;
        Fill();
        if (Tray.Icons is { } icons)
        {
            icons.CollectionChanged += OnChanged;
            Closed += (_, _) => icons.CollectionChanged -= OnChanged;
        }
        // after the input that closed us is handled; it gives every icon back to Explorer
        // (unless a new flyout opened meanwhile and still needs it)
        Closed += (_, _) =>
        {
            openCount--;
            Dispatcher.BeginInvoke(() => { if (openCount == 0) Tray.Stop(); }, System.Windows.Threading.DispatcherPriority.Background);
        };
        var settle = new System.Windows.Threading.DispatcherTimer { Interval = TimeSpan.FromSeconds(1.5) };
        settle.Tick += (_, _) => { settle.Stop(); Fill(); };
        settle.Start();
    }

    readonly DateTime opened;
    static int openCount;

    void OnChanged(object? s, System.Collections.Specialized.NotifyCollectionChangedEventArgs e) => Dispatcher.BeginInvoke(Fill);

    void Fill()
    {
        grid.Children.Clear();
        var icons = Tray.Icons?.Where(i => i.Icon != null && !i.IsHidden).ToList();
        if (icons == null || icons.Count == 0)
        {
            count.Text = "";
            grid.Children.Add(new TextBlock
            {
                Text = Tray.Icons == null ? "The notification area could not be hosted."
                    : DateTime.Now - opened < TimeSpan.FromSeconds(1.4) ? "Loading…" : "Nothing running in the background.",
                Foreground = new SolidColorBrush(Color.FromArgb(0x80, 255, 255, 255)),
                FontFamily = UiFont, FontSize = 12, Margin = new Thickness(4, 6, 4, 2),
            });
            return;
        }
        count.Text = icons.Count.ToString();
        foreach (var icon in icons) grid.Children.Add(Cell_(icon));
    }

    FrameworkElement Cell_(NotifyIcon icon)
    {
        var img = new Image { Source = icon.Icon, Width = 20, Height = 20, Stretch = Stretch.Uniform };
        RenderOptions.SetBitmapScalingMode(img, BitmapScalingMode.HighQuality);
        var b = new Border
        {
            Width = Cell, Height = Cell - 6,
            CornerRadius = new CornerRadius(9),
            Background = Brushes.Transparent,
            Child = img,
            ToolTip = !string.IsNullOrWhiteSpace(icon.Title) ? icon.Title.Trim()
                : !string.IsNullOrEmpty(icon.Path) ? Shell.FriendlyName(icon.Path, null, System.IO.Path.GetFileNameWithoutExtension(icon.Path))
                : null,
        };
        var hover = new SolidColorBrush(Color.FromArgb(0x24, 255, 255, 255));
        b.MouseEnter += (_, _) =>
        {
            b.Background = hover;
            Place(icon, b);
            icon.IconMouseEnter(CursorParam());
        };
        b.MouseLeave += (_, _) => { b.Background = Brushes.Transparent; icon.IconMouseLeave(CursorParam()); };
        b.MouseMove += (_, _) => icon.IconMouseMove(CursorParam());
        b.MouseDown += (_, e) =>
        {
            Place(icon, b);
            icon.IconMouseDown(e.ChangedButton, CursorParam(), (int)GetDoubleClickTime());
            e.Handled = true;
        };
        b.MouseUp += (_, e) =>
        {
            var button = e.ChangedButton;
            icon.IconMouseUp(button, CursorParam(), (int)GetDoubleClickTime());
            e.Handled = true;
            // a left click usually opens the app's window: get out of its way. A right click opens the app's
            // menu, which takes the foreground and so closes us anyway.
            if (button == MouseButton.Left) Dispatcher.BeginInvoke(Close);
        };
        return b;
    }

    /// <summary>Apps anchor their own popups (Win11 style flyouts, NIN_POPUPOPEN) to where the icon is.</summary>
    static void Place(NotifyIcon icon, FrameworkElement b)
    {
        try
        {
            var tl = b.PointToScreen(new Point(0, 0));
            var br = b.PointToScreen(new Point(b.ActualWidth, b.ActualHeight));
            icon.Placement = new ManagedShell.Interop.NativeMethods.Rect { Left = (int)tl.X, Top = (int)tl.Y, Right = (int)br.X, Bottom = (int)br.Y };
        }
        catch { }
    }

    static uint CursorParam()
    {
        GetCursorPos(out var p);
        return (uint)(((p.Y & 0xFFFF) << 16) | (p.X & 0xFFFF));
    }
}
