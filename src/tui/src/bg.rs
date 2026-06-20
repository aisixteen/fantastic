//! Arcade-cabinet **background**: a reusable, animated backdrop shared by the
//! attract screen and the chat screen — a looped drifting starfield plus the
//! big magenta-gradient FANTASTIC letterform.
//!
//! These draw STRAIGHT into `f.buffer_mut()` (no widgets) so that real widgets
//! (transcript, input box) render opaquely on top of them. The starfield reuses
//! the movie's deterministic seeded starfield (`movie::starfield`) so the bg and
//! the intro cutscene look identical; the title is built from
//! `intro::{letterform,downscale,gradient}`, scaled by `max_scale` and revealed
//! top→bottom by `reveal` so the attract screen can draw it large and "power it
//! on" row-by-row.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use crate::intro::{self, Bitmap};
use crate::movie;
use std::sync::OnceLock;

/// The cached big letterform (rendered once via the FIGlet engine).
fn title_bitmap() -> Option<&'static Bitmap> {
    static BMP: OnceLock<Option<Bitmap>> = OnceLock::new();
    BMP.get_or_init(intro::letterform).as_ref()
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

/// The big magenta-gradient FANTASTIC, centered horizontally near the top of
/// `area`. `max_scale` bounds the size: ~0.5 draws the large attract billboard;
/// a small value (e.g. 0.16) draws a slim dim title band for the chat screen.
/// `reveal ∈ [0,1]` is a top→bottom "power-on" wipe: only the top `ceil(reveal *
/// th)` rows are drawn (so the title appears row-by-row); `reveal >= 1.0` draws
/// the whole bitmap. Returns the bottom row (area-relative) the title occupies
/// (the FULL bottom, so callers can place text beneath it even mid-reveal).
/// Mirrors the movie Title scene's bitmap draw.
pub(crate) fn render_title(
    buf: &mut Buffer,
    area: Rect,
    _clock: f32,
    max_scale: f32,
    reveal: f32,
) -> i32 {
    let Some(bm) = title_bitmap() else {
        return 0;
    };
    let maxw = area.width as f32 * 0.82;
    let maxh = area.height as f32 * max_scale;
    let scale = (maxw / bm.w as f32)
        .min(maxh / bm.h as f32)
        .min(max_scale)
        .max(0.0);
    let tw = ((bm.w as f32 * scale) as usize).max(1);
    let th = ((bm.h as f32 * scale) as usize).max(1);
    let on = intro::downscale(bm, tw, th);
    let ox = (area.width as i32 - tw as i32) / 2;
    // Large titles sit slightly above center; small ones hug the top.
    let oy = if max_scale >= 0.3 {
        (area.height as i32 / 2 - th as i32 / 2 - 1).max(0)
    } else {
        0
    };
    // Dim the small chat-screen band so it reads as background, not foreground.
    let dim = max_scale < 0.3;
    // Top→bottom reveal: only the top `ceil(reveal * th)` rows are drawn.
    let revealed = (reveal.clamp(0.0, 1.0) * th as f32).ceil() as usize;
    for ty in 0..th.min(revealed) {
        let (r, g, b) = intro::gradient(ty as f32 / (th.max(2) - 1) as f32);
        let (r, g, b) = if dim {
            (r / 2, g / 2, b / 2)
        } else {
            (r, g, b)
        };
        let st = Style::default()
            .fg(Color::Rgb(r, g, b))
            .add_modifier(Modifier::BOLD);
        for tx in 0..tw {
            if on[ty * tw + tx] {
                plot(buf, area, ox + tx as i32, oy + ty as i32, '█', st);
            }
        }
    }
    oy + th as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Count the `█` title cells rendered into a fresh buffer at a given reveal.
    fn glyphs_at(reveal: f32) -> usize {
        let area = Rect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        render_title(&mut buf, area, 0.0, 0.5, reveal);
        buf.content().iter().filter(|c| c.symbol() == "█").count()
    }

    #[test]
    fn reveal_wipes_top_to_bottom() {
        // A 0.0 reveal draws nothing; partial draws some; >=1.0 draws the full
        // title — and the row-by-row wipe is monotonic (more reveal ≥ less).
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
}
