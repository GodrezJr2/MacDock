//! The AI usage extra: how fast Claude Code and Codex are burning tokens (5 / 30 min), the last 24 hours,
//! and each provider's limits with the points used per 5 / 30 min and where the current pace ends up.
//! Data comes from ai_usage.rs (local logs only).

use std::sync::Arc;

use crate::ai_usage::{self, compact, model_name, span, Limit, Provider, Session, Summary};
use crate::flyout::{white, Cmd, Content, Ui};

pub const CLAUDE: (u8, u8, u8) = (0xE8, 0x8A, 0x5E);
pub const CODEX: (u8, u8, u8) = (0x4C, 0xC9, 0xB0);
const GREEN: (u8, u8, u8) = (0x34, 0xC7, 0x59);
const AMBER: (u8, u8, u8) = (0xFF, 0x9F, 0x0A);
const RED: (u8, u8, u8) = (0xFF, 0x45, 0x3A);
const GREY: (u8, u8, u8) = (0x8E, 0x8E, 0x93);

/// Bar / ring colour for a used percentage.
pub fn level(pct: f64) -> (u8, u8, u8) {
    if pct >= 90.0 {
        RED
    } else if pct >= 70.0 {
        AMBER
    } else {
        GREEN
    }
}

fn a((r, g, b): (u8, u8, u8), alpha: u8) -> (u8, u8, u8, u8) {
    (r, g, b, alpha)
}

fn rate(r: f64) -> String {
    if r <= 0.0 {
        "0".into()
    } else if r < 10.0 {
        format!("{r:.1}")
    } else {
        compact(r)
    }
}

fn pts(p: f64) -> String {
    if p <= 0.0 {
        "0%".into()
    } else if p < 10.0 {
        format!("+{p:.1}%")
    } else {
        format!("+{p:.0}%")
    }
}

fn ago(secs: i64) -> String {
    if secs < 60 {
        crate::i18n::t("active now").into()
    } else {
        crate::i18n::fmt!("{} ago", "{}前", span(secs))
    }
}

pub struct AiFly {
    sum: Option<Arc<Summary>>,
    gen: u64,
}

impl AiFly {
    pub fn new() -> AiFly {
        AiFly { sum: ai_usage::summary(), gen: ai_usage::generation() }
    }

    fn provider(&self, ui: &mut Ui, y: f32, p: &Provider, tint: (u8, u8, u8), now: i64) -> f32 {
        let w = ui.w;
        let x0 = 12.0;
        let iw = w - 24.0;
        let mut cy = y + 11.0;
        let title_h = ui.line_h(13.0, 600);

        // measure first (the card is drawn behind), then paint: two passes over the same layout
        let lab_h = ui.line_h(11.0, 500);
        let sub_h = ui.line_h(10.5, 400);
        let val_h = ui.line_h(16.0, 600);
        let note_h = if p.note.is_some() { lab_h + 6.0 } else { 0.0 };
        let limits_h: f32 = note_h + if p.limits.is_empty() {
            if p.block.is_some() { lab_h + 6.0 } else { 0.0 }
        } else {
            p.limits.iter().map(|l| lab_h + 5.0 + 5.0 + 6.0 + sub_h + if l.d30 > 0.0 { sub_h } else { 0.0 } + 10.0).sum()
        };
        let tiles_h = 7.0 + lab_h + 2.0 + val_h + 8.0;
        let card_h = 11.0 + title_h + 10.0 + limits_h + tiles_h + 12.0;
        ui.fill(0.0, y, w, card_h, 12.0, white(0x0E));

        // title row
        ui.circle(x0 + 4.0, cy + title_h / 2.0, 4.0, a(tint, 255));
        let tw = ui.text(x0 + 14.0, cy, p.name, 13.0, 600, white(0xF2), 0.0);
        if let Some(plan) = &p.plan {
            let pw = ui.text_w(plan, 10.0, 600) + 12.0;
            let px = x0 + 14.0 + tw + 8.0;
            ui.fill(px, cy + (title_h - 16.0) / 2.0, pw, 16.0, 8.0, white(0x14));
            ui.text_mid(px + 6.0, cy + title_h / 2.0, plan, 10.0, 600, white(0xA0), 0.0);
        }
        let status = if p.rate5() > 0.0 { crate::i18n::t("active now").to_string() } else if p.last > 0 { ago(now - p.last) } else { String::new() };
        let sw = ui.text_w(&status, 11.0, 400);
        let sc = if p.rate5() > 0.0 { a(GREEN, 255) } else { white(0x80) };
        ui.text_mid(x0 + iw - sw, cy + title_h / 2.0, &status, 11.0, 400, sc, 0.0);
        cy += title_h + 10.0;

        // limits
        if let Some(n) = &p.note {
            ui.text(x0, cy, &crate::i18n::fmt!("Limits: {n}", "额度：{n}"), 11.0, 500, a(AMBER, 255), iw);
            cy += lab_h + 6.0;
        }
        if p.limits.is_empty() {
            if let Some((start, tok)) = p.block {
                let t = crate::i18n::fmt!("5-hour block  ·  {} tokens  ·  ends in {}", "5 小时周期 · {} 词元 · {}后结束", compact(tok as f64), span(start + 5 * 3600 - now));
                ui.text(x0, cy, &t, 11.0, 500, white(0xB0), iw);
                cy += lab_h + 6.0;
            }
        }
        for l in &p.limits {
            cy = self.limit(ui, x0, cy, iw, l, now);
        }

        // tiles: 5 min, 30 min, today
        let gap = 6.0;
        let tw = (iw - gap * 2.0) / 3.0;
        let tiles = [
            (crate::i18n::t("5 min"), crate::i18n::fmt!("{} tok/s", "{} 词元/秒", rate(p.rate5()))),
            (crate::i18n::t("30 min"), crate::i18n::fmt!("{} tok/s", "{} 词元/秒", rate(p.rate30()))),
            (crate::i18n::t("Today"), crate::i18n::fmt!("{} tok", "{} 词元", compact(p.today as f64))),
        ];
        let th = tiles_h - 4.0;
        for (i, (lab, val)) in tiles.iter().enumerate() {
            let tx = x0 + i as f32 * (tw + gap);
            ui.fill(tx, cy, tw, th, 9.0, white(0x0C));
            ui.text(tx + 9.0, cy + 7.0, lab, 11.0, 500, white(0x90), tw - 14.0);
            let mut size = 16.0;
            while size > 10.0 && ui.text_w(val, size, 600) > tw - 12.0 {
                size -= 1.0;
            }
            ui.text(tx + 9.0, cy + 7.0 + lab_h + 2.0, val, size, 600, white(0xF2), tw - 12.0);
        }
        y + card_h
    }

    fn sessions(&self, ui: &mut Ui, mut y: f32, list: &[Session], now: i64) -> f32 {
        const MAX: usize = 4;
        let w = ui.w;
        let busy = list.iter().filter(|s| s.tok5 > 0).count();
        ui.text(0.0, y, crate::i18n::t("Sessions"), 12.0, 600, white(0xD0), 0.0);
        let head = if busy > 0 { crate::i18n::fmt!("{busy} working  ·  {} in 30 min", "{busy} 个进行中 · 半小时内共 {} 个", list.len()) } else { crate::i18n::fmt!("{} in 30 min", "半小时内共 {} 个", list.len()) };
        let hw = ui.text_w(&head, 11.0, 400);
        ui.text(w - hw, y + 1.0, &head, 11.0, 400, white(0x80), 0.0);
        y += ui.line_h(12.0, 600) + 6.0;
        let (t_h, s_h) = (ui.line_h(12.5, 600), ui.line_h(10.5, 400));
        let row_h = 7.0 + t_h + 1.0 + s_h + 7.0;
        for (i, se) in list.iter().take(MAX).enumerate() {
            let tint = if se.provider == "Codex" { CODEX } else { CLAUDE };
            ui.fill(0.0, y, w, row_h, 9.0, white(if i % 2 == 0 { 0x0E } else { 0x09 }));
            let cy = y + 7.0;
            // dot: provider colour, a halo while it burned tokens in the last 5 minutes
            let dx = 12.0;
            let dy = cy + t_h / 2.0;
            if se.tok5 > 0 {
                ui.circle(dx, dy, 6.5, a(tint, 0x40));
            }
            ui.circle(dx, dy, 4.0, a(tint, if se.tok5 > 0 { 255 } else { 0x90 }));
            let (rt, rc) = if se.tok5 > 0 {
                (crate::i18n::fmt!("{} tok/s", "{} 词元/秒", rate(se.rate5())), white(0xF2))
            } else {
                (crate::i18n::fmt!("idle {}", "已空闲{}", span(now - se.last)), white(0x70))
            };
            let rw = ui.text_w(&rt, 12.0, 600);
            ui.text(w - 10.0 - rw, cy, &rt, 12.0, 600, rc, 0.0);
            ui.text(24.0, cy, &se.title, 12.5, 600, white(0xEE), w - 24.0 - rw - 20.0);
            let mut sub = vec![];
            if !se.project.is_empty() && se.project != se.title {
                sub.push(se.project.clone());
            }
            if let Some(m) = &se.model {
                sub.push(model_name(m));
            }
            if let Some(b) = &se.branch {
                sub.push(b.clone());
            }
            sub.push(crate::i18n::fmt!("{} tok", "{} 词元", compact(se.total as f64)));
            ui.text(24.0, cy + t_h + 1.0, &sub.join("  ·  "), 10.5, 400, white(0x80), w - 34.0);
            y += row_h + 4.0;
        }
        if list.len() > MAX {
            let more = crate::i18n::fmt!("+{} more", "另有 {} 个", list.len() - MAX);
            ui.text(24.0, y, &more, 10.5, 400, white(0x70), 0.0);
            y += ui.line_h(10.5, 400) + 2.0;
        }
        y - 4.0
    }

    fn limit(&self, ui: &mut Ui, x0: f32, mut cy: f32, iw: f32, l: &Limit, now: i64) -> f32 {
        let lab_h = ui.line_h(11.0, 500);
        let sub_h = ui.line_h(10.5, 400);
        let c = level(l.used);
        ui.text(x0, cy, &crate::i18n::fmt!("{} limit", "{}额度", crate::i18n::t(&l.label)), 11.0, 500, white(0xB0), 0.0);
        let pct = format!("{:.0}%", l.used);
        let pw = ui.text_w(&pct, 11.0, 600);
        ui.text(x0 + iw - pw, cy, &pct, 11.0, 600, a(c, 255), 0.0);
        cy += lab_h + 5.0;
        ui.fill(x0, cy, iw, 5.0, 2.5, white(0x14));
        let fw = (iw * (l.used.clamp(0.0, 100.0) / 100.0) as f32).max(if l.used > 0.0 { 5.0 } else { 0.0 });
        ui.fill(x0, cy, fw, 5.0, 2.5, a(c, 255));
        cy += 5.0 + 6.0;
        let mut sub = crate::i18n::fmt!("{} / 5 min  ·  {} / 30 min", "5 分钟 {} · 30 分钟 {}", pts(l.d5), pts(l.d30));
        if l.resets_at > now {
            sub += &crate::i18n::fmt!("  ·  resets in {}", " · {}后重置", span(l.resets_at - now));
        }
        ui.text(x0, cy, &sub, 10.5, 400, white(0x80), iw);
        cy += sub_h;
        if l.d30 > 0.0 {
            // where the 30-minute pace ends up before the window resets
            let per_s = l.d30 / 1800.0;
            let eta = ((100.0 - l.used).max(0.0) / per_s) as i64;
            let (t, col) = if l.resets_at > now && eta >= l.resets_at - now {
                (crate::i18n::t("At this pace it lasts until the reset").to_string(), white(0x80))
            } else {
                (crate::i18n::fmt!("At this pace the limit is hit in {}", "按当前速度，{}后将用尽额度", span(eta)), a(if eta < 3600 { RED } else { AMBER }, 255))
            };
            ui.text(x0, cy, &t, 10.5, 500, col, iw);
            cy += sub_h;
        }
        cy + 10.0
    }
}

impl Content for AiFly {
    fn width(&self) -> f32 {
        356.0
    }
    fn padding(&self) -> (f32, f32) {
        (16.0, 14.0)
    }
    fn period_ms(&self) -> u32 {
        2000
    }
    fn tick(&mut self) {
        let g = ai_usage::generation();
        if g != self.gen || self.sum.is_none() {
            self.gen = g;
            self.sum = ai_usage::summary();
        }
    }
    fn click(&mut self, id: u32, _fx: f32) -> Cmd {
        match id {
            1 => Cmd::Launch(crate::config::path()),
            _ => Cmd::None,
        }
    }

    fn draw(&mut self, ui: &mut Ui) -> f32 {
        let w = ui.w;
        let Some(s) = self.sum.clone() else {
            ui.text_mid(0.0, 10.0, crate::i18n::t("AI Usage"), 13.5, 600, white(0xF2), 0.0);
            return 20.0 + 12.0 + ui.text(0.0, 32.0, crate::i18n::t("Reading logs…"), 11.5, 400, white(0x80), 0.0).min(0.0) + ui.line_h(11.5, 400);
        };
        let now = ai_usage::now();
        let r5 = s.rate5();

        // header + chip
        ui.text_mid(0.0, 10.0, crate::i18n::t("AI Usage"), 13.5, 600, white(0xF2), 0.0);
        let (chip, tint) = if r5 > 0.0 { (crate::i18n::t("Burning"), AMBER) } else { (crate::i18n::t("Idle"), GREY) };
        let cw = ui.text_w(chip, 11.0, 600) + 16.0;
        ui.fill(w - cw, 10.0 - 9.5, cw, 19.0, 9.0, a(tint, 0x26));
        ui.text_mid(w - cw + 8.0, 9.5, chip, 11.0, 600, a(tint, 255), 0.0);
        let mut y = 30.0;

        // hero: combined tok/s over 5 min
        let big_h = ui.line_h(34.0, 600);
        let bw = ui.text(0.0, y, &rate(r5), 34.0, 600, white(255), 0.0);
        let small_h = ui.line_h(15.0, 500);
        ui.text(bw + 5.0, y + big_h - 7.0 - small_h, crate::i18n::t("tok/s"), 15.0, 500, white(0x8C), 0.0);
        y += big_h - 3.0;
        let line = crate::i18n::fmt!("last 5 min  ·  30-min average {} tok/s", "最近 5 分钟 · 半小时平均 {} 词元/秒", rate(s.rate30()));
        ui.text(0.0, y, &line, 12.0, 400, white(0x99), w);
        y += ui.line_h(12.0, 400) + 14.0;

        // last 24 hours, stacked per hour
        let chart_h = 38.0;
        let hours: Vec<(u64, u64)> = (0..24)
            .map(|i| (s.claude.as_ref().map_or(0, |p| p.hourly[i]), s.codex.as_ref().map_or(0, |p| p.hourly[i])))
            .collect();
        let max = hours.iter().map(|(a, b)| a + b).max().unwrap_or(0).max(1) as f32;
        let bw = (w - 23.0 * 3.0) / 24.0;
        for (i, (cl, cx)) in hours.iter().enumerate() {
            let x = i as f32 * (bw + 3.0);
            ui.fill(x, y, bw, chart_h, 2.0, white(0x08));
            let h1 = chart_h * *cl as f32 / max;
            let h2 = chart_h * *cx as f32 / max;
            if h1 > 0.0 {
                ui.fill(x, y + chart_h - h1, bw, h1.max(1.5), 2.0, a(CLAUDE, 0xE0));
            }
            if h2 > 0.0 {
                ui.fill(x, y + chart_h - h1 - h2, bw, h2.max(1.5), 2.0, a(CODEX, 0xE0));
            }
        }
        y += chart_h + 5.0;
        let lh = ui.line_h(10.5, 400);
        ui.text(0.0, y, crate::i18n::t("24 h ago"), 10.5, 400, white(0x66), 0.0);
        let peak = crate::i18n::fmt!("peak {} tok/h", "峰值 {} 词元/小时", compact(max as f64));
        let pw = ui.text_w(&peak, 10.5, 400);
        ui.text((w - pw) / 2.0, y, &peak, 10.5, 400, white(0x66), 0.0);
        let nw = ui.text_w(crate::i18n::t("now"), 10.5, 400);
        ui.text(w - nw, y, crate::i18n::t("now"), 10.5, 400, white(0x66), 0.0);
        y += lh + 12.0;

        // sessions with tokens in the last 30 minutes
        if !s.sessions.is_empty() {
            y = self.sessions(ui, y, &s.sessions, now) + 12.0;
        }

        // one card per provider
        let mut any = false;
        if let Some(p) = &s.claude {
            y = self.provider(ui, y, p, CLAUDE, now) + 8.0;
            any = true;
        }
        if let Some(p) = &s.codex {
            y = self.provider(ui, y, p, CODEX, now) + 8.0;
            any = true;
        }
        if !any {
            y += ui.text_wrap(0.0, y, crate::i18n::t("No Claude Code or Codex logs from the last 7 days."), 11.5, 400, white(0x80), w) + 8.0;
        }

        // note
        y += 2.0;
        let note = if s.claude.as_ref().is_some_and(|p| p.limits.is_empty()) {
            "Read from local logs. Claude Code logs no limit: turn on ClaudeUsageOAuth in config.json for its real 5-hour and weekly limits, or set ClaudeLimit5h for an estimate."
        } else if s.claude.as_ref().is_some_and(|p| p.limits.iter().any(|l| l.label == "Weekly")) {
            "Tokens from local logs. Claude limits from Anthropic's usage endpoint (your Claude Code login), Codex limits from its own reports."
        } else {
            "Read from local logs. Codex limits are its own server reports; Claude's percentage is estimated from ClaudeLimit5h."
        };
        y += ui.text_wrap(0.0, y, crate::i18n::t(note), 10.5, 400, white(0x66), w);
        y += 6.0;
        let hov = ui.row(1, -6.0, y - 3.0, ui.text_w(crate::i18n::t("Open config.json"), 11.0, 500) + 12.0, ui.line_h(11.0, 500) + 6.0);
        ui.text(0.0, y, crate::i18n::t("Open config.json"), 11.0, 500, if hov { white(0xF2) } else { (0x0A, 0x84, 0xFF, 255) }, 0.0);
        y + ui.line_h(11.0, 500)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai_usage::{Limit, Provider, Summary};

    fn save(fly: &mut AiFly, name: &str) {
        let (w, h, px) = crate::flyout::snapshot(fly, 1.25);
        let dir = std::env::var("SNAP_DIR").unwrap_or_else(|_| ".".into());
        assert!(crate::gfx::save_png(&format!("{dir}\\{name}.png"), w, h, &px));
    }

    #[test]
    #[ignore] // SNAP_DIR=<folder> cargo test --release ai_snap -- --ignored
    fn ai_snap() {
        crate::ai_usage::start(None, true, || {});
        let t = std::time::Instant::now();
        while crate::ai_usage::summary().is_none() && t.elapsed().as_secs() < 20 {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        save(&mut AiFly::new(), "ai_real");

    }

    #[test]
    #[ignore]
    fn ai_sample_snapshot() {
        let now = crate::ai_usage::now();
        let mut hourly = [0u64; 24];
        for (i, h) in hourly.iter_mut().enumerate() {
            *h = ((i as u64 * 7919) % 13) * 60_000;
        }
        let claude = Provider {
            name: "Claude Code", last: now - 20, tok5: 420_000, tok30: 1_900_000, today: 8_300_000,
            block: Some((now - 7200, 3_100_000)),
            limits: vec![
                Limit { label: "5-hour".into(), used: 78.0, resets_at: now + 3 * 3600, d5: 4.2, d30: 19.0 },
                Limit { label: "Weekly".into(), used: 65.0, resets_at: now + 3 * 86400, d5: 0.3, d30: 2.0 },
            ],
            plan: None, note: None, hourly,
        };
        let codex = Provider {
            name: "Codex", last: now - 900, tok5: 0, tok30: 55_000, today: 410_000, block: None,
            limits: vec![
                Limit { label: "5-hour".into(), used: 34.0, resets_at: now + 4000, d5: 0.0, d30: 3.0 },
                Limit { label: "Weekly".into(), used: 12.0, resets_at: now + 4 * 86400, d5: 0.0, d30: 0.5 },
            ],
            plan: Some("plus".into()), note: None, hourly: hourly.map(|h| h / 3),
        };
        let se = |provider, title: &str, project: &str, model: Option<&str>, last, tok5, total| Session {
            provider, title: title.into(), project: project.into(), model: model.map(String::from),
            branch: Some("master".into()), last, tok5, tok30: tok5 * 3, total,
        };
        let sessions = vec![
            se("Claude Code", "MacDock AI usage menu bar", "MacDock", Some("claude-opus-5-5"), now - 5, 300_000, 4_100_000),
            se("Claude Code", "Lintasi driver GPS report", "armada", Some("claude-sonnet-5-5"), now - 40, 120_000, 900_000),
            se("Codex", "Fix flaky routing tests", "RouteBench", Some("gpt-5.5-codex"), now - 900, 0, 55_000),
        ];
        let mut f = AiFly { sum: Some(Arc::new(Summary { now, claude: Some(claude), codex: Some(codex), sessions })), gen: 0 };
        save(&mut f, "ai_busy");
        crate::i18n::set("zh-CN");
        save(&mut f, "ai_busy_zh_cn");
        crate::i18n::set("en");
    }
}
