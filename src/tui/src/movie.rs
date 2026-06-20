//! "Intro" mode — a small, scripted, retro 19xx-game / demoscene **movie** that
//! explains how Fantastic works (everything is an agent · one verb `send` ·
//! `reflect` · compose a system · the brain).
//!
//! MODULAR / ARBITRARY: the movie is a `Vec<Box<dyn Scene>>`; add or reorder a
//! scene by editing `Movie::storyboard()`. Each scene draws into the body `area`
//! given its local progress `t ∈ [0,1]` plus the global `clock` (seconds) for
//! continuous effects (blink, color-cycle, starfield drift). Rendering is plain
//! buffer-cell writes (top-down coords), so it's resolution-tolerant and the
//! whole `area` is cleared each frame (no ghosts). Deterministic: any "noise"
//! comes from a seeded xorshift, so the cutscene looks identical every run.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::Frame;

/// A single movie scene.
trait Scene {
    /// Play length in seconds.
    fn dur(&self) -> f32;
    /// Draw at local progress `t ∈ [0,1]`; `clock` is global elapsed seconds.
    fn render(&self, buf: &mut Buffer, area: Rect, t: f32, clock: f32);
}

/// The scripted movie (a list of scenes, played in order, looping).
pub struct Movie {
    scenes: Vec<Box<dyn Scene>>,
}

impl Movie {
    /// Build the storyboard. **Add a scene = push one line here.**
    pub fn storyboard() -> Self {
        Movie {
            scenes: vec![
                Box::new(Send),
                Box::new(Reflect),
                Box::new(Compose),
                Box::new(Brain),
                Box::new(Credits),
            ],
        }
    }

    /// Total run length of the storyboard (sum of scene durations) in seconds.
    /// The attract loop uses this to know when the demo has finished one pass.
    pub fn total_secs(&self) -> f32 {
        self.scenes.iter().map(|s| s.dur()).sum()
    }

    /// Render the movie at `elapsed` seconds into `area` of the frame.
    pub fn render(&self, f: &mut Frame, area: Rect, elapsed: f32) {
        let durs: Vec<f32> = self.scenes.iter().map(|s| s.dur()).collect();
        let (idx, t) = scene_at(&durs, elapsed);
        let buf = f.buffer_mut();
        clear(buf, area);
        self.scenes[idx].render(buf, area, t, elapsed);
        chrome(buf, area, idx, self.scenes.len(), elapsed);
    }
}

/// Director math: given per-scene durations and `elapsed`, return the current
/// scene index and its local progress `t ∈ [0,1)`. Loops forever. Pure +
/// unit-tested (no `Frame` needed).
pub(crate) fn scene_at(durs: &[f32], elapsed: f32) -> (usize, f32) {
    let total: f32 = durs.iter().sum();
    if durs.is_empty() || total <= 0.0 {
        return (0, 0.0);
    }
    let mut e = elapsed.max(0.0) % total;
    for (i, &d) in durs.iter().enumerate() {
        if d > 0.0 && e < d {
            return (i, (e / d).clamp(0.0, 1.0));
        }
        e -= d;
    }
    (durs.len() - 1, 1.0)
}

// ── low-level drawing helpers (area-relative, clipped) ──────────────

/// Set one cell at area-relative `(x, y)`. Out-of-bounds is a no-op.
fn plot(buf: &mut Buffer, area: Rect, x: i32, y: i32, ch: char, style: Style) {
    if x < 0 || y < 0 || x >= area.width as i32 || y >= area.height as i32 {
        return;
    }
    let px = area.x + x as u16;
    let py = area.y + y as u16;
    if let Some(cell) = buf.cell_mut((px, py)) {
        cell.set_char(ch);
        cell.set_style(style);
    }
}

/// Write a string left-to-right from area-relative `(x, y)` (per-char clipped).
fn text(buf: &mut Buffer, area: Rect, x: i32, y: i32, s: &str, style: Style) {
    for (i, ch) in s.chars().enumerate() {
        plot(buf, area, x + i as i32, y, ch, style);
    }
}

/// Write `s` horizontally centered on row `y`.
fn text_center(buf: &mut Buffer, area: Rect, y: i32, s: &str, style: Style) {
    let x = (area.width as i32 - s.chars().count() as i32) / 2;
    text(buf, area, x, y, s, style);
}

/// Clear the whole area to black (so nothing ghosts between frames).
fn clear(buf: &mut Buffer, area: Rect) {
    let bg = Style::default().bg(Color::Black).fg(Color::Black);
    for y in 0..area.height as i32 {
        for x in 0..area.width as i32 {
            plot(buf, area, x, y, ' ', bg);
        }
    }
}

/// A labeled box with single-line borders; label centered on the middle row.
#[allow(clippy::too_many_arguments)] // a drawing primitive; a struct would be noise
fn draw_box(buf: &mut Buffer, area: Rect, x: i32, y: i32, w: i32, h: i32, label: &str, st: Style) {
    if w < 2 || h < 2 {
        return;
    }
    for i in 0..w {
        plot(buf, area, x + i, y, '─', st);
        plot(buf, area, x + i, y + h - 1, '─', st);
    }
    for j in 0..h {
        plot(buf, area, x, y + j, '│', st);
        plot(buf, area, x + w - 1, y + j, '│', st);
    }
    plot(buf, area, x, y, '┌', st);
    plot(buf, area, x + w - 1, y, '┐', st);
    plot(buf, area, x, y + h - 1, '└', st);
    plot(buf, area, x + w - 1, y + h - 1, '┘', st);
    let lx = x + (w - label.chars().count() as i32) / 2;
    text(buf, area, lx, y + h / 2, label, st);
}

/// Deterministic xorshift — our only "randomness" (no `rand` dep).
fn lcg(mut x: u32) -> u32 {
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    x
}

/// On/off square wave at `hz` from the clock (for blinking prompts).
fn blink(clock: f32, hz: f32) -> bool {
    ((clock * hz) as i64) % 2 == 0
}

/// A 4-color CGA-ish cycle offset by `off`, stepping a few times per second.
fn cycle(clock: f32, off: usize) -> Color {
    const P: [Color; 4] = [
        Color::Cyan,
        Color::Magenta,
        Color::LightCyan,
        Color::LightMagenta,
    ];
    P[(((clock * 4.0) as usize) + off) % P.len()]
}

fn bold(c: Color) -> Style {
    Style::default().fg(c).add_modifier(Modifier::BOLD)
}

/// Cubic ease-in-out.
fn ease(t: f32) -> f32 {
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

/// Twinkling, drifting starfield (deterministic). `pub(crate)` so the arcade
/// background (`bg::render_stars`) reuses the exact same field — the attract
/// screen, the chat screen, and the intro cutscene all share one starfield.
pub(crate) fn starfield(buf: &mut Buffer, area: Rect, clock: f32) {
    let w = area.width.max(1) as u32;
    let h = area.height.max(1) as u32;
    let n = ((w * h) / 35).clamp(20, 220);
    for i in 0..n {
        let s = lcg(i.wrapping_mul(2_654_435_761));
        let layer = 1 + (s % 3); // parallax speed
        let by = (s >> 9) % h;
        let drift = (clock * layer as f32 * 4.0) as u32;
        let x = (((s >> 3) % w + drift) % w) as i32;
        let ch = match (s >> 17) % 3 {
            0 => '.',
            1 => '·',
            _ => '*',
        };
        let tw = (clock * (2.0 + (s % 5) as f32) + (s % 7) as f32).sin();
        let col = if tw > 0.5 {
            Color::White
        } else if tw > -0.2 {
            Color::Gray
        } else {
            Color::DarkGray
        };
        plot(buf, area, x, by as i32, ch, Style::default().fg(col));
    }
}

/// Persistent chrome over every scene: scene counter + blinking exit hint.
fn chrome(buf: &mut Buffer, area: Rect, idx: usize, total: usize, clock: f32) {
    let tag = format!("SCENE {}/{}", idx + 1, total);
    text(
        buf,
        area,
        area.width as i32 - tag.len() as i32 - 1,
        0,
        &tag,
        Style::default().fg(Color::DarkGray),
    );
    if blink(clock, 1.5) {
        text_center(
            buf,
            area,
            area.height as i32 - 1,
            "▶ SHIFT-TAB ▶",
            bold(Color::Yellow),
        );
    }
}

// ── scenes ──────────────────────────────────────────────────────────

/// SEND: a packet eases along the wire from [core] to [web].
struct Send;
impl Scene for Send {
    fn dur(&self) -> f32 {
        4.0
    }
    fn render(&self, buf: &mut Buffer, area: Rect, t: f32, clock: f32) {
        let midy = area.height as i32 / 2 - 1;
        let bw = 8;
        let bh = 3;
        let lx = 4;
        let rx = area.width as i32 - 4 - bw;
        let wire_y = midy + 1;
        // wire
        for x in (lx + bw)..rx {
            plot(
                buf,
                area,
                x,
                wire_y,
                '─',
                Style::default().fg(Color::DarkGray),
            );
        }
        let arrived = t > 0.92;
        draw_box(buf, area, lx, midy, bw, bh, "core", bold(Color::Cyan));
        draw_box(
            buf,
            area,
            rx,
            midy,
            bw,
            bh,
            "web",
            if arrived && blink(clock, 6.0) {
                bold(Color::White)
            } else {
                bold(Color::Magenta)
            },
        );
        // packet
        let p = ease(t);
        let px = (lx + bw) as f32 + ((rx - lx - bw) as f32) * p;
        plot(buf, area, px as i32, wire_y, '●', bold(Color::Yellow));
        if px as i32 - 1 > lx + bw {
            plot(
                buf,
                area,
                px as i32 - 1,
                wire_y,
                '∘',
                Style::default().fg(Color::Yellow),
            );
        }
        text_center(buf, area, midy + 4, "ONE VERB", bold(Color::White));
        text_center(
            buf,
            area,
            midy + 5,
            "send(target, payload)",
            bold(cycle(clock, 0)),
        );
    }
}

/// REFLECT: an agent box self-describes (typewriter).
struct Reflect;
impl Scene for Reflect {
    fn dur(&self) -> f32 {
        4.4
    }
    fn render(&self, buf: &mut Buffer, area: Rect, t: f32, _clock: f32) {
        let lines = [
            "reflect →",
            "{ id: \"web\",",
            "  verbs: [boot, mount, serve],",
            "  serves: \"http\" }",
        ];
        let total: usize = lines.iter().map(|l| l.chars().count()).sum();
        let shown = (t * total as f32 * 1.15) as usize;
        let bx = (area.width as i32 - 34) / 2;
        let by = area.height as i32 / 2 - 4;
        draw_box(
            buf,
            area,
            bx,
            by,
            34,
            lines.len() as i32 + 2,
            "",
            bold(Color::Magenta),
        );
        let mut budget = shown;
        for (i, l) in lines.iter().enumerate() {
            let take = budget.min(l.chars().count());
            let part: String = l.chars().take(take).collect();
            let col = if i == 0 {
                Color::Yellow
            } else {
                Color::LightCyan
            };
            text(
                buf,
                area,
                bx + 2,
                by + 1 + i as i32,
                &part,
                Style::default().fg(col),
            );
            budget = budget.saturating_sub(l.chars().count());
        }
        text_center(
            buf,
            area,
            by + lines.len() as i32 + 3,
            "AGENTS DESCRIBE THEMSELVES",
            bold(Color::White),
        );
        text_center(
            buf,
            area,
            by + lines.len() as i32 + 4,
            "capability emerges",
            Style::default().fg(Color::DarkGray),
        );
    }
}

/// COMPOSE: nodes pop in and wire up into a living system.
struct Compose;
impl Scene for Compose {
    fn dur(&self) -> f32 {
        5.0
    }
    fn render(&self, buf: &mut Buffer, area: Rect, t: f32, clock: f32) {
        let cx = area.width as i32 / 2;
        let cy = area.height as i32 / 2;
        let nodes = [
            ("core", cx - 5, cy - 4),
            ("file", cx - 16, cy + 1),
            ("web", cx + 6, cy + 1),
            ("brain", cx - 5, cy + 5),
        ];
        let edges = [(0usize, 1usize), (0, 2), (0, 3)];
        let vis = |k: usize| t > k as f32 / nodes.len() as f32;
        // edges first (so boxes sit on top)
        for &(a, b) in &edges {
            if vis(a) && vis(b) {
                line(
                    buf,
                    area,
                    nodes[a].1 + 3,
                    nodes[a].2 + 1,
                    nodes[b].1 + 3,
                    nodes[b].2 + 1,
                    Color::DarkGray,
                );
            }
        }
        for (k, (name, x, y)) in nodes.iter().enumerate() {
            if vis(k) {
                let st = bold(cycle(clock, k));
                draw_box(
                    buf,
                    area,
                    *x,
                    *y,
                    name.chars().count() as i32 + 2,
                    3,
                    name,
                    st,
                );
            }
        }
        text_center(
            buf,
            area,
            area.height as i32 - 3,
            "COMPOSE A LIVING SYSTEM",
            bold(Color::White),
        );
    }
}

/// BRAIN: the brain fires packets to other agents (it drives `send` too).
struct Brain;
impl Scene for Brain {
    fn dur(&self) -> f32 {
        4.4
    }
    fn render(&self, buf: &mut Buffer, area: Rect, _t: f32, clock: f32) {
        let cx = area.width as i32 / 2;
        let cy = area.height as i32 / 2 - 1;
        let targets = [
            ("file", cx - 18, cy - 3),
            ("web", cx + 12, cy - 3),
            ("ui", cx, cy + 4),
        ];
        let (bcx, bcy) = (cx as f32 + 1.0, cy as f32 + 1.0); // brain center
        for (k, (name, x, y)) in targets.iter().enumerate() {
            draw_box(
                buf,
                area,
                *x,
                *y,
                name.chars().count() as i32 + 2,
                3,
                name,
                bold(Color::Cyan),
            );
            // a packet pulses out from the brain toward target k, phase-shifted
            let (tcx, tcy) = (*x as f32 + 1.0, *y as f32 + 1.0);
            let phase = (clock * 0.8 + k as f32 * 0.33).fract();
            pp(
                buf,
                area,
                bcx + (tcx - bcx) * phase,
                bcy + (tcy - bcy) * phase,
                '●',
                bold(Color::Yellow),
            );
            let trail = (phase - 0.12).max(0.0);
            pp(
                buf,
                area,
                bcx + (tcx - bcx) * trail,
                bcy + (tcy - bcy) * trail,
                '·',
                Style::default().fg(Color::Yellow),
            );
        }
        draw_box(buf, area, cx - 3, cy, 8, 3, "brain", bold(Color::Magenta));
        text_center(
            buf,
            area,
            area.height as i32 - 3,
            "THE BRAIN DRIVES THE SAME send",
            bold(Color::White),
        );
    }
}

/// CREDITS: a color-cycling marquee scroller.
struct Credits;
impl Scene for Credits {
    fn dur(&self) -> f32 {
        6.0
    }
    fn render(&self, buf: &mut Buffer, area: Rect, _t: f32, clock: f32) {
        const MSG: &str = "FANTASTIC  ·  everything is an agent  ·  one verb: send  ·  reflect to discover  ·  compose a living system  ·  scaffolding for emerging software  ·  press SHIFT-TAB  ·   ";
        let chars: Vec<char> = MSG.chars().collect();
        let w = area.width as i32;
        let y = area.height as i32 / 2;
        let off = (clock * 14.0) as usize;
        for col in 0..w {
            let ch = chars[(off + col as usize) % chars.len()];
            if ch != ' ' {
                plot(buf, area, col, y, ch, bold(cycle(clock, col as usize / 3)));
            }
        }
        // sine baseline of blocks under the scroller
        for col in 0..w {
            let s = ((col as f32 * 0.3 + clock * 4.0).sin() * 1.5) as i32;
            plot(
                buf,
                area,
                col,
                y + 3 + s,
                '▂',
                Style::default().fg(cycle(clock, col as usize)),
            );
        }
        text_center(buf, area, y - 3, "FANTASTIC", bold(Color::LightMagenta));
    }
}

/// Draw a straight line between two area-relative points (Bresenham).
fn line(buf: &mut Buffer, area: Rect, x0: i32, y0: i32, x1: i32, y1: i32, c: Color) {
    let (dx, dy) = ((x1 - x0).abs(), -(y1 - y0).abs());
    let (sx, sy) = (if x0 < x1 { 1 } else { -1 }, if y0 < y1 { 1 } else { -1 });
    let (mut x, mut y, mut err) = (x0, y0, dx + dy);
    let st = Style::default().fg(c);
    loop {
        plot(buf, area, x, y, '·', st);
        if x == x1 && y == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

/// Plot at float coords (rounded).
fn pp(buf: &mut Buffer, area: Rect, x: f32, y: f32, ch: char, st: Style) {
    plot(buf, area, x.round() as i32, y.round() as i32, ch, st);
}

#[cfg(test)]
mod tests {
    use super::{scene_at, Movie};

    #[test]
    fn total_secs_sums_scene_durations() {
        let m = Movie::storyboard();
        // The sum of every scene's `dur()` — matches `scene_at` looping at total.
        let expect: f32 = m.scenes.iter().map(|s| s.dur()).sum();
        assert!((m.total_secs() - expect).abs() < 1e-6);
        assert!(m.total_secs() > 0.0, "storyboard has positive run length");
    }

    #[test]
    fn picks_scene_and_local_progress() {
        let d = [1.0_f32, 2.0, 3.0];
        assert_eq!(scene_at(&d, 0.0), (0, 0.0));
        assert_eq!(scene_at(&d, 0.5).0, 0);
        assert!((scene_at(&d, 0.5).1 - 0.5).abs() < 1e-5);
        assert_eq!(scene_at(&d, 1.0), (1, 0.0)); // boundary → next scene
        assert_eq!(scene_at(&d, 3.0), (2, 0.0));
    }

    #[test]
    fn loops_after_total() {
        let d = [1.0_f32, 2.0, 3.0]; // total 6
        assert_eq!(scene_at(&d, 6.0), (0, 0.0));
        assert_eq!(scene_at(&d, 6.5).0, 0);
        assert_eq!(scene_at(&d, 7.0), (1, 0.0));
    }

    #[test]
    fn degenerate_durations_are_safe() {
        assert_eq!(scene_at(&[], 3.0), (0, 0.0));
        assert_eq!(scene_at(&[0.0, 0.0], 1.0), (0, 0.0));
    }
}
