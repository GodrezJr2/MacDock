using System;
using System.Windows;
using System.Windows.Media;

namespace MacDock;

/// <summary>
/// A macOS-style battery drawn by hand: rounded outline, a fill that follows the charge, the terminal nub and a
/// bolt while plugged in. Scales to any size, so the menu bar and the flyout share it.
/// </summary>
class BatteryGlyph : FrameworkElement
{
    int percent = 100;
    bool plugged, charging;

    public void Set(int pct, bool onAc, bool isCharging)
    {
        pct = Math.Clamp(pct, 0, 100);
        if (pct == percent && onAc == plugged && isCharging == charging) return;
        percent = pct; plugged = onAc; charging = isCharging;
        InvalidateVisual();
    }

    protected override Size MeasureOverride(Size available) => new(Width, Height);

    protected override void OnRender(DrawingContext dc)
    {
        double w = ActualWidth, h = ActualHeight;
        if (w <= 0 || h <= 0) return;
        double nubW = Math.Max(1.5, w * 0.07);
        double bodyW = w - nubW - w * 0.03;
        double stroke = Math.Max(1, h * 0.085);
        double r = h * 0.3;

        var outline = new Pen(new SolidColorBrush(Color.FromArgb(0x8C, 255, 255, 255)), stroke);
        var body = new Rect(stroke / 2, stroke / 2, bodyW - stroke, h - stroke);
        dc.DrawRoundedRectangle(null, outline, body, r, r);

        double nubH = h * 0.36;
        dc.DrawRoundedRectangle(new SolidColorBrush(Color.FromArgb(0x8C, 255, 255, 255)), null,
            new Rect(w - nubW, (h - nubH) / 2, nubW, nubH), nubW / 2, nubW / 2);

        double inset = stroke + Math.Max(1, h * 0.09);
        var inner = new Rect(inset, inset, bodyW - 2 * inset, h - 2 * inset);
        double fillW = Math.Max(percent > 0 ? inner.Height * 0.5 : 0, inner.Width * percent / 100.0);
        var color = charging ? Color.FromRgb(0x34, 0xC7, 0x59)
            : percent <= 10 && !plugged ? Color.FromRgb(0xFF, 0x45, 0x3A)
            : percent <= 20 && !plugged ? Color.FromRgb(0xFF, 0x9F, 0x0A)
            : Colors.White;
        double ir = Math.Max(0.5, r - inset + stroke / 2);
        if (fillW > 0)
            dc.DrawRoundedRectangle(new SolidColorBrush(color), null, new Rect(inner.X, inner.Y, fillW, inner.Height), ir, ir);

        if (plugged)
        {
            // bolt, centred on the body, with a dark rim so it reads over both the fill and the empty part
            double bh = h * 0.86, bw = bh * 0.56;
            double cx = body.X + body.Width / 2, cy = h / 2;
            var g = new StreamGeometry();
            using (var c = g.Open())
            {
                c.BeginFigure(new Point(cx + bw * 0.12, cy - bh / 2), true, true);
                c.LineTo(new Point(cx - bw * 0.50, cy + bh * 0.08), true, false);
                c.LineTo(new Point(cx - bw * 0.02, cy + bh * 0.08), true, false);
                c.LineTo(new Point(cx - bw * 0.12, cy + bh / 2), true, false);
                c.LineTo(new Point(cx + bw * 0.50, cy - bh * 0.08), true, false);
                c.LineTo(new Point(cx + bw * 0.02, cy - bh * 0.08), true, false);
            }
            g.Freeze();
            var rim = new Pen(new SolidColorBrush(Color.FromArgb(0xA0, 0, 0, 0)), Math.Max(1.2, h * 0.14)) { LineJoin = PenLineJoin.Round };
            dc.DrawGeometry(null, rim, g);
            dc.DrawGeometry(Brushes.White, null, g);
        }
    }
}
