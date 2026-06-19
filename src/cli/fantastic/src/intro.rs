//! Optional startup flourish: a big magenta-gradient FANTASTIC that starts
//! centered and procedurally SHRINKS toward the top-left, settling into the
//! small live banner.
//!
//! How it works: the fat letterform comes from the zero-dep `tui-banner` FIGlet
//! engine, captured once as a monochrome bitmap. Every frame that bitmap is
//! procedurally downscaled (area sampling) to the current size and re-colored
//! with a vertical magenta gradient — so the shrink is continuous, not stepped.
//! Each frame is one batched clear-and-write, so there are no ghost rows.
//!
//! Fully MODULAR / REMOVABLE: delete this file and the single `intro::play()`
//! call in `run_tui` (main.rs) to remove it. Self-skips when stdout isn't a tty
//! or `FANTASTIC_NO_INTRO` is set.

use std::io::{self, IsTerminal, Write};
use std::time::Duration;

use ratatui::crossterm::{
    cursor::{Hide, Show},
    execute,
    terminal::{size, Clear, ClearType},
};
use tui_banner::Banner;

const RESET: &str = "\x1b[0m";
const HOLD_MS: u64 = 900;
const SHRINK_MS: u64 = 1100;
const FRAMES: u32 = 44;

/// Monochrome letterform: `on[y*w + x]` is a filled cell.
pub(crate) struct Bitmap {
    pub(crate) w: usize,
    pub(crate) h: usize,
    pub(crate) on: Vec<bool>,
}

/// Cubic ease-in-out — slow start, fast middle, slow stop.
fn ease(t: f32) -> f32 {
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

/// Strip `\x1b[…m` color escapes, returning the visible chars.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c2 in chars.by_ref() {
                if c2 == 'm' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Vertical magenta gradient: pink → neon-magenta → violet across `t ∈ [0,1]`.
pub(crate) fn gradient(t: f32) -> (u8, u8, u8) {
    const PINK: (f32, f32, f32) = (255.0, 110.0, 199.0);
    const MAGENTA: (f32, f32, f32) = (215.0, 0.0, 255.0);
    const VIOLET: (f32, f32, f32) = (157.0, 0.0, 255.0);
    let lerp = |a: (f32, f32, f32), b: (f32, f32, f32), f: f32| {
        (
            (a.0 + (b.0 - a.0) * f) as u8,
            (a.1 + (b.1 - a.1) * f) as u8,
            (a.2 + (b.2 - a.2) * f) as u8,
        )
    };
    if t < 0.5 {
        lerp(PINK, MAGENTA, t * 2.0)
    } else {
        lerp(MAGENTA, VIOLET, (t - 0.5) * 2.0)
    }
}

/// Capture the big FIGlet letterform as a bitmap (color stripped; any non-space
/// glyph cell = on). `None` if the engine errors.
pub(crate) fn letterform() -> Option<Bitmap> {
    let rendered = Banner::new("FANTASTIC").ok()?.render();
    let rows: Vec<Vec<char>> = rendered
        .lines()
        .map(|l| strip_ansi(l).trim_end().chars().collect())
        .collect();
    let w = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    // Keep only rows that have any glyph.
    let rows: Vec<&Vec<char>> = rows
        .iter()
        .filter(|r| r.iter().any(|c| *c != ' '))
        .collect();
    let h = rows.len();
    if w == 0 || h == 0 {
        return None;
    }
    let mut on = vec![false; w * h];
    for (y, r) in rows.iter().enumerate() {
        for (x, c) in r.iter().enumerate() {
            on[y * w + x] = *c != ' ';
        }
    }
    Some(Bitmap { w, h, on })
}

/// Area-sample `src` down to `tw × th` (a target cell is on if ANY source cell
/// in its region is on — preserves thin strokes as it shrinks).
pub(crate) fn downscale(src: &Bitmap, tw: usize, th: usize) -> Vec<bool> {
    let mut on = vec![false; tw * th];
    for ty in 0..th {
        let sy0 = ty * src.h / th;
        let sy1 = (((ty + 1) * src.h / th).max(sy0 + 1)).min(src.h);
        for tx in 0..tw {
            let sx0 = tx * src.w / tw;
            let sx1 = (((tx + 1) * src.w / tw).max(sx0 + 1)).min(src.w);
            'cell: for sy in sy0..sy1 {
                for sx in sx0..sx1 {
                    if src.on[sy * src.w + sx] {
                        on[ty * tw + tx] = true;
                        break 'cell;
                    }
                }
            }
        }
    }
    on
}

/// One batched frame at scale `scale`, top-left at `(col, row)` — built as a
/// single string (clear-all + positioned colored rows) so nothing ghosts.
fn frame(src: &Bitmap, scale: f32, col: u16, row: u16) -> String {
    let tw = ((src.w as f32 * scale).round() as usize).max(1);
    let th = ((src.h as f32 * scale).round() as usize).max(1);
    let on = downscale(src, tw, th);
    let mut buf = String::from("\x1b[2J"); // clear whole screen
    for ty in 0..th {
        let (r, g, b) = gradient(ty as f32 / (th.max(2) - 1) as f32);
        buf.push_str(&format!(
            "\x1b[{};{}H\x1b[1m\x1b[38;2;{};{};{}m",
            row as usize + ty + 1,
            col as usize + 1,
            r,
            g,
            b
        ));
        for tx in 0..tw {
            buf.push(if on[ty * tw + tx] { '█' } else { ' ' });
        }
        buf.push_str(RESET);
    }
    buf
}

/// Play the shrink-to-corner intro. No-op without a tty or when
/// `FANTASTIC_NO_INTRO` is set. Safe inside the alternate screen + raw mode.
pub fn play() {
    if !io::stdout().is_terminal() || std::env::var_os("FANTASTIC_NO_INTRO").is_some() {
        return;
    }
    let Some(src) = letterform() else { return };
    let mut out = io::stdout();
    let (cols, rows) = size().unwrap_or((80, 24));
    let _ = execute!(out, Hide);

    // Fit the START size to the terminal (≤ ~90% width, ~60% height), then shrink
    // to about a fifth of that. The top-left corner glides from centered to (0,0).
    let fit = ((cols as f32 * 0.9) / src.w as f32).min((rows as f32 * 0.6) / src.h as f32);
    let start = fit.clamp(0.1, 1.0);
    let end = (start * 0.2).max(2.0 / src.h as f32);
    let sw0 = (src.w as f32 * start).round() as u16;
    let sh0 = (src.h as f32 * start).round() as u16;
    let col0 = cols.saturating_sub(sw0) / 2;
    let row0 = rows.saturating_sub(sh0) / 2;

    let render = |out: &mut io::Stdout, scale: f32, e: f32| {
        let col = (col0 as f32 * (1.0 - e)).round() as u16;
        let row = (row0 as f32 * (1.0 - e)).round() as u16;
        let _ = out.write_all(frame(&src, scale, col, row).as_bytes());
        let _ = out.flush();
    };

    // Hold the big centered billboard, then procedurally shrink to the corner.
    render(&mut out, start, 0.0);
    std::thread::sleep(Duration::from_millis(HOLD_MS));
    let dt = Duration::from_millis(SHRINK_MS / FRAMES as u64);
    for f in 1..=FRAMES {
        let e = ease(f as f32 / FRAMES as f32);
        render(&mut out, start + (end - start) * e, e);
        std::thread::sleep(dt);
    }
    let _ = execute!(out, Clear(ClearType::All), Show);
}
