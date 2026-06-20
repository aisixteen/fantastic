//! The big magenta-gradient FANTASTIC letterform, captured once as a monochrome
//! bitmap and reused by the arcade background (`bg.rs`) and the intro movie.
//!
//! How it works: the fat letterform comes from the zero-dep `tui-banner` FIGlet
//! engine, captured once as a monochrome bitmap. Callers procedurally downscale
//! that bitmap (area sampling) to a target size and re-color it with a vertical
//! magenta gradient — so any scaling is continuous, not stepped.

use tui_banner::Banner;

/// Monochrome letterform: `on[y*w + x]` is a filled cell.
pub(crate) struct Bitmap {
    pub(crate) w: usize,
    pub(crate) h: usize,
    pub(crate) on: Vec<bool>,
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
