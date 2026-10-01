//! THE FRAME. The tree takes the left column, full height; the right column is
//! the selected agent's own UI, the in-app terminal under it when open, and the
//! composer at its bottom. One statusline across the whole width under both.
//!
//! Every pane is a pure function of state: nothing here owns a channel, spawns a
//! task or touches the wire. There is no docked title: the start screen owns the
//! name, it dissolves in place when the shell goes live, and every row after that
//! belongs to the work.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::{App, Focus, Link};
use crate::kern::model::AgentId;
use crate::kern::terminal::TERM_ID;
use crate::keys;
use crate::theme;
use crate::ui::chrome;
use crate::ui::composer::{Mode, Palette};

use crate::ui::layout::{COMPOSER_H, STATUS_H, TITLE_H};

/// The rows the in-app terminal takes below the page: a heading line plus
/// its screen.
fn term_h(body: u16) -> u16 {
    (body * 45 / 100).max(8).min(body.saturating_sub(4))
}

/// Where everything goes, for a screen of `area`. The ONE copy of the split,
/// so `layout`, `draw` and a mouse click cannot disagree about it.
///
/// EVERY PANEL IS BOXED: a rounded border, dim when unfocused and accent when
/// focused, so Tab visibly moves the focus. Each panel always has its border,
/// so moving the focus never reflows content. The fields are the CONTENT rects,
/// inside the borders; `*_box` are the borders.
pub struct Areas {
    pub tree_box: Rect,
    pub tree: Rect,
    pub page_box: Rect,
    /// The selected agent's own UI.
    pub page: Rect,
    /// The in-app terminal, BELOW the page (its box, its screen), while one is
    /// open and it is not itself the selection.
    pub term: Option<(Rect, Rect)>,
    pub composer_box: Rect,
    pub composer: Rect,
    pub status: Rect,
}

/// The content rect inside a border.
pub fn inner(r: Rect) -> Rect {
    Rect {
        x: r.x.saturating_add(1),
        y: r.y.saturating_add(1),
        width: r.width.saturating_sub(2),
        height: r.height.saturating_sub(2),
    }
}

pub fn areas(app: &App, area: Rect) -> Areas {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(TITLE_H),
            Constraint::Min(5),
            Constraint::Length(STATUS_H),
        ])
        .split(area);
    // THE TREE TAKES THE LEFT COLUMN, full height; the right column stacks the
    // page, the in-app terminal and the composer — a coding-CLI layout.
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(app.side_w()), Constraint::Min(20)])
        .split(rows[1]);
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(COMPOSER_H + 2)])
        .split(body[1]);
    let mut page_box = right[0];
    let mut term = None;
    let below = app.term_open() && app.selected().map(String::as_str) != Some(TERM_ID);
    if below && page_box.height >= 14 {
        let h = term_h(page_box.height);
        let tbox = Rect { y: page_box.y + page_box.height - h, height: h, ..page_box };
        page_box.height -= h;
        term = Some((tbox, inner(tbox)));
    }
    Areas {
        tree_box: body[0],
        tree: inner(body[0]),
        page_box,
        page: inner(page_box),
        term,
        composer_box: right[1],
        composer: inner(right[1]),
        status: rows[2],
    }
}

/// Which region a screen cell falls in, for a mouse click.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    Tree,
    Page,
    Term,
    Composer,
    None,
}

pub fn hit(a: &Areas, x: u16, y: u16) -> Hit {
    let inside = |r: Rect| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height;
    if inside(a.tree_box) {
        Hit::Tree
    } else if a.term.is_some_and(|(b, _)| inside(b)) {
        Hit::Term
    } else if inside(a.page_box) {
        Hit::Page
    } else if inside(a.composer_box) {
        Hit::Composer
    } else {
        Hit::None
    }
}

/// A panel's border; the focused one wears the accent.
fn frame_box<'a>(f: &mut Frame, area: Rect, focused: bool, title: Option<Line<'a>>) {
    use ratatui::widgets::{Block, BorderType, Borders};
    let mut b = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(if focused {
            Style::default().fg(theme::ACCENT_HI).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::RULE)
        });
    if let Some(t) = title {
        b = b.title(t);
    }
    f.render_widget(b, area);
}

/// THE VIEWMODEL STEP of a frame: every derived field brought up to date for a
/// screen of `area`, BEFORE anything is drawn. Only this takes the app
/// mutably; `draw` then reads it.
pub fn layout(app: &mut App, area: Rect) {
    if app.help {
        return;
    }
    app.screen = area;
    let a = areas(app, area);
    let mut todo: Vec<(AgentId, Rect)> = Vec::new();
    if let Some(agent) = app.selected().cloned() {
        todo.push((agent, a.page));
    }
    if let Some((_, screen)) = a.term {
        todo.push((TERM_ID.into(), screen));
    }
    for (agent, rect) in todo {
        // Taken out of the map for the call only, so the panel can borrow the
        // app for its context while it updates itself.
        if let Some(mut p) = app.panels.remove(&agent) {
            {
                let cx = app.cx(&agent);
                p.layout(rect, &cx);
            }
            let cmds = p.take_cmds();
            app.panels.insert(agent, p);
            for c in cmds {
                app.push(c);
            }
        }
    }
}

/// THE VIEW of a frame. Read-only over the app: it draws what `layout` left.
pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    // An opaque canvas first, so the shell looks the same on a light terminal.
    chrome::fill_black(f.buffer_mut(), area);

    // No docked title: the start screen owns the name, and the live shell gives
    // every row to the work (design decision, `title_h = 0`).

    if app.help {
        help(f, area);
        return;
    }

    let a = areas(app, area);
    let page_is_term = app.selected().map(String::as_str) == Some(TERM_ID);
    frame_box(f, a.tree_box, app.focus == Focus::Tree, None);
    frame_box(
        f,
        a.page_box,
        app.focus == Focus::Panel || (app.focus == Focus::Term && page_is_term),
        None,
    );
    frame_box(f, a.composer_box, app.focus == Focus::Composer, None);

    crate::ui::tree::render(
        f,
        a.tree,
        &app.trows,
        app.sel,
        app.focus == Focus::Tree,
        // ALWAYS, not only while typing. A committed filter still hides agents,
        // and a filter you cannot see is indistinguishable from a broken tree
        // (measured 2026-09-13: every agent vanished and the pane just said
        // "reading the tree…").
        app.filter.as_deref(),
        app.side,
        app.system_folded,
    );

    // The right pane IS the selected agent's UI. Taken out of the map so the
    // panel can borrow the app for its context.
    if let Some(agent) = app.selected().cloned() {
        if let Some(p) = app.panels.get(&agent) {
            let cx = app.cx(&agent);
            p.render(f, a.page, &cx);
        } else {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "  press Enter to open this agent",
                    Style::default().fg(theme::DIM),
                ))),
                a.page,
            );
        }
    } else {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  reading the kernel's shape…",
                Style::default().fg(theme::DIM),
            ))),
            a.page,
        );
    }

    if let Some((tbox, screen)) = a.term {
        term_heading(f, tbox, app);
        if let Some(p) = app.panels.get(TERM_ID) {
            let id: AgentId = TERM_ID.into();
            p.render(f, screen, &app.cx(&id));
        }
    }

    composer(f, a.composer, app);
    status(f, a.status, app);

    if app.composer.palette != Palette::None && app.focus == Focus::Composer {
        palette(f, a.page, app);
    }
}

/// The terminal's box: its name, what runs in it, and who has the keyboard,
/// in the border's title.
fn term_heading(f: &mut Frame, area: Rect, app: &App) {
    let typing = app.focus == Focus::Term;
    let command = app
        .reflects
        .get(TERM_ID)
        .and_then(|r| r.get("command"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let title = Line::from(vec![
        Span::styled(
            format!(" {TERM_ID} "),
            Style::default().fg(theme::INK).bg(theme::ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  {command}  "), Style::default().fg(theme::DIM)),
        Span::styled(
            if typing { " typing · Esc hands the keyboard back " } else { " T type into it · t close it " },
            Style::default().fg(if typing { theme::WORKING } else { theme::DIM }),
        ),
    ]);
    frame_box(f, area, typing, Some(title));
}

fn composer(f: &mut Frame, area: Rect, app: &App) {
    let focused = app.focus == Focus::Composer;
    let sigil = match app.composer.mode {
        Mode::Message => "›",
        Mode::Verb => "/",
        Mode::Locked => "×",
    };
    let style = if focused {
        theme::prompt()
    } else {
        Style::default().fg(theme::DIM)
    };
    let text = app.composer.text.replace('\n', "⏎");
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!(" {sigil} "), style),
            Span::styled(text, Style::default().fg(theme::FG)),
        ])),
        Rect { height: 1, ..area },
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("   {}", keys::composer_hint(app)),
            Style::default().fg(theme::DIM),
        ))),
        Rect {
            y: area.y + 1,
            height: 1,
            ..area
        },
    );
    if focused {
        // A real caret, so the human can see where typing lands.
        f.set_cursor_position((area.x + 3 + app.composer.caret_col(), area.y));
    }
}

fn status(f: &mut Frame, area: Rect, app: &App) {
    let mut spans = Vec::new();
    let (glyph, colour, label) = match &app.link {
        Link::Up => (theme::NODE_LIVE, theme::WORKING, String::new()),
        Link::Down { why } => (theme::NODE_BARE, theme::FAILED, format!(" {why}")),
    };
    spans.push(Span::styled(
        format!(" {glyph}{label} "),
        Style::default().fg(colour),
    ));

    if !app.runtime.is_empty() {
        spans.push(Span::styled(
            format!("{} · {} ", app.root_id, app.runtime),
            Style::default().fg(theme::DIM),
        ));
    }
    if let Some(a) = app.selected() {
        if let Some(p) = app.panels.get(a) {
            let cx = app.cx(a);
            if let Some(s) = p.status(&cx) {
                spans.push(Span::styled(
                    format!("· {s} "),
                    Style::default().fg(theme::FG),
                ));
            }
        }
    }
    if app.prefix {
        spans.push(Span::styled(
            "· Ctrl-A: m mouse · d detach · b boot ",
            Style::default()
                .fg(theme::ACCENT_HI)
                .add_modifier(Modifier::BOLD),
        ));
    }
    // THE NEWEST LOUD FAILURE, and how many are behind it. Never a modal, and
    // never dropped: `?` shows the keys, this shows what went wrong.
    if let Some(n) = app.notes.last() {
        let more = if app.notes.len() > 1 {
            format!(" (+{})", app.notes.len() - 1)
        } else {
            String::new()
        };
        spans.push(Span::styled(
            format!("· {n}{more}"),
            Style::default().fg(theme::BLOCKED),
        ));
    }
    let line = Line::from(spans);
    f.render_widget(Paragraph::new(line), area);
}

/// The palette, floated over the right pane. Rendered from the live tree and the
/// selected agent's own reflect, so it is correct for an agent nobody taught this
/// shell about.
fn palette(f: &mut Frame, area: Rect, app: &App) {
    let (title, opts, sel) = match &app.composer.palette {
        Palette::Agents { query, sel } => ("agents", keys::agent_options(app, query), *sel),
        Palette::Verbs { query, sel } => ("verbs", keys::verb_options(app, query), *sel),
        Palette::None => return,
    };
    let h = (opts.len() as u16 + 2).min(area.height).max(3);
    let w = (opts.iter().map(String::len).max().unwrap_or(10) as u16 + 6).min(area.width);
    let rect = Rect {
        x: area.x + 2,
        y: area.y + area.height.saturating_sub(h),
        width: w,
        height: h,
    };
    chrome::fill_black(f.buffer_mut(), rect);
    chrome::draw_box(
        f.buffer_mut(),
        area,
        rect.x as i32,
        rect.y as i32,
        rect.width as i32,
        rect.height as i32,
        Style::default().fg(theme::ACCENT),
    );
    let mut lines = vec![Line::from(Span::styled(
        format!(" {title}"),
        Style::default()
            .fg(theme::LABEL)
            .add_modifier(Modifier::BOLD),
    ))];
    if opts.is_empty() {
        lines.push(Line::from(Span::styled(
            " nothing matches",
            Style::default().fg(theme::DIM),
        )));
    }
    for (i, o) in opts.iter().enumerate() {
        lines.push(Line::from(Span::styled(
            format!(" {o}"),
            if i == sel {
                Style::default()
                    .fg(theme::SELECT)
                    .add_modifier(Modifier::REVERSED)
            } else {
                Style::default().fg(theme::FG)
            },
        )));
    }
    f.render_widget(
        Paragraph::new(lines),
        Rect {
            x: rect.x + 1,
            y: rect.y + 1,
            width: rect.width.saturating_sub(2),
            height: rect.height.saturating_sub(2),
        },
    );
}

/// The help overlay renders the KEY TABLE ITSELF, so it cannot describe a binding
/// that does not exist or miss one that does.
fn help(f: &mut Frame, area: Rect) {
    chrome::fill_black(f.buffer_mut(), area);
    let mut lines = vec![
        Line::from(Span::styled(
            "  keys — any key closes this",
            Style::default()
                .fg(theme::LABEL)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    let mut scope = "";
    for b in keys::TABLE {
        if b.scope != scope {
            scope = b.scope;
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("  {scope}"),
                Style::default()
                    .fg(theme::ACCENT)
                    .add_modifier(Modifier::BOLD),
            )));
        }
        lines.push(Line::from(vec![
            Span::styled(
                format!("    {:<18}", b.keys),
                Style::default().fg(theme::FG),
            ),
            Span::styled(b.does.to_string(), Style::default().fg(theme::DIM)),
        ]));
    }
    f.render_widget(Paragraph::new(lines), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_panel_is_boxed_and_a_click_names_the_panel_under_it() {
        let app = App::new(crate::env::Paths::at(std::path::Path::new("/nonexistent")));
        let a = areas(&app, Rect::new(0, 0, 120, 40));
        assert_eq!(a.tree, inner(a.tree_box), "content sits inside its border");
        assert_eq!(a.page, inner(a.page_box));
        assert_eq!(a.composer, inner(a.composer_box));
        assert_eq!(hit(&a, a.tree_box.x + 2, a.tree_box.y + 3), Hit::Tree);
        assert_eq!(hit(&a, a.page_box.x + 5, a.page_box.y + 5), Hit::Page);
        assert_eq!(hit(&a, a.composer_box.x + 5, a.composer_box.y + 1), Hit::Composer);
        assert_eq!(hit(&a, 0, a.status.y), Hit::None, "the statusline is not a panel");
    }

    #[test]
    fn the_focused_panels_border_wears_the_accent_and_the_rest_do_not() {
        use ratatui::backend::TestBackend;
        let mut app = App::new(crate::env::Paths::at(std::path::Path::new("/nonexistent")));
        let area = Rect::new(0, 0, 100, 30);
        let mut t = ratatui::Terminal::new(TestBackend::new(100, 30)).unwrap();
        for (focus, want) in [(Focus::Tree, 0), (Focus::Panel, 1), (Focus::Composer, 2)] {
            app.focus = focus;
            layout(&mut app, area);
            let buf = t.draw(|f| draw(f, &app)).unwrap().buffer.clone();
            let a = areas(&app, area);
            let corner = |r: Rect| buf.cell((r.x, r.y)).unwrap().fg;
            let boxes = [a.tree_box, a.page_box, a.composer_box];
            for (i, b) in boxes.iter().enumerate() {
                let fg = corner(*b);
                if i == want {
                    assert_eq!(fg, theme::ACCENT_HI, "{focus:?}: box {i} is focused");
                } else {
                    assert_eq!(fg, theme::RULE, "{focus:?}: box {i} is not");
                }
            }
        }
    }
}
