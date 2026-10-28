using System;
using System.Windows;
using System.Windows.Media;
using System.Windows.Media.Effects;
using System.Windows.Media.Imaging;

namespace MacDock;

/// <summary>
/// macOS Tahoe look for Windows icons: every icon becomes the same squircle tile. Icons that already are a
/// square tile are trimmed and masked to the squircle; any other shape (circles, gears, folders) sits at ~70%
/// on a light tile, the way Tahoe frames legacy icons.
/// </summary>
static class IconTile
{
    const int Canvas = 256;
    const double Body = 226;       // tile size inside the canvas, leaving room for the shadow
    const double Glyph = 0.70;     // a non-tile icon's longest side, relative to the tile

    static readonly Geometry squircle = Squircle(new Rect((Canvas - Body) / 2, (Canvas - Body) / 2 - 3, Body, Body));

    /// <summary>Superellipse |x|^5 + |y|^5 = 1: close to Apple's continuous-corner icon shape.</summary>
    static Geometry Squircle(Rect r)
    {
        const int N = 160;
        const double n = 5;
        double cx = r.X + r.Width / 2, cy = r.Y + r.Height / 2, a = r.Width / 2, b = r.Height / 2;
        var fig = new PathFigure { IsClosed = true, IsFilled = true };
        for (int i = 0; i < N; i++)
        {
            double t = 2 * Math.PI * i / N, c = Math.Cos(t), s = Math.Sin(t);
            var p = new Point(cx + a * Math.Sign(c) * Math.Pow(Math.Abs(c), 2 / n), cy + b * Math.Sign(s) * Math.Pow(Math.Abs(s), 2 / n));
            if (i == 0) fig.StartPoint = p; else fig.Segments.Add(new LineSegment(p, true));
        }
        var g = new PathGeometry(new[] { fig });
        g.Freeze();
        return g;
    }

    // Shell.Icon caches its images, so the same source comes back for an app that restarts
    static readonly System.Runtime.CompilerServices.ConditionalWeakTable<ImageSource, ImageSource> made = new();

    public static ImageSource? Make(ImageSource? src)
    {
        if (src is not BitmapSource bmp) return src;
        if (made.TryGetValue(src, out var hit)) return hit;
        var tiled = Build(bmp);
        made.AddOrUpdate(src, tiled);
        return tiled;
    }

    static ImageSource Build(BitmapSource bmp)
    {
        var src = bmp;
        try
        {
            var px = new FormatConvertedBitmap(bmp, PixelFormats.Pbgra32, null, 0);
            int w = px.PixelWidth, h = px.PixelHeight;
            var buf = new byte[w * h * 4];
            px.CopyPixels(buf, w * 4, 0);

            // bounding box of the visible pixels, and how much of it is solid
            int l = w, t = h, r = -1, btm = -1;
            for (int y = 0; y < h; y++)
                for (int x = 0; x < w; x++)
                    if (buf[(y * w + x) * 4 + 3] > 40) { l = Math.Min(l, x); r = Math.Max(r, x); t = Math.Min(t, y); btm = Math.Max(btm, y); }
            if (r < 0) return src;
            int bw = r - l + 1, bh = btm - t + 1, solid = 0;
            for (int y = t; y <= btm; y++)
                for (int x = l; x <= r; x++)
                    if (buf[(y * w + x) * 4 + 3] > 200) solid++;
            double fill = (double)solid / (bw * bh);
            bool square = Math.Abs(bw - bh) <= 0.08 * Math.Max(bw, bh);
            // a rounded square covers ~95% of its box, a circle 78.5%
            bool isTile = square && fill > 0.88;

            var crop = new CroppedBitmap(bmp, new Int32Rect(l, t, bw, bh));
            var tile = squircle.Bounds;
            var dv = new DrawingVisual { Effect = new DropShadowEffect { BlurRadius = 10, ShadowDepth = 3, Direction = 270, Opacity = 0.35, Color = Colors.Black } };
            RenderOptions.SetBitmapScalingMode(dv, BitmapScalingMode.HighQuality);
            using (var dc = dv.RenderOpen())
            {
                if (isTile)
                {
                    dc.PushClip(squircle);
                    // a hair larger than the tile so the icon's own rounded corners fall outside the mask
                    var over = tile; over.Inflate(Body * 0.02, Body * 0.02);
                    dc.DrawImage(crop, over);
                    dc.Pop();
                }
                else
                {
                    var bg = new LinearGradientBrush(Color.FromRgb(0xFF, 0xFF, 0xFF), Color.FromRgb(0xE6, 0xE7, 0xEC), 90);
                    dc.DrawGeometry(bg, null, squircle);
                    double k = Body * Glyph / Math.Max(bw, bh);
                    double gw = bw * k, gh = bh * k;
                    dc.DrawImage(crop, new Rect(tile.X + (tile.Width - gw) / 2, tile.Y + (tile.Height - gh) / 2, gw, gh));
                }
                // thin light rim, like the glass edge on Tahoe icons
                dc.DrawGeometry(null, new Pen(new SolidColorBrush(Color.FromArgb(0x40, 0xFF, 0xFF, 0xFF)), 2), squircle);
            }
            var rtb = new RenderTargetBitmap(Canvas, Canvas, 96, 96, PixelFormats.Pbgra32);
            rtb.Render(dv);
            rtb.Freeze();
            return rtb;
        }
        catch { return src; }
    }
}
