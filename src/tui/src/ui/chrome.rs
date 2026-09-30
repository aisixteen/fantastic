//! Buffer primitives shared by every pane — lifted verbatim from the old shell
//! (`src/tui/src/lib.rs:1849-2017` at f09e7476) because they were the one part
//! of it that owed nothing to the in-proc kernel.
//!
//! Everything here writes straight into the `Buffer` and clips itself. Nothing
//! allocates a widget; nothing knows what an agent is.

// Lifted whole: these are the primitives every later pane draws with, and
// splitting the lift across steps would mean re-reading the old shell each
// time. Steps 7+ consume `cell_at`, `str_at`, `clip_str`, `draw_box`.
#![allow(dead_code)]

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

use crate::theme;

/// Paint `area` opaque. Called first on every frame: the terminal's own palette
/// never shows through, so a light terminal renders the same as a dark one.
pub fn fill_black(buf: &mut Buffer, area: Rect) {
    let st = Style::default().bg(theme::BG).fg(theme::FG);
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_char(' ');
                cell.set_style(st);
            }
        }
    }
}

/// On/off square wave from a clock — the blinking attract prompt, and nothing
/// else. Pure, so it is unit-testable without a terminal.
pub fn blink(clock: f32, hz: f32) -> bool {
    ((clock * hz) as i64) % 2 == 0
}

/// Write `s` centred on row `y` of `area`, straight into the buffer (over the
/// sky). `y` is relative to `area`.
pub fn text_center(buf: &mut Buffer, area: Rect, y: i32, s: &str, style: Style) {
    if y < 0 || y >= area.height as i32 {
        return;
    }
    let len = s.chars().count() as i32;
    let x0 = area.x as i32 + (area.width as i32 - len) / 2;
    for (i, ch) in s.chars().enumerate() {
        let x = x0 + i as i32;
        if x < area.x as i32 || x >= (area.x + area.width) as i32 {
            continue;
        }
        if let Some(cell) = buf.cell_mut((x as u16, area.y + y as u16)) {
            cell.set_char(ch);
            cell.set_style(style);
        }
    }
}

/// Write one cell at ABSOLUTE `(x, y)`, clipped to `clip`; out of bounds is a
/// no-op rather than a panic — every caller is doing arithmetic on i32.
pub fn cell_at(buf: &mut Buffer, clip: Rect, x: i32, y: i32, ch: char, style: Style) {
    if x < clip.x as i32
        || y < clip.y as i32
        || x >= (clip.x + clip.width) as i32
        || y >= (clip.y + clip.height) as i32
    {
        return;
    }
    if let Some(c) = buf.cell_mut((x as u16, y as u16)) {
        c.set_char(ch);
        c.set_style(style);
    }
}

/// Write a string from ABSOLUTE `(x, y)`, clipped.
pub fn str_at(buf: &mut Buffer, clip: Rect, x: i32, y: i32, s: &str, style: Style) {
    for (i, ch) in s.chars().enumerate() {
        cell_at(buf, clip, x + i as i32, y, ch, style);
    }
}

/// Truncate `s` to `w` display cells, appending `…` when it overflows.
///
/// Width-correct: a CJK ideograph or an emoji occupies two cells, and counting
/// `chars()` would let such a line overrun its pane. The old shell counted
/// chars; that bug does not come across with the lift.
/// WORD WRAP to `w` display columns: every source line wraps on spaces, a word
/// wider than `w` is split, and a blank line stays a blank line. Unicode width,
/// not char count — a wide glyph takes two columns.
pub fn wrap(text: &str, w: usize) -> Vec<String> {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    let w = w.max(1);
    let mut out = Vec::new();
    for src in text.split('\n') {
        let mut line = String::new();
        let mut used = 0usize;
        for word in src.split(' ') {
            let ww = UnicodeWidthStr::width(word);
            let gap = usize::from(!line.is_empty());
            if used + gap + ww <= w {
                if gap == 1 {
                    line.push(' ');
                }
                line.push_str(word);
                used += gap + ww;
                continue;
            }
            if !line.is_empty() {
                out.push(std::mem::take(&mut line));
                used = 0;
            }
            // A word wider than the whole line is split where it overflows.
            for ch in word.chars() {
                let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
                if used + cw > w {
                    out.push(std::mem::take(&mut line));
                    used = 0;
                }
                line.push(ch);
                used += cw;
            }
        }
        out.push(line);
    }
    out
}

pub fn clip_str(s: &str, w: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    if UnicodeWidthStr::width(s) <= w {
        return s.to_string();
    }
    if w == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cw > w.saturating_sub(1) {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push('…');
    out
}

/// Draw a 1-cell box border `[bx, by] .. [bx+w-1, by+h-1]`, clipped.
pub fn draw_box(buf: &mut Buffer, clip: Rect, bx: i32, by: i32, w: i32, h: i32, st: Style) {
    let (r, b) = (bx + w - 1, by + h - 1);
    cell_at(buf, clip, bx, by, '┌', st);
    cell_at(buf, clip, r, by, '┐', st);
    cell_at(buf, clip, bx, b, '└', st);
    cell_at(buf, clip, r, b, '┘', st);
    for x in (bx + 1)..r {
        cell_at(buf, clip, x, by, '─', st);
        cell_at(buf, clip, x, b, '─', st);
    }
    for y in (by + 1)..b {
        cell_at(buf, clip, bx, y, '│', st);
        cell_at(buf, clip, r, y, '│', st);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn wrap_breaks_on_words_splits_a_long_one_and_keeps_blank_lines() {
        assert_eq!(super::wrap("the entry point is main", 10), vec!["the entry", "point is", "main"]);
        assert_eq!(super::wrap("abcdefghijkl", 5), vec!["abcde", "fghij", "kl"]);
        assert_eq!(super::wrap("a\n\nb", 5), vec!["a", "", "b"]);
        // display width, not chars: each CJK glyph is two columns
        assert_eq!(super::wrap("日本語", 4), vec!["日本", "語"]);
    }

    use super::*;

    #[test]
    fn blink_alternates() {
        assert!(blink(0.0, 1.0));
        assert!(!blink(1.2, 1.0));
        assert!(blink(2.1, 1.0));
    }

    #[test]
    fn clip_str_is_width_correct() {
        assert_eq!(clip_str("abc", 5), "abc");
        assert_eq!(clip_str("abcdef", 4), "abc…");
        // Two-cell glyphs: three ideographs are six cells, so at width 4 only
        // one fits beside the ellipsis.
        assert_eq!(clip_str("日本語", 4), "日…");
        assert_eq!(clip_str("日本語", 0), "");
    }

    #[test]
    fn cell_at_out_of_bounds_is_a_noop() {
        let area = Rect::new(0, 0, 4, 2);
        let mut buf = Buffer::empty(area);
        cell_at(&mut buf, area, -1, 0, 'x', Style::default());
        cell_at(&mut buf, area, 99, 0, 'x', Style::default());
        cell_at(&mut buf, area, 0, 9, 'x', Style::default());
        assert_eq!(buf.cell((0u16, 0u16)).unwrap().symbol(), " ");
    }
}
