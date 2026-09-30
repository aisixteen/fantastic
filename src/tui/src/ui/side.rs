//! THE SIDEBAR'S THREE VIEWS of the same rows.
//!
//!   TREE — the kernel's real hierarchy, for when WHERE an agent sits matters
//!          (`ui/tree.rs`, the default).
//!   FLAT — a channel list: the kernel as a header, its Activity, then every
//!          agent as `# name`, one level. Plumbing (the root's loaders and the
//!          persistence provider) folds into one `+ N system` line: collapsed,
//!          not hidden.
//!   RAIL — the sidebar collapsed to one column of square tiles. A tile says
//!          four things: the LETTER and COLOUR (who, stable per agent), the
//!          LEFT MARK (▐ selected, ◆ this kernel), the CORNER BADGE (open
//!          questions) and the STATUS DOT (● working).
//!
//! `v` toggles tree ↔ flat, `<` collapses to the rail, `>` expands back.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use crate::panel::PanelKind;
use crate::presence::Presence;
use crate::theme;
use crate::ui::tree::TreeRow;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SideView {
    #[default]
    Tree,
    Flat,
    Rail,
}

/// The rail's width: a 5-cell tile, its left mark, its badge.
pub const RAIL_W: u16 = 10;

/// Rows a rail tile takes: ▄ top, the letter, ▀ bottom.
pub const TILE_H: u16 = 3;

const TILES: [Color; 6] = [
    theme::TILE_VIOLET,
    theme::TILE_ROSE,
    theme::TILE_GREEN,
    theme::TILE_AMBER,
    theme::TILE_TEAL,
    theme::TILE_BLUE,
];

/// An agent's tile colour: from its id, so it is the same on every run and in
/// every client.
pub fn tile_colour(id: &str) -> Color {
    let h = id.bytes().fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b)));
    TILES[(h % TILES.len() as u32) as usize]
}

/// The FLAT projection of the tree rows: the root first, then the rest at one
/// level with the system agents taken out. Returns the rows and how many were
/// folded into `+ N system`.
pub fn flatten(rows: Vec<TreeRow>, is_system: impl Fn(&TreeRow) -> bool) -> (Vec<TreeRow>, usize) {
    let mut out = Vec::with_capacity(rows.len());
    let mut folded = 0;
    for (i, mut r) in rows.into_iter().enumerate() {
        if i == 0 && r.depth == 0 {
            out.push(r);
            continue;
        }
        if is_system(&r) {
            folded += 1;
            continue;
        }
        r.depth = 1;
        r.has_children = false;
        r.collapsed = false;
        out.push(r);
    }
    (out, folded)
}

/// Where row `i` sits on the rail.
pub fn rail_y(i: usize) -> u16 {
    i as u16 * TILE_H
}

/// Which tile a click at screen row `y` lands on (the rail's own scroll).
pub fn rail_row_at(n: usize, sel: usize, area: Rect, y: u16) -> Option<usize> {
    let fit = (area.height.saturating_sub(1) / TILE_H) as usize;
    let top = if sel >= fit && fit > 0 { sel + 1 - fit } else { 0 };
    let slot = ((y - area.y) / TILE_H) as usize;
    let i = top + slot;
    (slot < fit && i < n).then_some(i)
}

/// The rail: one tile per row, the selected one marked.
pub fn render_rail(buf: &mut Buffer, area: Rect, rows: &[TreeRow], sel: usize) {
    let fit = (area.height.saturating_sub(1) / TILE_H) as usize;
    let top = if sel >= fit && fit > 0 { sel + 1 - fit } else { 0 };
    let x = area.x + 2;
    for (slot, (i, r)) in rows.iter().enumerate().skip(top).take(fit).enumerate() {
        let y = area.y + rail_y(slot);
        let root = i == 0 && r.depth == 0;
        let (colour, ink, letter) = if r.kind == PanelKind::Activity {
            (theme::BLOCKED, theme::INK, theme::INQUIRY)
        } else if r.kind == PanelKind::Setup {
            (theme::BAND, theme::MUTED, '+')
        } else if root {
            (theme::ACCENT, theme::FG, 'F')
        } else {
            (tile_colour(&r.id), theme::FG, r.label.chars().next().unwrap_or('?').to_ascii_uppercase())
        };
        put(buf, x, y, "▄▄▄▄▄", Style::default().fg(colour));
        put(buf, x, y + 1, "     ", Style::default().bg(colour));
        put(
            buf,
            x + 2,
            y + 1,
            &letter.to_string(),
            Style::default().fg(ink).bg(colour).add_modifier(Modifier::BOLD),
        );
        put(buf, x, y + 2, "▀▀▀▀▀", Style::default().fg(colour));
        let mark = Style::default().fg(theme::ACCENT_HI);
        if i == sel {
            put(buf, area.x, y, "▗", mark);
            put(buf, area.x, y + 1, "▐", mark);
            put(buf, area.x, y + 2, "▝", mark);
        } else if root {
            put(buf, area.x, y + 1, "◆", Style::default().fg(theme::ACCENT));
        }
        if r.inquiries > 0 {
            put(
                buf,
                x + 5,
                y,
                &r.inquiries.to_string(),
                Style::default().fg(theme::INK).bg(theme::BLOCKED).add_modifier(Modifier::BOLD),
            );
        }
        if r.presence == Presence::Working {
            put(buf, x + 5, y + 2, "●", Style::default().fg(theme::WORKING).add_modifier(Modifier::BOLD));
        }
    }
    put(
        buf,
        area.x + 1,
        area.y + area.height.saturating_sub(1),
        "» >",
        Style::default().fg(theme::DIM),
    );
}

fn put(buf: &mut Buffer, x: u16, y: u16, s: &str, st: Style) {
    let a = buf.area;
    if y < a.y + a.height && x < a.x + a.width {
        buf.set_stringn(x, y, s, (a.x + a.width - x) as usize, st);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, depth: u8) -> TreeRow {
        TreeRow {
            id: id.into(),
            depth,
            label: id.into(),
            kind: PanelKind::Generic,
            live: true,
            has_children: false,
            collapsed: false,
            presence: Presence::Idle,
            inquiries: 0,
        }
    }

    #[test]
    fn flat_keeps_the_kernel_first_lists_agents_one_level_and_folds_the_system() {
        let rows = vec![row("kernel_state", 0), row("fs_loader", 1), row("exec", 1), row("ai", 2), row("store", 1)];
        let (flat, folded) = flatten(rows, |r| r.id == "fs_loader" || r.id == "store");
        let ids: Vec<(&str, u8)> = flat.iter().map(|r| (r.id.as_str(), r.depth)).collect();
        assert_eq!(ids, vec![("kernel_state", 0), ("exec", 1), ("ai", 1)]);
        assert_eq!(folded, 2);
    }

    #[test]
    fn a_tile_colour_is_stable_per_agent() {
        assert_eq!(tile_colour("ai"), tile_colour("ai"));
        assert!(TILES.contains(&tile_colour("reviewer")));
    }

    #[test]
    fn the_rail_marks_the_selection_and_badges_questions() {
        let mut rows = vec![row("kernel_state", 0), row("ai", 1)];
        rows[1].inquiries = 3;
        let area = Rect::new(0, 0, RAIL_W, 12);
        let mut buf = Buffer::empty(area);
        render_rail(&mut buf, area, &rows, 1);
        let cell = |x: u16, y: u16| buf.cell((x, y)).unwrap().symbol().to_string();
        assert_eq!(cell(0, rail_y(1) + 1), "▐", "the selected tile has the tall mark");
        assert_eq!(cell(0, rail_y(0) + 1), "◆", "the kernel's own tile says so");
        assert_eq!(cell(4, rail_y(1) + 1), "A");
        assert_eq!(cell(7, rail_y(1)), "3", "open questions in the corner");
    }
}
