//! The arcade **background**: a twinkling, drifting starfield (the "moving sky",
//! [`render_sky`]) + a crisp block-font **FANTASTIC title**.
//!
//! The sky is a deterministic parallax starfield drawn behind every phase. The
//! title is a hand block font drawn at NATIVE cell resolution and
//! **integer-scaled** (no FIGlet mush). Its geometry (scale + position) is factored
//! out ([`title_geometry`] / [`draw_title`]) so the launch can TWEEN it from
//! big-centered to small-docked over ~1s.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use std::sync::OnceLock;

/// The word, in block-font order.
const LETTERS: [char; 9] = ['F', 'A', 'N', 'T', 'A', 'S', 'T', 'I', 'C'];
/// Block-font glyph height (rows).
const GH: usize = 5;

/// 5-row block glyphs — crisp and legible at native cell size.
fn glyph(c: char) -> [&'static str; GH] {
    match c {
        'F' => ["████", "█   ", "███ ", "█   ", "█   "],
        'A' => [" ██ ", "█  █", "████", "█  █", "█  █"],
        'N' => ["█  █", "██ █", "█ ██", "█  █", "█  █"],
        'T' => ["█████", "  █  ", "  █  ", "  █  ", "  █  "],
        'S' => ["████", "█   ", "████", "   █", "████"],
        'I' => ["███", " █ ", " █ ", " █ ", "███"],
        'C' => ["████", "█   ", "█   ", "█   ", "████"],
        _ => ["    ", "    ", "    ", "    ", "    "],
    }
}

/// The assembled FANTASTIC bitmap (`GH` rows; glyphs joined by a 1-col gap),
/// cached — `on[y][x]` is a lit block cell. All rows share one width.
fn word_rows() -> &'static Vec<Vec<bool>> {
    static W: OnceLock<Vec<Vec<bool>>> = OnceLock::new();
    W.get_or_init(|| {
        (0..GH)
            .map(|r| {
                let mut line: Vec<bool> = Vec::new();
                for (i, &c) in LETTERS.iter().enumerate() {
                    if i > 0 {
                        line.push(false); // 1-col gap between letters
                    }
                    for ch in glyph(c)[r].chars() {
                        line.push(ch != ' ');
                    }
                }
                line
            })
            .collect()
    })
}

/// Set one cell at area-relative `(x, y)`; out-of-bounds is a no-op.
fn plot(buf: &mut Buffer, area: Rect, x: i32, y: i32, ch: char, style: Style) {
    if x < 0 || y < 0 || x >= area.width as i32 || y >= area.height as i32 {
        return;
    }
    if let Some(cell) = buf.cell_mut((area.x + x as u16, area.y + y as u16)) {
        cell.set_char(ch);
        cell.set_style(style);
    }
}

/// Deterministic xorshift — the starfield's only "randomness" (no `rand` dep).
fn lcg(mut x: u32) -> u32 {
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    x
}

/// The **moving sky**: a twinkling, slowly-drifting starfield on the black canvas.
/// Deterministic (seeded xorshift) so it's identical every run; three parallax
/// layers drift at different speeds, and each star twinkles on a `clock`-driven
/// sine. Drawn behind every phase (attract · minimize · hub · chat).
pub fn render_sky(buf: &mut Buffer, area: Rect, clock: f32) {
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
        plot(
            buf,
            area,
            x,
            by as i32,
            ch,
            Style::default().fg(col).bg(Color::Black),
        );
    }
}

/// Bright magenta gradient (pink → magenta → violet) across `t ∈ [0,1]` — high
/// luminance for a vivid title.
fn gradient(t: f32) -> (u8, u8, u8) {
    const A: (f32, f32, f32) = (255.0, 120.0, 225.0); // bright pink
    const B: (f32, f32, f32) = (255.0, 55.0, 215.0); // bright magenta
    const C: (f32, f32, f32) = (205.0, 100.0, 255.0); // bright violet
    let lerp = |a: (f32, f32, f32), b: (f32, f32, f32), f: f32| {
        (
            (a.0 + (b.0 - a.0) * f) as u8,
            (a.1 + (b.1 - a.1) * f) as u8,
            (a.2 + (b.2 - a.2) * f) as u8,
        )
    };
    if t < 0.5 {
        lerp(A, B, t * 2.0)
    } else {
        lerp(B, C, (t - 0.5) * 2.0)
    }
}

/// The title's placement: integer `scale` (cells per glyph pixel) + area-relative
/// top-left `(ox, oy)`. Factored out so the launch can TWEEN between two of these.
#[derive(Clone, Copy, Debug)]
pub struct TitleGeo {
    pub scale: usize,
    pub ox: i32,
    pub oy: i32,
}

/// The big, centered ATTRACT geometry — as large as the width allows (≤92%),
/// capped at ~30% of the smaller screen dim; centered.
pub fn title_geometry(area: Rect) -> TitleGeo {
    let rows = word_rows();
    let (w, h) = (rows.first().map(|r| r.len()).unwrap_or(0), rows.len());
    if w == 0 || area.width < 4 || area.height < 3 {
        return TitleGeo {
            scale: 1,
            ox: 0,
            oy: 0,
        };
    }
    let avail_w = (area.width as usize * 92) / 100;
    let cap = (area.width.min(area.height) as usize) * 3 / 10;
    let scale = (avail_w / w).min((cap / h).max(1)).max(1);
    let trows = (h * scale).div_ceil(2);
    TitleGeo {
        scale,
        ox: (area.width as i32 - (w * scale) as i32) / 2,
        oy: ((area.height as i32 - trows as i32) / 2 - 1).max(0),
    }
}

/// Draw the crisp block-font FANTASTIC at an explicit `geo`. Width is
/// integer-scaled; height is packed with **half-block** glyphs (`▀▄█`, two glyph
/// rows per terminal row) so the title isn't vertically stretched. `reveal ∈
/// [0,1]` wipes it on top→bottom (powers on). Returns the bottom row occupied.
pub fn draw_title(buf: &mut Buffer, area: Rect, geo: TitleGeo, reveal: f32) -> i32 {
    let rows = word_rows();
    let (w, h) = (rows.first().map(|r| r.len()).unwrap_or(0), rows.len());
    if w == 0 || area.width < 4 || area.height < 3 {
        return 0;
    }
    let scale = geo.scale.max(1);
    let (ox, oy) = (geo.ox, geo.oy);
    let tw = w * scale;
    let sh = h * scale; // scaled glyph sub-rows (2 per terminal row)
    let trows = sh.div_ceil(2);
    let revealed = (reveal.clamp(0.0, 1.0) * trows as f32).ceil() as usize;
    let lit = |sub: usize, tx: usize| -> bool {
        let by = sub / scale;
        by < h && rows[by][tx / scale]
    };
    for ry in 0..trows.min(revealed) {
        let (r, g, b) = gradient(ry as f32 / (trows.max(2) - 1) as f32);
        let st = Style::default()
            .fg(Color::Rgb(r, g, b))
            .add_modifier(Modifier::BOLD);
        for tx in 0..tw {
            let top = lit(2 * ry, tx);
            let bot = 2 * ry + 1 < sh && lit(2 * ry + 1, tx);
            let ch = match (top, bot) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => continue,
            };
            plot(buf, area, ox + tx as i32, oy + ry as i32, ch, st);
        }
    }
    oy + trows as i32
}

/// The big, centered ATTRACT title with a top→bottom `reveal` wipe.
pub fn render_title(buf: &mut Buffer, area: Rect, reveal: f32) -> i32 {
    draw_title(buf, area, title_geometry(area), reveal)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glyphs_at(reveal: f32) -> usize {
        let area = Rect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        render_title(&mut buf, area, reveal);
        buf.content()
            .iter()
            .filter(|c| matches!(c.symbol(), "█" | "▀" | "▄"))
            .count()
    }

    #[test]
    fn reveal_wipes_top_to_bottom() {
        let none = glyphs_at(0.0);
        let half = glyphs_at(0.5);
        let full = glyphs_at(1.0);
        assert_eq!(none, 0, "reveal 0 draws no title rows");
        assert!(full > 0, "a full reveal draws the title");
        assert!(
            half <= full,
            "a partial reveal draws no more than the full one"
        );
        assert!(half > none, "a partial reveal draws more than nothing");
    }

    #[test]
    fn scale_fits_width() {
        // On an 80-wide terminal the native 45-col title fits at scale 1; the
        // rendered title never exceeds the area width.
        let area = Rect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        let bottom = render_title(&mut buf, area, 1.0);
        assert!(bottom > 0 && bottom <= 24);
    }
}
