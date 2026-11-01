using System;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Interop;
using System.Windows.Media;
using System.Windows.Media.Effects;
using System.Windows.Threading;
using static MacDock.Native;

namespace MacDock;

/// <summary>
/// The flyout behind the menu bar's battery item: charge, then the power flow — adapter input, total system
/// draw and the battery's share — with a CPU / GPU / rest breakdown. See <see cref="PowerMeter"/> for what is
/// measured and what is estimated.
/// </summary>
class BatteryPopup : Flyout
{
    static readonly Color Green = Color.FromRgb(0x34, 0xC7, 0x59);
    static readonly Color Amber = Color.FromRgb(0xFF, 0x9F, 0x0A);
    static readonly Color CpuColor = Color.FromRgb(0x0A, 0x84, 0xFF);
    static readonly Color GpuColor = Color.FromRgb(0x30, 0xD1, 0x58);
    static readonly Color RestColor = Color.FromArgb(0x70, 255, 255, 255);

    readonly PowerMeter meter;
    readonly bool ownsMeter;
    readonly BatteryGlyph glyph = new() { Width = 46, Height = 22, VerticalAlignment = VerticalAlignment.Center };
    readonly TextBlock statusText, pctText, timeText, capText, noteText;
    readonly TextBlock dcValue, dcSub, totalValue, totalSub, battLabel, battValue, battSub;
    readonly TextBlock cpuLegend, gpuLegend, restLegend;
    readonly ColumnDefinition cpuCol = new(), gpuCol = new(), restCol = new();
    readonly Border chip;
    readonly DispatcherTimer timer = new() { Interval = TimeSpan.FromSeconds(1) };

    public BatteryPopup(PowerMeter? shared = null) : base(330)
    {
        meter = shared ?? new PowerMeter();
        ownsMeter = shared == null;

        Title = "MacDock Battery";

        // ---- header
        statusText = Text("", 11, 0xE6, FontWeights.SemiBold);
        chip = new Border
        {
            CornerRadius = new CornerRadius(9),
            Padding = new Thickness(8, 2, 8, 3),
            VerticalAlignment = VerticalAlignment.Center,
            Child = statusText,
        };
        var header = new DockPanel();
        DockPanel.SetDock(chip, Dock.Right);
        header.Children.Add(chip);
        header.Children.Add(Text("Battery", 13.5, 0xF2, FontWeights.SemiBold));

        // ---- hero: glyph, big percentage, time line
        pctText = Text("—", 34, 0xFF, FontWeights.SemiBold);
        var pctSign = Text("%", 16, 0x8C, FontWeights.Medium);
        pctSign.VerticalAlignment = VerticalAlignment.Bottom;
        pctSign.Margin = new Thickness(2, 0, 0, 7);
        var pctRow = new StackPanel { Orientation = Orientation.Horizontal };
        pctRow.Children.Add(pctText);
        pctRow.Children.Add(pctSign);
        timeText = Text("", 12, 0x99);
        timeText.Margin = new Thickness(0, -3, 0, 0);
        var heroText = new StackPanel { Margin = new Thickness(14, 0, 0, 0), VerticalAlignment = VerticalAlignment.Center };
        heroText.Children.Add(pctRow);
        heroText.Children.Add(timeText);
        var hero = new StackPanel { Orientation = Orientation.Horizontal, Margin = new Thickness(2, 10, 0, 0) };
        hero.Children.Add(glyph);
        hero.Children.Add(heroText);

        // ---- power flow tiles
        dcValue = Big(); dcSub = Small();
        totalValue = Big(); totalSub = Small();
        battValue = Big(); battSub = Small();
        battLabel = Text("From battery", 11, 0x99, FontWeights.Medium);
        var tiles = new Grid { Margin = new Thickness(0, 14, 0, 0) };
        for (int i = 0; i < 3; i++)
        {
            tiles.ColumnDefinitions.Add(new ColumnDefinition());
            if (i < 2) tiles.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(6) });
        }
        AddTile(tiles, 0, Text("DC in", 11, 0x99, FontWeights.Medium), dcValue, dcSub);
        AddTile(tiles, 2, Text("Total draw", 11, 0x99, FontWeights.Medium), totalValue, totalSub);
        AddTile(tiles, 4, battLabel, battValue, battSub);

        // ---- breakdown bar
        var bar = new Grid { Height = 6, Margin = new Thickness(0, 14, 0, 0), ClipToBounds = true };
        bar.ColumnDefinitions.Add(cpuCol);
        bar.ColumnDefinitions.Add(gpuCol);
        bar.ColumnDefinitions.Add(restCol);
        bar.Children.Add(Segment(CpuColor, 0));
        bar.Children.Add(Segment(GpuColor, 1));
        bar.Children.Add(Segment(RestColor, 2));
        var barShell = new Border { Margin = new Thickness(0, 14, 0, 0), Child = bar };
        bar.Margin = new Thickness(0);
        barShell.SizeChanged += (_, e) => barShell.Clip = new RectangleGeometry(new Rect(e.NewSize), 3, 3);

        cpuLegend = Small(); gpuLegend = Small(); restLegend = Small();
        var legend = new StackPanel { Orientation = Orientation.Horizontal, Margin = new Thickness(0, 7, 0, 0) };
        legend.Children.Add(Legend(CpuColor, cpuLegend));
        legend.Children.Add(Legend(GpuColor, gpuLegend));
        legend.Children.Add(Legend(RestColor, restLegend));

        capText = Text("", 11.5, 0x80);
        noteText = Text("", 10.5, 0x66);
        noteText.TextWrapping = TextWrapping.Wrap;
        noteText.Margin = new Thickness(0, 6, 0, 0);

        var stack = new StackPanel();
        stack.Children.Add(header);
        stack.Children.Add(hero);
        stack.Children.Add(tiles);
        stack.Children.Add(barShell);
        stack.Children.Add(legend);
        stack.Children.Add(Divider());
        stack.Children.Add(capText);
        stack.Children.Add(noteText);

        Panel.Padding = new Thickness(16, 14, 16, 14);
        Panel.Child = stack;

        if (meter.Recent is PowerMeter.Sample recent) Apply(recent);
        Loaded += (_, _) => Refresh();
        timer.Tick += (_, _) => Refresh();
        timer.Start();
        Closed += (_, _) => { closed = true; timer.Stop(); if (ownsMeter && !reading) meter.Dispose(); };
    }

    bool reading, closed;

    /// <summary>The sensors block for up to half a second (NVML waking the dGPU), so they are read off the UI thread.</summary>
    async void Refresh()
    {
        if (reading || closed) return;
        reading = true;
        try
        {
            var s = await System.Threading.Tasks.Task.Run(meter.Read);
            if (!closed) Apply(s);
        }
        catch { }
        finally
        {
            reading = false;
            if (closed && ownsMeter) meter.Dispose();
        }
    }

    void Apply(PowerMeter.Sample s)
    {
        var r = s.Battery;

        (string status, Color tint) = !r.Present ? ("No battery", RestColor)
            : r.Charging ? ("Charging", Green)
            : r.OnAc ? ("Plugged in", Green)
            : r.Percent <= 20 ? ("Low", Amber)
            : ("On battery", Color.FromRgb(0x8E, 0x8E, 0x93));
        statusText.Text = status;
        statusText.Foreground = new SolidColorBrush(tint);
        chip.Background = new SolidColorBrush(Color.FromArgb(0x26, tint.R, tint.G, tint.B));

        pctText.Text = r.Present ? r.Percent.ToString() : "—";
        timeText.Text = TimeLine(r);
        glyph.Set(r.Percent, r.OnAc, r.Charging);

        string est = s.TotalMeasured ? "" : "≈";
        if (r.OnAc || !r.Present)
        {
            dcValue.Text = est + W(s.DcInW);
            dcSub.Text = "adapter";
        }
        else
        {
            dcValue.Text = "0 W";
            dcSub.Text = "unplugged";
        }
        totalValue.Text = est + W(s.TotalW);
        totalSub.Text = s.TotalMeasured ? "measured" : "estimated";

        if (r.Charging)
        {
            battLabel.Text = "Into battery";
            battValue.Text = W(s.BatteryFlowW);
            battValue.Foreground = new SolidColorBrush(Green);
            battSub.Text = "charging";
        }
        else
        {
            battLabel.Text = "From battery";
            battValue.Text = W(Math.Max(0, -s.BatteryFlowW));
            battValue.Foreground = new SolidColorBrush(Color.FromArgb(0xF2, 255, 255, 255));
            battSub.Text = r.Discharging ? "discharging" : r.OnAc ? "idle" : "—";
        }

        double cpu = s.CpuW ?? 0, gpu = s.GpuW ?? 0;
        double rest = Math.Max(0, s.TotalMeasured ? s.TotalW - cpu - gpu : s.RestW);
        double sum = Math.Max(0.1, cpu + gpu + rest);
        cpuCol.Width = new GridLength(cpu / sum, GridUnitType.Star);
        gpuCol.Width = new GridLength(gpu / sum, GridUnitType.Star);
        restCol.Width = new GridLength(rest / sum, GridUnitType.Star);
        cpuLegend.Text = s.CpuW is null ? "CPU —" : $"CPU {cpu:0} W";
        gpuLegend.Text = s.GpuAsleep ? "GPU off" : s.GpuW is null ? "GPU —" : $"GPU {gpu:0} W";
        restLegend.Text = $"Rest {(s.TotalMeasured ? "" : "~")}{rest:0} W";

        capText.Text = r.FullWh > 0
            ? string.Create(Inv, $"{r.CapacityWh:0.0} / {r.FullWh:0.0} Wh") + (r.WearPercent >= 0 ? $"  ·  {r.WearPercent}% wear" : "") + (r.Cycles > 0 ? $"  ·  {r.Cycles} cycles" : "")
            : "";
        noteText.Text = s.TotalMeasured
            ? "Total draw measured at the battery. Rest = everything besides CPU and GPU (screen, RAM, SSD, fans)."
            : s.RestLearned
                ? "On the adapter nothing measures the input, so it is CPU + GPU sensors plus the rest, learned while on battery."
                : "Estimated: CPU + GPU sensors plus a default for the rest. Run on battery with this open for a minute to calibrate.";
    }

    static string TimeLine(Battery.Reading r)
    {
        if (!r.Present) return "";
        if (r.Charging && r.Watts > 0.5 && r.FullMwh > r.CapacityMwh)
            return Duration((r.FullMwh - r.CapacityMwh) / 1000.0 / r.Watts) + " until full";
        if (!r.OnAc && r.SecondsLeft > 0) return Duration(r.SecondsLeft / 3600.0) + " remaining";
        if (r.Charging) return "Charging…";
        if (r.OnAc) return r.Percent >= 95 ? "Fully charged" : "Holding charge";
        return "Estimating…";
    }

    static string Duration(double hours)
    {
        int total = (int)Math.Round(hours * 60);
        int h = total / 60, m = total % 60;
        return h > 0 ? $"{h} h {m:00} min" : $"{m} min";
    }

    static string W(double w) => (w < 9.95 ? w.ToString("0.0", Inv) : w.ToString("0", Inv)) + " W";
    static readonly System.Globalization.CultureInfo Inv = System.Globalization.CultureInfo.InvariantCulture;

    static void AddTile(Grid grid, int col, TextBlock label, TextBlock value, TextBlock sub)
    {
        var sp = new StackPanel();
        sp.Children.Add(label);
        value.Margin = new Thickness(0, 3, 0, 0);
        sp.Children.Add(value);
        sp.Children.Add(sub);
        var tile = new Border
        {
            CornerRadius = new CornerRadius(10),
            Background = new SolidColorBrush(Color.FromArgb(0x10, 255, 255, 255)),
            Padding = new Thickness(10, 8, 10, 9),
            Child = sp,
        };
        Grid.SetColumn(tile, col);
        grid.Children.Add(tile);
    }

    static Border Segment(Color c, int col)
    {
        var b = new Border { Background = new SolidColorBrush(c), Margin = new Thickness(0, 0, col < 2 ? 1.5 : 0, 0) };
        Grid.SetColumn(b, col);
        return b;
    }

    static StackPanel Legend(Color c, TextBlock label)
    {
        var sp = new StackPanel { Orientation = Orientation.Horizontal, Margin = new Thickness(0, 0, 14, 0) };
        sp.Children.Add(new Border
        {
            Width = 7, Height = 7, CornerRadius = new CornerRadius(3.5),
            Background = new SolidColorBrush(c), VerticalAlignment = VerticalAlignment.Center,
            Margin = new Thickness(0, 1, 5, 0),
        });
        sp.Children.Add(label);
        return sp;
    }

    static TextBlock Big() => Text("—", 19, 0xF2, FontWeights.SemiBold);
    static TextBlock Small() => Text("", 10.5, 0x80);

    new static Border Divider() => new()
    {
        Height = 1,
        Margin = new Thickness(0, 12, 0, 10),
        Background = new SolidColorBrush(Color.FromArgb(0x1A, 255, 255, 255)),
    };

    static TextBlock Text(string s, double size, byte alpha, FontWeight? weight = null) => new()
    {
        Text = s,
        FontFamily = new FontFamily("Segoe UI Variable Text, Segoe UI"),
        FontSize = size,
        Foreground = new SolidColorBrush(Color.FromArgb(alpha, 255, 255, 255)),
        FontWeight = weight ?? FontWeights.Normal,
        VerticalAlignment = VerticalAlignment.Center,
    };
}
