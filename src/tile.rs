//! macOS Tahoe look for Windows icons (port of IconTile.cs): every icon becomes the same squircle tile.
//! Icons that already are a square tile are trimmed and masked to the squircle; any other shape (circles,
//! gears, folders) sits at ~70% on a light tile, the way Tahoe frames legacy icons.

use tiny_skia::{
    Color, FillRule, FilterQuality, GradientStop, LinearGradient, Mask, Paint, Path, PathBuilder, Pixmap, PixmapPaint,
    Point, SpreadMode, Stroke, Transform,
};

const CANVAS: f32 = 256.0;
const BODY: f32 = 226.0; // tile size inside the canvas, leaving room for the shadow
const GLYPH: f32 = 0.70; // a non-tile icon's longest side, relative to the tile

/// Superellipse |x|^5 + |y|^5 = 1: close to Apple's continuous-corner icon shape.
fn squircle(x: f32, y: f32, w: f32, h: f32) -> Option<Path> {
    const N: usize = 160;
    const E: f64 = 5.0;
    let (cx, cy, a, b) = ((x + w / 2.0) as f64, (y + h / 2.0) as f64, (w / 2.0) as f64, (h / 2.0) as f64);
    let mut p = PathBuilder::new();
    for i in 0..N {
        let t = 2.0 * std::f64::consts::PI * i as f64 / N as f64;
        let (c, s) = (t.cos(), t.sin());
        let px = cx + a * c.signum() * c.abs().powf(2.0 / E);
        let py = cy + b * s.signum() * s.abs().powf(2.0 / E);
        if i == 0 {
            p.move_to(px as f32, py as f32);
        } else {
            p.line_to(px as f32, py as f32);
        }
    }
    p.close();
    p.finish()
}

/// The tile for an icon at `out` px (the 256 canvas scaled down). Input and output are premultiplied BGRA.
pub fn make(src: &Pixmap, out: u32) -> Option<Pixmap> {
    let (w, h) = (src.width() as usize, src.height() as usize);
    let data = src.data();
    // bounding box of the visible pixels, and how much of it is solid
    let (mut l, mut t, mut r, mut btm) = (w, h, 0usize, 0usize);
    let mut any = false;
    for y in 0..h {
        for x in 0..w {
            if data[(y * w + x) * 4 + 3] > 40 {
                any = true;
                l = l.min(x);
                r = r.max(x);
                t = t.min(y);
                btm = btm.max(y);
            }
        }
    }
    if !any {
        return None;
    }
    let (bw, bh) = (r - l + 1, btm - t + 1);
    let mut solid = 0usize;
    for y in t..=btm {
        for x in l..=r {
            if data[(y * w + x) * 4 + 3] > 200 {
                solid += 1;
            }
        }
    }
    let fill = solid as f64 / (bw * bh) as f64;
    let square = (bw as f64 - bh as f64).abs() <= 0.08 * bw.max(bh) as f64;
    // a rounded square covers ~95% of its box, a circle 78.5%
    let is_tile = square && fill > 0.88;

    let k = out as f32 / CANVAS;
    let base = Transform::from_scale(k, k);
    let (tx, ty) = ((CANVAS - BODY) / 2.0, (CANVAS - BODY) / 2.0 - 3.0);
    let shape = squircle(tx, ty, BODY, BODY)?;
    let mut pm = Pixmap::new(out, out)?;

    // drop shadow: blur 10, 3 px down, 35% black (in canvas units, scaled)
    if let Some(scaled) = shape.clone().transform(base.pre_translate(0.0, 3.0)) {
        if let Some((sh, ox, oy)) = crate::gfx::shadow(&scaled, 10.0 * k, (0, 0, 0, 0x5A)) {
            pm.draw_pixmap(ox.round() as i32, oy.round() as i32, sh.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
        }
    }

    let paint_img = PixmapPaint { quality: FilterQuality::Bicubic, ..Default::default() };
    // the cropped icon as its own pixmap so it can be scaled from (0, 0)
    let mut crop = Pixmap::new(bw as u32, bh as u32)?;
    crop.draw_pixmap(-(l as i32), -(t as i32), src.as_ref(), &PixmapPaint::default(), Transform::identity(), None);

    if is_tile {
        // a hair larger than the tile so the icon's own rounded corners fall outside the mask
        let over = BODY * 0.02;
        let (ox, oy, ow, oh) = (tx - over, ty - over, BODY + over * 2.0, BODY + over * 2.0);
        let mut mask = Mask::new(out, out)?;
        mask.fill_path(&shape, FillRule::Winding, true, base);
        let tf = base.pre_translate(ox, oy).pre_scale(ow / bw as f32, oh / bh as f32);
        pm.draw_pixmap(0, 0, crop.as_ref(), &paint_img, tf, Some(&mask));
    } else {
        let mut paint = Paint::default();
        paint.anti_alias = true;
        // white → #E6E7EC top to bottom (colours are BGRA)
        paint.shader = LinearGradient::new(
            Point::from_xy(0.0, ty), Point::from_xy(0.0, ty + BODY),
            vec![GradientStop::new(0.0, Color::from_rgba8(255, 255, 255, 255)), GradientStop::new(1.0, Color::from_rgba8(0xEC, 0xE7, 0xE6, 255))],
            SpreadMode::Pad, Transform::identity(),
        )?;
        pm.fill_path(&shape, &paint, FillRule::Winding, base, None);
        let s = BODY * GLYPH / bw.max(bh) as f32;
        let (gw, gh) = (bw as f32 * s, bh as f32 * s);
        let tf = base.pre_translate(tx + (BODY - gw) / 2.0, ty + (BODY - gh) / 2.0).pre_scale(s, s);
        pm.draw_pixmap(0, 0, crop.as_ref(), &paint_img, tf, None);
    }
    // thin light rim, like the glass edge on Tahoe icons
    let mut rim = Paint::default();
    rim.anti_alias = true;
    rim.set_color(Color::from_rgba8(255, 255, 255, 0x40));
    pm.stroke_path(&shape, &rim, &Stroke { width: 2.0, ..Default::default() }, base, None);
    Some(pm)
}
