//! ONE KEY TABLE. The help overlay renders THIS, so the help cannot drift from
//! the behaviour — a binding that is not in the table does not exist, and one
//! that is in it is documented by construction.
//!
//! Two exit gestures, both deliberate: `Ctrl-C` twice inside a second, or `q`
//! held down. A single stray `Ctrl-C` never kills a session, and `q` never quits
//! while you are typing.
//!
//! `Ctrl-A` is a prefix, armed for exactly one key, shown in the statusline while
//! it is armed. The dangerous things live behind it.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::app::{App, Focus, Link};
use crate::ev::Cmd;
use crate::panel::{Handled, PanelKind};
use crate::ui::composer::{Mode, Palette};

/// One documented binding.
pub struct Binding {
    pub scope: &'static str,
    pub keys: &'static str,
    pub does: &'static str,
}

/// THE TABLE. `ui::help` renders it verbatim.
pub const TABLE: &[Binding] = &[
    Binding {
        scope: "any",
        keys: "Tab / Shift-Tab",
        does: "cycle focus: tree, panel, composer",
    },
    Binding {
        scope: "any",
        keys: "Ctrl-C ×2",
        does: "quit (two presses within a second)",
    },
    Binding {
        scope: "any",
        keys: "q held",
        does: "quit (not while typing)",
    },
    Binding {
        scope: "any",
        keys: "? / F1",
        does: "this help",
    },
    Binding {
        scope: "any",
        keys: "R",
        does: "re-read everything from the kernel",
    },
    Binding {
        scope: "any",
        keys: "Ctrl-L",
        does: "force a full redraw",
    },
    Binding {
        scope: "any",
        keys: "Ctrl-A m",
        does: "mouse capture (stops native copy)",
    },
    Binding {
        scope: "any",
        keys: "mouse wheel",
        does: "scroll what is under the pointer (tree rows, AI messages); focus stays",
    },
    Binding {
        scope: "any",
        keys: "Ctrl-A d",
        does: "detach and quit; the kernel keeps running",
    },
    Binding {
        scope: "any",
        keys: "Ctrl-A b",
        does: "boot the roster from the manifest",
    },
    Binding {
        scope: "tree",
        keys: "↑ ↓ / k j",
        does: "move",
    },
    Binding {
        scope: "tree",
        keys: "→ l / ← h",
        does: "expand / collapse",
    },
    Binding {
        scope: "tree",
        keys: "Enter",
        does: "select: the right pane becomes this agent's UI",
    },
    Binding {
        scope: "tree",
        keys: "n",
        does: "next agent with an open question",
    },
    Binding {
        scope: "tree",
        keys: "/",
        does: "filter by substring",
    },
    Binding {
        scope: "tree",
        keys: "t",
        does: "open / close a terminal below the page",
    },
    Binding {
        scope: "tree",
        keys: "T",
        does: "type into the open terminal (Esc hands it back)",
    },
    Binding {
        scope: "tree",
        keys: "v",
        does: "sidebar: tree ↔ flat",
    },
    Binding {
        scope: "tree",
        keys: "< / >",
        does: "sidebar: collapse to the rail / expand",
    },
    Binding {
        scope: "tree",
        keys: "g / G",
        does: "first / last",
    },
    Binding {
        scope: "ai",
        keys: "↑ ↓",
        does: "move the outline cursor",
    },
    Binding {
        scope: "ai",
        keys: "Space",
        does: "pin open / unpin — a pin survives the next fold",
    },
    Binding {
        scope: "ai",
        keys: "Enter",
        does: "narrow into a thread, or open a body",
    },
    Binding {
        scope: "ai",
        keys: "Esc",
        does: "widen one level, then leave the pane",
    },
    Binding {
        scope: "ai",
        keys: "n",
        does: "next thread waiting on you",
    },
    Binding {
        scope: "ai",
        keys: "z",
        does: "zoom: all, decisions, roots",
    },
    Binding {
        scope: "ai",
        keys: "v",
        does: "show only what the model can see",
    },
    Binding {
        scope: "ai",
        keys: "S",
        does: "stop this agent's work",
    },
    Binding {
        scope: "terminal",
        keys: "Ctrl-F",
        does: "take over the keyboard",
    },
    Binding {
        scope: "takeover",
        keys: "Esc",
        does: "hand the keyboard back",
    },
    Binding {
        scope: "files",
        keys: "↑ ↓ Enter",
        does: "browse and open",
    },
    Binding {
        scope: "files",
        keys: "← h / r",
        does: "up a directory / refresh",
    },
    Binding {
        scope: "composer",
        keys: "Enter",
        does: "send to the selected agent",
    },
    Binding {
        scope: "composer",
        keys: "@ / /",
        does: "agent palette / verb palette",
    },
    Binding {
        scope: "composer",
        keys: "/q …",
        does: "send explicitly queued behind the running turn",
    },
    Binding {
        scope: "composer",
        keys: "↑ ↓",
        does: "history",
    },
    Binding {
        scope: "composer",
        keys: "Ctrl-U / Ctrl-W",
        does: "kill line / kill word",
    },
    Binding {
        scope: "composer",
        keys: "Esc",
        does: "leave the composer",
    },
];

pub fn handle(app: &mut App, e: Event) {
    match e {
        Event::Key(k) if k.kind == KeyEventKind::Press => key(app, k),
        // A paste is TEXT, never a trigger: it must not spring a palette or run a
        // binding, however it happens to start.
        Event::Paste(s) => {
            if app.focus == Focus::Composer {
                app.composer.insert_str(&s);
                app.dirty = true;
            }
        }
        Event::Resize(_, _) => app.dirty = true,
        Event::Mouse(m) if app.mouse && m.kind == crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
            click(app, m.column, m.row)
        }
        Event::Mouse(m) if app.mouse && matches!(m.kind, crossterm::event::MouseEventKind::ScrollUp | crossterm::event::MouseEventKind::ScrollDown) => {
            wheel(app, m.column, m.row, m.kind == crossterm::event::MouseEventKind::ScrollUp)
        }
        _ => {}
    }
}

fn key(app: &mut App, k: KeyEvent) {
    let now = std::time::Instant::now();
    // ── Ctrl-A, armed for one key ────────────────────────────────────────
    if app.prefix {
        app.prefix = false;
        app.dirty = true;
        match k.code {
            KeyCode::Char('m') => {
                app.mouse = !app.mouse;
                app.note(if app.mouse {
                    "mouse capture ON — your terminal's own selection and copy stop working."
                } else {
                    "mouse capture off — native selection and copy work again."
                });
            }
            KeyCode::Char('d') => {
                // DETACH, not shutdown: the kernel is a server and outlives this
                // client by design.
                app.quit_or_detach();
            }
            KeyCode::Char('b') => app.push(Cmd::RosterBoot),
            _ => app.note("Ctrl-A: m mouse · d detach · b boot the roster"),
        }
        return;
    }
    if k.code == KeyCode::Char('a') && k.modifiers.contains(KeyModifiers::CONTROL) {
        app.prefix = true;
        app.dirty = true;
        return;
    }

    // ── the terminal has the keyboard: EVERYTHING goes to it (Ctrl-C too —
    // it belongs to the program running there), except Esc, which hands the
    // keyboard back.
    if app.focus == Focus::Term {
        term_key(app, k);
        return;
    }

    if quit_gesture(app, k, now) {
        return;
    }

    // ── the help overlay swallows the next key ───────────────────────────
    if app.help {
        app.help = false;
        app.dirty = true;
        return;
    }

    // ── the tree filter is a text field while it is open ─────────────────
    if app.filtering {
        match k.code {
            KeyCode::Esc => {
                app.filtering = false;
                app.filter = None;
                app.reflow();
            }
            KeyCode::Enter => {
                app.filtering = false;
                app.dirty = true;
            }
            KeyCode::Backspace => {
                if let Some(f) = app.filter.as_mut() {
                    f.pop();
                }
                app.reflow();
            }
            KeyCode::Char(c) => {
                app.filter.get_or_insert_with(String::new).push(c);
                app.reflow();
            }
            _ => {}
        }
        return;
    }

    // ── global, in every focus ───────────────────────────────────────────
    match k.code {
        KeyCode::Char('?') | KeyCode::F(1) if app.focus != Focus::Composer => {
            app.help = true;
            app.dirty = true;
            return;
        }
        KeyCode::Char('l') if k.modifiers.contains(KeyModifiers::CONTROL) => {
            app.dirty = true;
            return;
        }
        KeyCode::Char('R') if app.focus != Focus::Composer => {
            app.stale = true;
            app.note("re-reading every agent from the kernel.");
            return;
        }
        KeyCode::Tab => {
            app.focus = match app.focus {
                Focus::Tree => Focus::Panel,
                Focus::Panel => Focus::Composer,
                Focus::Composer | Focus::Term => Focus::Tree,
            };
            app.dirty = true;
            return;
        }
        KeyCode::BackTab => {
            app.focus = match app.focus {
                Focus::Tree => Focus::Composer,
                Focus::Panel | Focus::Term => Focus::Tree,
                Focus::Composer => Focus::Panel,
            };
            app.dirty = true;
            return;
        }
        _ => {}
    }

    match app.focus {
        Focus::Tree => tree_key(app, k),
        Focus::Panel => panel_key(app, k),
        Focus::Composer => composer_key(app, k),
        Focus::Term => term_key(app, k),
    }
}

/// A key while the terminal below the page has the keyboard.
fn term_key(app: &mut App, k: KeyEvent) {
    let id: crate::kern::model::AgentId = crate::kern::terminal::TERM_ID.into();
    let Some(mut p) = app.panels.remove(&id) else {
        app.focus = Focus::Tree;
        return;
    };
    let mut out = Vec::new();
    {
        let cx = app.cx(&id);
        p.on_key(k, &cx, &mut out);
    }
    let still = p
        .as_any_mut()
        .downcast_mut::<crate::panel::terminal::TerminalPanel>()
        .is_some_and(|t| t.takeover);
    app.panels.insert(id, p);
    app.out_extend(out);
    if !still {
        app.focus = Focus::Tree;
    }
    app.dirty = true;
}

/// Two Ctrl-C inside this window exit. One is a cancel, two is a quit.
pub const DOUBLE_PRESS_MS: u64 = 400;
/// Key auto-repeat arrives faster than this; a slower `q` restarts the streak.
pub const Q_REPEAT_MS: u64 = 220;
/// Hold `q` this many repeats to quit.
pub const Q_HOLD_STREAK: u8 = 12;

/// True when this Ctrl-C should exit: the previous one landed inside the
/// double-press window. Pure — the caller owns the clock and the last stamp.
pub fn ctrl_c_exits(last: Option<std::time::Instant>, now: std::time::Instant) -> bool {
    last.is_some_and(|t| now.duration_since(t) < std::time::Duration::from_millis(DOUBLE_PRESS_MS))
}

/// The next `q`-hold streak, given the previous streak and the last `q`. A press
/// within `Q_REPEAT_MS` extends the run (auto-repeat means the key is held); a
/// slower press restarts at 1. The caller exits once this reaches
/// `Q_HOLD_STREAK`. Pure.
pub fn q_hold_streak(prev: u8, last: Option<std::time::Instant>, now: std::time::Instant) -> u8 {
    if last.is_some_and(|t| now.duration_since(t) < std::time::Duration::from_millis(Q_REPEAT_MS)) {
        prev.saturating_add(1)
    } else {
        1
    }
}

/// Two presses of `Ctrl-C` inside a second, or `q` held. Returns true if the key
/// was consumed.
fn quit_gesture(app: &mut App, k: KeyEvent, now: std::time::Instant) -> bool {
    if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
        if ctrl_c_exits(app.last_ctrl_c, now) {
            app.quit_or_detach();
        } else {
            app.note("press Ctrl-C again to quit — the kernel keeps running.");
        }
        app.last_ctrl_c = Some(now);
        app.dirty = true;
        return true;
    }
    // Never while typing: `q` is a letter in the composer and in the filter.
    if k.code == KeyCode::Char('q') && app.focus != Focus::Composer && !app.filtering {
        app.q_streak = q_hold_streak(app.q_streak, app.last_q, now);
        app.last_q = Some(now);
        if app.q_streak >= Q_HOLD_STREAK {
            app.quit_or_detach();
            return true;
        }
    }
    false
}

fn tree_key(app: &mut App, k: KeyEvent) {
    let n = app.trows.len();
    match k.code {
        KeyCode::Down | KeyCode::Char('j') => {
            if n > 0 {
                app.sel = (app.sel + 1).min(n - 1);
            }
        }
        KeyCode::Up | KeyCode::Char('k') => app.sel = app.sel.saturating_sub(1),
        KeyCode::Char('g') => app.sel = 0,
        KeyCode::Char('G') => app.sel = n.saturating_sub(1),
        KeyCode::Right | KeyCode::Char('l') => {
            if let Some(id) = app.selected().cloned() {
                app.collapsed.remove(&id);
                app.reflow();
            }
        }
        KeyCode::Left | KeyCode::Char('h') => {
            if let Some(id) = app.selected().cloned() {
                app.collapsed.insert(id);
                app.reflow();
            }
        }
        KeyCode::Char('t') => app.toggle_term(),
        KeyCode::Char('v') => app.toggle_side(),
        KeyCode::Char('<') => app.rail(true),
        KeyCode::Char('>') => app.rail(false),
        KeyCode::Char('T') if app.term_open() => {
            app.focus = Focus::Term;
            let id: crate::kern::model::AgentId = crate::kern::terminal::TERM_ID.into();
            if let Some(p) = app.panels.get_mut(&id) {
                if let Some(t) = p.as_any_mut().downcast_mut::<crate::panel::terminal::TerminalPanel>() {
                    t.takeover = true;
                }
            }
        }
        KeyCode::Char('/') => {
            app.filtering = true;
            app.filter = Some(String::new());
        }
        // A committed filter is still a filter. Esc in the tree drops it rather
        // than leaving the human with a pane that hides most of the kernel.
        KeyCode::Esc if app.filter.is_some() => {
            app.filter = None;
            app.reflow();
        }
        KeyCode::Char('n') => {
            // The next agent with an open question, wrapping.
            let start = app.sel;
            for step in 1..=n {
                let i = (start + step) % n.max(1);
                if app.trows.get(i).map(|r| r.inquiries > 0 && r.kind != PanelKind::Activity).unwrap_or(false) {
                    app.sel = i;
                    break;
                }
            }
        }
        KeyCode::Enter => {
            if let Some(id) = app.selected().cloned() {
                let kind = app.selected_kind();
                app.select(id.clone(), kind);
                app.focus = Focus::Panel;
                // The terminal's own row: straight into it, so Ctrl-C and the
                // rest reach the program rather than the shell's keymap.
                if id == crate::kern::terminal::TERM_ID {
                    app.focus = Focus::Term;
                    if let Some(p) = app.panels.get_mut(&id) {
                        if let Some(t) = p.as_any_mut().downcast_mut::<crate::panel::terminal::TerminalPanel>() {
                            t.takeover = true;
                        }
                    }
                }
            }
        }
        _ => {}
    }
    app.dirty = true;
}

fn panel_key(app: &mut App, k: KeyEvent) {
    let Some(agent) = app.selected().cloned() else {
        return;
    };
    // The panel is taken out so the borrow of `app` for its context is clean.
    let Some(mut p) = app.panels.remove(&agent) else {
        app.focus = Focus::Tree;
        return;
    };
    let mut out = Vec::new();
    let handled = {
        let cx = app.cx(&agent);
        p.on_key(k, &cx, &mut out)
    };
    let looked = p.seen_now();
    app.panels.insert(agent, p);
    app.mark_seen(looked);
    app.out_extend(out);
    app.dirty = true;
    if handled == Handled::Yes {
        return;
    }
    // A key the panel did not want: only `Esc` means anything here, and it means
    // "give the tree back".
    if k.code == KeyCode::Esc {
        app.focus = Focus::Tree;
    }
}

fn composer_key(app: &mut App, k: KeyEvent) {
    use KeyCode as K;
    // A palette is a completion for the token being typed: Esc closes it, Enter
    // takes the highlighted option, and the arrows move within it instead of
    // walking history. Typing a space closes it (see `Composer::insert`), which is
    // what lets `Enter` mean SEND on a finished line.
    if app.composer.palette != Palette::None {
        let len = palette_len(app);
        match k.code {
            K::Esc => {
                app.composer.palette = Palette::None;
                app.dirty = true;
                return;
            }
            K::Enter => {
                // Nothing matches what was typed, or what was typed IS the
                // option: either way there is nothing left to complete, so Enter
                // means send. Completing an already-complete word and making the
                // human press Enter twice is the kind of surprise that reads as a
                // dropped keystroke.
                if len == 0 || palette_exact(app) {
                    app.composer.palette = Palette::None;
                    app.submit();
                } else {
                    commit_palette(app);
                }
                app.dirty = true;
                return;
            }
            K::Down | K::Up => {
                app.composer
                    .palette_move(if k.code == K::Down { 1 } else { -1 }, len);
                app.dirty = true;
                return;
            }
            _ => {}
        }
    }
    match k.code {
        K::Esc => {
            if app.composer.palette != Palette::None {
                app.composer.palette = Palette::None;
            } else {
                app.focus = Focus::Panel;
            }
        }
        K::Enter if k.modifiers.contains(KeyModifiers::ALT) => app.composer.insert('\n'),
        K::Enter => app.submit(),
        K::Backspace => app.composer.backspace(),
        K::Delete => app.composer.delete(),
        K::Left => app.composer.left(),
        K::Right => app.composer.right(),
        K::Home => app.composer.home(),
        K::End => app.composer.end(),
        K::Up => app.composer.recall_back(),
        K::Down => app.composer.recall_forward(),
        K::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => app.composer.kill_line(),
        K::Char('w') if k.modifiers.contains(KeyModifiers::CONTROL) => app.composer.kill_word(),
        K::Char('e') if k.modifiers.contains(KeyModifiers::CONTROL) => app.composer.end(),
        K::Char(c) => app.composer.insert(c),
        _ => {}
    }
    app.dirty = true;
}

/// Put the palette's choice into the line. The agent palette retargets by writing
/// the id; the verb palette switches the composer into verb mode with the verb
/// already typed, because that is what the human meant by choosing it.
fn commit_palette(app: &mut App) {
    let pick = match &app.composer.palette {
        Palette::Agents { query, sel } => {
            let opts = agent_options(app, query);
            opts.get(*sel).cloned().map(|a| (true, a))
        }
        Palette::Verbs { query, sel } => {
            let opts = verb_options(app, query);
            opts.get(*sel).cloned().map(|v| (false, v))
        }
        Palette::None => None,
    };
    app.composer.palette = Palette::None;
    let Some((is_agent, name)) = pick else { return };
    if is_agent {
        // Move the selection: addressing an agent IS selecting it, so the pane and
        // the message never disagree about who is being talked to.
        if let Some(i) = app.trows.iter().position(|r| r.id == name) {
            app.sel = i;
            let kind = app.trows[i].kind;
            app.select(name, kind);
        }
        // Drop the `@` the human typed to open the palette.
        app.composer.kill_word();
    } else {
        app.composer.kill_word();
        app.composer.mode = Mode::Verb;
        app.composer.insert_str(&format!("/{name} "));
    }
}

/// TRUE when what the human typed is already exactly the highlighted option.
fn palette_exact(app: &App) -> bool {
    let (opts, q, sel) = match &app.composer.palette {
        Palette::Agents { query, sel } => (agent_options(app, query), query, *sel),
        Palette::Verbs { query, sel } => (verb_options(app, query), query, *sel),
        Palette::None => return false,
    };
    opts.get(sel).map(|o| o == q).unwrap_or(false)
}

/// How many options the open palette currently lists.
fn palette_len(app: &App) -> usize {
    match &app.composer.palette {
        Palette::Agents { query, .. } => agent_options(app, query).len(),
        Palette::Verbs { query, .. } => verb_options(app, query).len(),
        Palette::None => 0,
    }
}

pub fn agent_options(app: &App, query: &str) -> Vec<String> {
    let q = query.to_lowercase();
    app.trows
        .iter()
        .filter(|r| r.live && r.id.to_lowercase().contains(&q))
        .map(|r| r.id.clone())
        .collect()
}

pub fn verb_options(app: &App, query: &str) -> Vec<String> {
    let q = query.to_lowercase();
    let Some(a) = app.selected() else {
        return Vec::new();
    };
    crate::panel::generic::GenericPanel::verbs(app.reflects.get(a))
        .into_iter()
        .map(|(k, _)| k)
        .filter(|k| k.to_lowercase().contains(&q))
        .collect()
}

/// The line under the composer. It states the queue truth rather than hiding it.
pub fn composer_hint(app: &App) -> String {
    if app.composer.mode == Mode::Locked {
        return "acts are unavailable: the human's own agent is not installed in this kernel."
            .into();
    }
    if app.link != Link::Up {
        return "the kernel is not answering; nothing can be sent.".into();
    }
    let busy = app
        .selected()
        .map(|a| {
            app.tasks
                .iter()
                .any(|t| &t.agent == a && crate::presence::is_work(t))
        })
        .unwrap_or(false);
    if busy && !app.composer.explicit_queue() {
        // The honest word. A second send does NOT interrupt: it waits behind the
        // turn that holds the backend's lock, linked to it but not steering it.
        return "a turn is running — this will be QUEUED behind it, not an interrupt.".into();
    }
    match app.composer.re {
        Some(r) => format!("answering row {r}"),
        None => "Enter sends · / for a verb · @ for an agent".into(),
    }
}

impl App {
    fn out_extend(&mut self, cs: Vec<Cmd>) {
        for c in cs {
            self.push(c);
        }
    }
}


/// THE WHEEL SCROLLS WHAT IS UNDER THE POINTER, and moves no focus: over the
/// tree it moves the selection, over the page it is the page's own ↑/↓ (the AI
/// panel moves by message). One notch, one step. The terminal and the composer
/// keep their own input.
pub fn wheel(app: &mut App, x: u16, y: u16, up: bool) {
    use crate::ui::frame::{areas, hit, Hit};
    if app.help || app.screen.width == 0 {
        return;
    }
    let k = KeyEvent::new(if up { KeyCode::Up } else { KeyCode::Down }, crossterm::event::KeyModifiers::NONE);
    let a = areas(app, app.screen);
    match hit(&a, x, y) {
        Hit::Tree => tree_key(app, k),
        Hit::Page if app.selected().is_some_and(|id| app.panels.contains_key(id)) => panel_key(app, k),
        _ => return,
    }
    app.dirty = true;
}

/// A LEFT CLICK FOCUSES WHERE IT LANDS: the sidebar (and a row there is
/// selected and opened, like Enter), the page, the terminal (it takes the
/// keyboard) or the composer.
pub fn click(app: &mut App, x: u16, y: u16) {
    use crate::ui::frame::{areas, hit, Hit};
    if app.help || app.screen.width == 0 {
        return;
    }
    let a = areas(app, app.screen);
    match hit(&a, x, y) {
        Hit::Tree => {
            app.focus = Focus::Tree;
            let row = crate::ui::tree::row_at(
                app.trows.len(),
                app.sel,
                a.tree,
                app.filter.is_some(),
                app.side,
                app.system_folded,
                y,
            );
            if let Some(i) = row {
                app.sel = i;
                let (id, kind) = (app.trows[i].id.clone(), app.trows[i].kind);
                app.select(id, kind);
            }
        }
        Hit::Page => app.focus = Focus::Panel,
        Hit::Term => {
            app.focus = Focus::Term;
            let id: crate::kern::model::AgentId = crate::kern::terminal::TERM_ID.into();
            if let Some(p) = app.panels.get_mut(&id) {
                if let Some(t) = p.as_any_mut().downcast_mut::<crate::panel::terminal::TerminalPanel>() {
                    t.takeover = true;
                }
            }
        }
        Hit::Composer => app.focus = Focus::Composer,
        Hit::None => {}
    }
    app.dirty = true;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_binding_names_a_scope_a_key_and_an_effect() {
        assert!(TABLE.len() > 20);
        for b in TABLE {
            assert!(!b.scope.is_empty() && !b.keys.is_empty() && !b.does.is_empty());
            // The help is prose a human reads, so no glyph-only entries.
            assert!(
                b.does.chars().any(|c| c.is_ascii_lowercase()),
                "{:?} explains nothing",
                b.keys
            );
        }
    }

    #[test]
    fn ctrl_c_exits_only_inside_the_window() {
        use std::time::{Duration, Instant};
        let now = Instant::now();
        assert!(!ctrl_c_exits(None, now));
        assert!(ctrl_c_exits(Some(now - Duration::from_millis(DOUBLE_PRESS_MS / 2)), now));
        assert!(!ctrl_c_exits(Some(now - Duration::from_millis(DOUBLE_PRESS_MS + 50)), now));
    }

    #[test]
    fn q_hold_streak_counts_fast_repeats() {
        use std::time::{Duration, Instant};
        let now = Instant::now();
        assert_eq!(q_hold_streak(0, None, now), 1);
        assert_eq!(q_hold_streak(3, Some(now - Duration::from_millis(Q_REPEAT_MS / 2)), now), 4);
        assert_eq!(q_hold_streak(9, Some(now - Duration::from_millis(Q_REPEAT_MS * 3)), now), 1);
    }

    #[test]
    fn the_wheel_moves_the_selection_under_the_pointer_and_never_the_focus() {
        use crate::kern::model::TreeNode;
        let leaf = |id: &str| TreeNode { id: id.into(), parent_id: None, handler_module: Some("x.tools".into()), display_name: None, description: None, children: vec![] };
        let mut app = App::new(crate::env::Paths::at(std::path::Path::new("/nonexistent")));
        app.screen = ratatui::layout::Rect::new(0, 0, 120, 40);
        app.tree = Some(TreeNode { children: vec![leaf("a"), leaf("b"), leaf("c")], ..leaf("root") });
        app.reflow();
        app.focus = Focus::Composer;
        let a = crate::ui::frame::areas(&app, app.screen);
        let (x, y) = (a.tree.x + 2, a.tree.y + 2);
        let before = app.sel;
        wheel(&mut app, x, y, false);
        assert_eq!(app.sel, before + 1, "a notch down moves the selection one row");
        wheel(&mut app, x, y, true);
        assert_eq!(app.sel, before);
        assert_eq!(app.focus, Focus::Composer, "scrolling moves no focus");
        // over the statusline nothing happens
        wheel(&mut app, 1, a.status.y, false);
        assert_eq!(app.sel, before);
    }

    #[test]
    fn the_table_documents_the_gestures_that_can_lose_work() {
        let all: String = TABLE
            .iter()
            .map(|b| format!("{} {}", b.keys, b.does))
            .collect();
        assert!(all.contains("Ctrl-C"), "quitting must be documented");
        assert!(all.contains("detach"), "so must detaching");
        assert!(all.contains("mouse"), "and the copy-breaking toggle");
    }
}
