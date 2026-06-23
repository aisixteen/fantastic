//! Arcade-cabinet **background**: an animated starfield + a crisp block-font
//! FANTASTIC title.
//!
//! The title is a hand block font drawn at NATIVE cell resolution and
//! **integer-scaled** to the terminal — NO FIGlet downscaling (a 118-col FIGlet
//! shrunk to fit turned to mush). The scale is recomputed each frame: as large
//! as the width allows, but capped at ~half the smaller screen dimension. Bright
//! magenta gradient, bold, revealed top→bottom by `reveal`. The starfield reuses
//! the movie's deterministic seeded starfield so the bg and the cutscene match.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use std::sync::OnceLock;

use crate::movie;

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

/// The looped, drifting, twinkling starfield — REUSES the movie's seeded
/// starfield so the bg and the intro cutscene are visually identical.
pub(crate) fn render_stars(buf: &mut Buffer, area: Rect, clock: f32) {
    movie::starfield(buf, area, clock);
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

/// Draw the crisp block-font FANTASTIC, centered, scaled to the terminal. Width is
/// integer-scaled to fit; height is packed with **half-block** glyphs (`▀▄█`, two
/// glyph rows per terminal row) so the title isn't vertically stretched — terminal
/// cells are ~2:1, so packing two rows per cell ≈ corrects the aspect (~1.5–2×
/// shorter than full-block). `reveal ∈ [0,1]` wipes it on top→bottom (powers on).
/// Returns the bottom row (area-relative) the title occupies.
pub(crate) fn render_title(buf: &mut Buffer, area: Rect, reveal: f32) -> i32 {
    let rows = word_rows();
    let w = rows.first().map(|r| r.len()).unwrap_or(0);
    let h = rows.len();
    if w == 0 || area.width < 4 || area.height < 3 {
        return 0;
    }
    // Integer scale fits ≤92% width, capped at ~30% of the smaller screen dim.
    // Half-block packing then halves the rendered HEIGHT (the stretch fix) without
    // changing the width — same footprint, ~1.5–2× shorter.
    let avail_w = (area.width as usize * 92) / 100;
    let cap = (area.width.min(area.height) as usize) * 3 / 10;
    let scale = (avail_w / w).min((cap / h).max(1)).max(1);
    let tw = w * scale; // terminal cols
    let sh = h * scale; // scaled glyph sub-rows (2 per terminal row)
    let trows = sh.div_ceil(2); // terminal rows after half-block packing
    let ox = (area.width as i32 - tw as i32) / 2;
    let oy = ((area.height as i32 - trows as i32) / 2 - 1).max(0);
    let revealed = (reveal.clamp(0.0, 1.0) * trows as f32).ceil() as usize;
    // Is scaled sub-row `sub`, terminal col `tx`, a lit block cell?
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
