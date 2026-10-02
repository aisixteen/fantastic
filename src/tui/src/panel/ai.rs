//! THE CONVERSATION, as the human reads it and as the MODEL sees it, on one
//! screen.
//!
//! Two things make this pane different from every other chat view:
//!
//!   1. **Threads are parsed, not stored.** The model never emits a thread id and
//!      never knows this structure exists. Parents come from row links, folds come
//!      from the kernel's own fold rows, and the outline is a projection over the
//!      log. Change the projection and nothing about the model changes.
//!
//!   2. **The gutter is the model's own view row, verbatim.** Every line carries a
//!      mark saying whether the model sees it whole, abbreviated, as a fold, or not
//!      at all. Nothing here is inferred from token counts or guessed from
//!      position, so the human can see exactly what the model is working from —
//!      which is the difference between reading a transcript and understanding a
//!      context.
//!
//! Folded by default: a finished root is one message. A coding session that ran
//! forty tool calls reads as the request it answered, and opening it is one key.
//!
//! A TRANSCRIPT: every message shows its whole text (`OutlineLine::body`),
//! word-wrapped to the pane — a coding-CLI read, not a clipped list — and the
//! cursor and the scroll move by message. The model's reasoning is never shown:
//! while a turn is in flight the pane flashes one `thinking` line instead of the
//! token stream; the answer arrives as its row.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use serde_json::{json, Value};

use crate::ev::Cmd;
use crate::kern::model::{AgentId, RowId};
use crate::panel::{Cx, Handled, Needs, Panel, PanelKind};
use crate::theme;
use crate::thread::{self, LineKind, Outline, OutlineState, Waiting, Zoom};

/// (row, width) → (the text's hash, its wrapped lines).
type WrapMemo = std::collections::HashMap<(RowId, usize), (u64, std::rc::Rc<Vec<String>>)>;

/// What one View derives for this panel: its window into the thread and the
/// wrap memo for its width (`Panel::take_view`).
#[derive(Default)]
struct AiView {
    top: usize,
    wraps: WrapMemo,
    wraps_w: u16,
}

pub struct AiPanel {
    agent: AgentId,
    pub st: OutlineState,
    /// First visible line. Kept as an OFFSET because it is a window into the
    /// rendered list, while the cursor is a row id.
    pub top: usize,
    /// A turn is in flight (a `thinking`/`streaming`/`tool_calling` status, or
    /// tokens arriving): the pane flashes `thinking` — never the token stream.
    pub live: bool,
    /// When the turn in flight began, for the indicator's clock.
    since: Option<std::time::Instant>,
    /// Seconds since `since`, taken in `layout` so `render` stays a pure view.
    clock: f32,
    /// The kernel's own phase for this agent, from its status frames. Never
    /// inferred: `queued` with `ahead` is the honest word for a second send.
    pub phase: Option<String>,
    pub ahead: Option<i64>,
    /// The body a human opened in the pager.
    pub body: Option<(RowId, String)>,
    /// The outline as of the last render, so keys can act on what is on screen.
    last: Outline,
    /// What `last` was projected from (`proj_key`): a frame re-projects only
    /// when the rows, the task table or the fold state changed — never per frame.
    proj_key: Option<u64>,
    /// Each message's wrapped text, per (row, width), keyed by its text: a
    /// frame wraps only what changed. A memo, not state — `render` may fill it.
    wraps: std::cell::RefCell<WrapMemo>,
    /// The pane width the memo was filled at: a resize empties it, so it never
    /// accumulates one copy per width.
    wraps_w: u16,
    waiting: Vec<Waiting>,
}

impl AiPanel {
    pub fn new(agent: AgentId) -> Self {
        Self {
            agent,
            st: OutlineState::default(),
            top: 0,
            live: false,
            since: None,
            clock: 0.0,
            phase: None,
            ahead: None,
            body: None,
            last: Outline::default(),
            proj_key: None,
            wraps: Default::default(),
            wraps_w: 0,
            waiting: Vec::new(),
        }
    }

    pub fn project(&mut self, cx: &Cx<'_>) -> Outline {
        self.waiting = thread::waiting_for(cx.store, cx.agent, crate::env::HUMAN);
        let o = thread::outline(thread::Args {
            store: cx.store,
            agent: cx.agent,
            tasks: cx.tasks,
            waiting: &self.waiting,
            seen: cx.seen,
            st: &self.st,
        });
        // A fold can swallow the row the cursor was on. Lifting it to whatever
        // covers it is the only behaviour that does not lose the human's place.
        self.st.cursor = thread::repair_cursor(&o, self.st.cursor);
        o
    }

    /// A turn is in flight: start the indicator's clock (once per turn).
    fn go_live(&mut self) {
        if !self.live {
            self.live = true;
            self.since = Some(std::time::Instant::now());
        }
    }

    /// What the projection depends on: the rows (`Store::rev`), the task table,
    /// and the fold state. The cursor is not in it — moving never re-projects.
    fn proj_key(&self, cx: &Cx<'_>) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        cx.store.rev.hash(&mut h);
        for t in cx.tasks {
            (t.id, &t.agent, t.parent).hash(&mut h);
        }
        let mut pins: Vec<_> = self.st.pins.iter().collect();
        pins.sort();
        let mut open: Vec<_> = self.st.open.iter().collect();
        open.sort();
        (pins, open, self.st.zoom as u8, self.st.model_view_only, self.st.narrow).hash(&mut h);
        h.finish()
    }

    /// Re-project only when the key moved; the cursor is repaired either way.
    fn refresh(&mut self, cx: &Cx<'_>) {
        let key = self.proj_key(cx);
        if self.proj_key != Some(key) {
            self.last = self.project(cx);
            self.proj_key = Some(key);
        } else {
            self.st.cursor = thread::repair_cursor(&self.last, self.st.cursor);
        }
    }

    /// `text` wrapped to `w`, from the memo when the row's text is unchanged.
    fn wrapped(&self, id: RowId, w: usize, text: &str) -> std::rc::Rc<Vec<String>> {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut h);
        let sig = h.finish();
        let mut memo = self.wraps.borrow_mut();
        if let Some((s, lines)) = memo.get(&(id, w)) {
            if *s == sig {
                return std::rc::Rc::clone(lines);
            }
        }
        let lines = std::rc::Rc::new(crate::ui::chrome::wrap(text, w));
        memo.insert((id, w), (sig, std::rc::Rc::clone(&lines)));
        lines
    }

    /// The row the cursor is on, or the first line.
    fn at(&self) -> Option<RowId> {
        self.at_in(&self.last)
    }

    /// The same, against an outline that is not stored yet — `layout` resolves
    /// the cursor on the NEW projection before it replaces the old one.
    fn at_in(&self, o: &Outline) -> Option<RowId> {
        self.st.cursor.or_else(|| o.lines.first().map(|l| l.id))
    }

    fn move_by(&mut self, d: isize) {
        if self.last.lines.is_empty() {
            return;
        }
        let cur = self.at().and_then(|id| self.last.position(id)).unwrap_or(0) as isize;
        let n = self.last.lines.len() as isize;
        let next = (cur + d).clamp(0, n - 1) as usize;
        self.st.cursor = Some(self.last.lines[next].id);
    }

    /// `Space`: pin or unpin. A PIN IS THE HUMAN'S, and it survives the next fold —
    /// otherwise the kernel's summarizer would keep closing the thing you are
    /// reading.
    fn toggle_pin(&mut self) {
        let Some(id) = self.at() else { return };
        if self.st.pins.remove(&id) {
            self.st.open.remove(&id);
        } else {
            self.st.pins.insert(id);
            self.st.open.insert(id);
        }
    }

    /// The next open question to the human. Derived from rows, so it is correct
    /// without anything being tracked. It never meant "stopped": the thread
    /// carrying the question keeps going.
    fn next_asking(&mut self) {
        let start = self.at().and_then(|id| self.last.position(id)).unwrap_or(0);
        let n = self.last.lines.len();
        for step in 1..=n {
            let i = (start + step) % n.max(1);
            if let Some(l) = self.last.lines.get(i) {
                if l.asking {
                    self.st.cursor = Some(l.id);
                    return;
                }
            }
        }
    }

    /// `S`: stop every non-infrastructure task under this agent. The kernel kills
    /// the subtree post-order, so one cancel per root task is enough.
    fn stop(&self, cx: &Cx<'_>, out: &mut Vec<Cmd>) {
        for t in cx.tasks {
            if &t.agent == cx.agent && crate::presence::is_work(t) {
                out.push(Cmd::Cancel { task_id: t.id });
            }
        }
    }

    /// The gutter legend. It names the marks in the model's own words, because a
    /// glyph nobody can decode is decoration.
    fn header<'a>(&self, o: &Outline) -> Vec<Line<'a>> {
        let mut out = Vec::new();
        match &o.view {
            Some(v) => {
                out.push(Line::from(vec![
                    Span::styled("  context  ", Style::default().fg(theme::LABEL)),
                    Span::styled(
                        format!("{} rows", v.rows),
                        Style::default().fg(theme::FG).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        if v.budget > 0 {
                            format!("  ·  budget {}", v.budget)
                        } else {
                            String::new()
                        },
                        Style::default().fg(theme::DIM),
                    ),
                    Span::styled(
                        if v.why.is_empty() {
                            String::new()
                        } else {
                            format!("  ·  {}", v.why)
                        },
                        Style::default().fg(theme::DIM),
                    ),
                ]));
                out.push(Line::from(Span::styled(
                    format!(
                        "  {} in full   {} shortened   {} folded   {} not in context",
                        theme::MARK_FULL,
                        theme::MARK_STUB,
                        theme::MARK_FOLD,
                        theme::MARK_OUT
                    ),
                    Style::default().fg(theme::DIM),
                )));
            }
            None => out.push(Line::from(Span::styled(
                "  context  the model has written no projection yet, so every row rides in full",
                Style::default().fg(theme::DIM),
            ))),
        }
        out.push(Line::from(""));
        out
    }

    /// The text a message shows: its whole body for a turn or a question, the
    /// one-line head for plumbing (folds, recalls, other kinds) and whenever the
    /// body is empty.
    fn text_of(l: &thread::OutlineLine) -> &str {
        match &l.kind {
            LineKind::Turn { .. } | LineKind::Waiting | LineKind::Late if !l.body.is_empty() => &l.body,
            _ => &l.head,
        }
    }

    /// One message as the lines it takes at `width`: the gutter (the model's own
    /// view mark), then the text word-wrapped — the first line after its bullet,
    /// tag and sender, the rest indented under them.
    fn block_of<'a>(&self, l: &thread::OutlineLine, cursor: bool, width: u16) -> Vec<Line<'a>> {
        let (glyph, gcol) = theme::mark(l.mark);
        let indent = "  ".repeat(l.depth as usize);
        let bullet = match (&l.kind, l.expandable, l.expanded) {
            (LineKind::Fold { .. }, _, false) => theme::FOLDED,
            (LineKind::Fold { .. }, _, true) => theme::OPENED,
            (_, true, false) => theme::FOLDED,
            (_, true, true) => theme::OPENED,
            _ => ' ',
        };
        let tag = match &l.kind {
            LineKind::Fold { level, refs } => format!("fold L{level} · {refs} "),
            LineKind::Recall => "recall ".into(),
            LineKind::Late => "late ".into(),
            LineKind::Waiting => "asks you ".into(),
            LineKind::Turn { .. } => String::new(),
            LineKind::Other(k) => format!("{k} "),
        };
        // WHAT THE LINE IS decides its hue (theme's SAY_*); a failed thread's
        // text reads red whatever it is. The bullet and bar keep the state.
        let text_col = match (&l.kind, l.reply, l.closure) {
            (_, _, thread::Closure::Failed) => theme::FAILED,
            (LineKind::Fold { .. } | LineKind::Recall | LineKind::Other(_), _, _) => theme::SAY_PLUMBING,
            (_, true, _) => theme::SAY_REPLY,
            _ => theme::FG,
        };
        let mut style = Style::default().fg(text_col);
        if l.pinned {
            style = style.add_modifier(Modifier::BOLD);
        }
        let state = Style::default().fg(theme::closure(l.closure));
        // Who said it: the vouched sender, else — for the model's own turn — this
        // agent's id. Its label wears the speaker's hue.
        let (from, from_col) = match (&l.from, &l.kind) {
            (Some(f), _) if f == crate::env::HUMAN => (format!("{f}: "), theme::SAY_HUMAN),
            (Some(f), _) if f == &self.agent => (format!("{f}: "), theme::SAY_SELF),
            (Some(f), _) => (format!("{f}: "), theme::SAY_OTHER),
            (None, LineKind::Turn { assistant: true }) => (format!("{}: ", self.agent), theme::SAY_SELF),
            _ => (String::new(), theme::FG),
        };
        // THE QUESTION BADGE on a root that carries one: the inquiry colour, the
        // envelope — beside the thread's own state, never instead of it.
        let badge = l.asking && l.depth == 0;
        // Width is a render concern, handled here and nowhere in the projection:
        // gutter (3) + badge (2) + indent + bullet and space (2).
        let lead = 3 + if badge { 2 } else { 0 } + indent.chars().count() + 2;
        let avail = (width as usize).saturating_sub(lead + 1).max(8);
        let text = format!("{tag}{from}{}", Self::text_of(l));
        let wrapped = self.wrapped(l.id, avail, &text);
        let bar = if cursor { "▌" } else { " " };
        let mut out = Vec::with_capacity(wrapped.len());
        for (n, piece) in wrapped.iter().enumerate() {
            let mut spans = Vec::new();
            spans.push(Span::styled(bar, Style::default().fg(theme::ACCENT_HI)));
            if n == 0 {
                spans.push(Span::styled(format!("{glyph} "), Style::default().fg(gcol)));
                if badge {
                    spans.push(Span::styled("✉ ", Style::default().fg(theme::BLOCKED).add_modifier(Modifier::BOLD)));
                }
                spans.push(Span::styled(format!("{indent}{bullet} "), state));
            } else {
                spans.push(Span::raw(" ".repeat(lead - 1)));
            }
            let rev = |st: Style| if cursor && n == 0 { st.add_modifier(Modifier::REVERSED) } else { st };
            let mut rest: &str = piece;
            if n == 0 {
                if let Some(r) = rest.strip_prefix(tag.as_str()).filter(|_| !tag.is_empty()) {
                    spans.push(Span::styled(tag.clone(), rev(Style::default().fg(theme::LABEL))));
                    rest = r;
                }
                if let Some(r) = rest.strip_prefix(from.as_str()).filter(|_| !from.is_empty()) {
                    spans.push(Span::styled(from.clone(), rev(Style::default().fg(from_col).add_modifier(Modifier::BOLD))));
                    rest = r;
                }
            }
            // a tool call, as `body_of` wrote it
            let st = if rest.starts_with("▸ ") { Style::default().fg(theme::SAY_CALL) } else { style };
            spans.push(Span::styled(rest.to_string(), rev(st)));
            out.push(Line::from(spans));
        }
        out
    }

    /// Lines a message takes on screen at `width`, the spacer above a new root
    /// included (none above the first one shown).
    fn height_of(&self, l: &thread::OutlineLine, first: bool, width: u16) -> usize {
        self.block_of(l, false, width).len() + usize::from(!first && l.depth == 0)
    }

    /// THE THINKING LINE: a braille spinner, the word with a shimmer travelling
    /// through it, the seconds so far. Driven by `clock` (set in `layout`).
    fn thinking_line<'a>(&self) -> Line<'a> {
        const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        const WORD: &str = "thinking";
        let t = self.clock;
        let frame = SPIN[(t * 12.0) as usize % SPIN.len()];
        let n = WORD.chars().count() as f32;
        let at = (t * 9.0) % (n + 6.0) - 3.0; // the shimmer's centre, off both ends
        let mut spans = vec![
            Span::raw("  "),
            Span::styled(format!("{frame} "), Style::default().fg(theme::ACCENT_HI).add_modifier(Modifier::BOLD)),
        ];
        for (i, ch) in WORD.chars().enumerate() {
            let d = (i as f32 - at).abs();
            let st = if d < 0.8 {
                Style::default().fg(theme::ACCENT_HI).add_modifier(Modifier::BOLD)
            } else if d < 2.0 {
                Style::default().fg(theme::FG)
            } else {
                Style::default().fg(theme::DIM)
            };
            spans.push(Span::styled(ch.to_string(), st));
        }
        spans.push(Span::styled(format!("  {}s", t as u64), Style::default().fg(theme::DIM)));
        Line::from(spans)
    }
}

impl Panel for AiPanel {
    fn kind(&self) -> PanelKind {
        PanelKind::Ai
    }

    fn needs(&self) -> Needs {
        Needs {
            rows: true,
            reflect: true,
            watch: true,
        }
    }

    fn on_key(
        &mut self,
        k: crossterm::event::KeyEvent,
        cx: &Cx<'_>,
        out: &mut Vec<Cmd>,
    ) -> Handled {
        use crossterm::event::KeyCode as K;
        if self.body.is_some() {
            if matches!(k.code, K::Esc | K::Char('q')) {
                self.body = None;
                return Handled::Yes;
            }
            return Handled::No;
        }
        match k.code {
            K::Down | K::Char('j') => {
                self.move_by(1);
                Handled::Yes
            }
            K::Up | K::Char('k') => {
                self.move_by(-1);
                Handled::Yes
            }
            K::PageDown => {
                self.move_by(15);
                Handled::Yes
            }
            K::PageUp => {
                self.move_by(-15);
                Handled::Yes
            }
            K::Char(' ') => {
                self.toggle_pin();
                Handled::Yes
            }
            K::Enter => {
                let Some(id) = self.at() else {
                    return Handled::Yes;
                };
                let expandable = self
                    .last
                    .lines
                    .iter()
                    .find(|l| l.id == id)
                    .map(|l| l.expandable)
                    .unwrap_or(false);
                if expandable {
                    // Narrow: the subtree becomes the whole pane, with a
                    // breadcrumb out.
                    self.st.narrow = Some(id);
                    self.st.open.insert(id);
                } else if let Some(bid) = cx.store.body_of.get(&id) {
                    let text = cx
                        .store
                        .rows
                        .get(bid)
                        .and_then(|r| r.content.clone().or_else(|| r.body.clone()))
                        .unwrap_or_default();
                    self.body = Some((id, text));
                } else if let Some(r) = cx.store.rows.get(&id) {
                    self.body = Some((id, r.content.clone().unwrap_or_default()));
                }
                Handled::Yes
            }
            K::Esc => {
                if self.st.narrow.take().is_some() {
                    Handled::Yes
                } else {
                    Handled::No // let focus leave the panel
                }
            }
            K::Char('n') => {
                self.next_asking();
                Handled::Yes
            }
            K::Char('z') => {
                self.st.zoom = match self.st.zoom {
                    Zoom::All => Zoom::Decisions,
                    Zoom::Decisions => Zoom::Roots,
                    Zoom::Roots => Zoom::All,
                };
                Handled::Yes
            }
            K::Char('v') => {
                self.st.model_view_only = !self.st.model_view_only;
                Handled::Yes
            }
            K::Char('S') => {
                self.stop(cx, out);
                Handled::Yes
            }
            _ => Handled::No,
        }
    }

    fn on_event(&mut self, payload: &Value, _out: &mut Vec<Cmd>) -> bool {
        match payload.get("type").and_then(Value::as_str) {
            // A token means a turn is in flight. Its TEXT is never shown: the
            // durable answer is the row, and the reasoning stays the model's.
            Some("token") => {
                let was = self.live;
                self.go_live();
                !was
            }
            Some("status") => {
                let phase = payload
                    .get("phase")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                self.ahead = payload
                    .get("detail")
                    .and_then(|d| d.get("ahead"))
                    .and_then(Value::as_i64);
                match phase.as_deref() {
                    Some("thinking" | "streaming" | "tool_calling") => self.go_live(),
                    _ => {
                        self.live = false;
                        self.since = None;
                    }
                }
                self.phase = phase;
                true
            }
            // A compaction happened. The gutter is about to change, so the pane
            // must re-read rather than keep rendering a stale projection.
            Some("context") => true,
            _ => false,
        }
    }

    fn on_select(&mut self, _cx: &Cx<'_>, out: &mut Vec<Cmd>) {
        out.push(Cmd::Reflect {
            agent: self.agent.clone(),
        });
        out.push(Cmd::Rows {
            agent: self.agent.clone(),
            full: true,
            after: None,
        });
        out.push(Cmd::Watch(self.agent.clone()));
    }

    fn seen_now(&self) -> Vec<RowId> {
        // The line under the cursor, and the body the human opened. Not the whole
        // visible page: scrolling past something is not reading it.
        self.st
            .cursor
            .into_iter()
            .chain(self.body.as_ref().map(|(id, _)| *id))
            .collect()
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn animating(&self) -> bool {
        self.live
    }

    fn take_view(&mut self) -> Option<Box<dyn std::any::Any>> {
        Some(Box::new(AiView {
            top: std::mem::take(&mut self.top),
            wraps: std::mem::take(&mut *self.wraps.borrow_mut()),
            wraps_w: std::mem::take(&mut self.wraps_w),
        }))
    }

    fn put_view(&mut self, view: Box<dyn std::any::Any>) {
        if let Ok(v) = view.downcast::<AiView>() {
            self.top = v.top;
            *self.wraps.borrow_mut() = v.wraps;
            self.wraps_w = v.wraps_w;
        }
    }

    fn layout(&mut self, area: Rect, cx: &Cx<'_>) {
        self.clock = self.since.map_or(0.0, |t| t.elapsed().as_secs_f32());
        if area.width != self.wraps_w {
            self.wraps.borrow_mut().clear();
            self.wraps_w = area.width;
        }
        self.refresh(cx);
        if self.body.is_none() && !self.last.lines.is_empty() {
            let o = &self.last;
            let head_h = self.header(o).len();
            let live_h = if self.live { 2 } else { 0 };
            let body_h = (area.height as usize).saturating_sub(head_h + live_h);
            // Keep the cursor's WHOLE message on screen — messages have heights —
            // scrolling as little as it takes. O(what is on screen): walk UP from
            // the cursor until the screen is full, never over the whole thread.
            let cur = self.at_in(o).and_then(|id| o.position(id)).unwrap_or(0);
            let mut top = self.top.min(o.lines.len().saturating_sub(1));
            if cur < top {
                top = cur;
            } else {
                let mut used = 0usize;
                let mut i = cur;
                loop {
                    // as the first message shown (no spacer above it)
                    let h = self.height_of(&o.lines[i], true, area.width);
                    if used + h > body_h && i < cur {
                        top = top.max(i + 1);
                        break;
                    }
                    if i <= top {
                        break;
                    }
                    used += self.height_of(&o.lines[i], false, area.width);
                    i -= 1;
                }
            }
            self.top = top;
        }
    }

    fn render(&self, f: &mut Frame<'_>, area: Rect, _cx: &Cx<'_>) {
        let o = &self.last;

        if let Some((id, text)) = &self.body {
            let mut lines = vec![Line::from(Span::styled(
                format!("  row {id} · Esc closes"),
                Style::default()
                    .fg(theme::LABEL)
                    .add_modifier(Modifier::BOLD),
            ))];
            for l in text.lines() {
                lines.push(Line::from(Span::styled(
                    l.to_string(),
                    Style::default().fg(theme::FG),
                )));
            }
            f.render_widget(Paragraph::new(lines), area);
            return;
        }

        let mut lines = self.header(o);
        let head_h = lines.len();
        let live_h = if self.live { 2 } else { 0 };
        let body_h = (area.height as usize).saturating_sub(head_h + live_h);

        if o.lines.is_empty() {
            lines.push(Line::from(Span::styled(
                "  nothing yet. Type below to start a thread.",
                Style::default().fg(theme::DIM),
            )));
        }
        let mut used = 0usize;
        for (k, l) in o.lines.iter().skip(self.top).enumerate() {
            if used >= body_h {
                break;
            }
            if k > 0 && l.depth == 0 {
                lines.push(Line::from(""));
                used += 1;
            }
            let is_cur = Some(l.id) == self.at();
            for line in self.block_of(l, is_cur, area.width) {
                if used >= body_h {
                    break;
                }
                lines.push(line);
                used += 1;
            }
        }

        if self.live {
            // pinned to the bottom of the pane, under the transcript
            while lines.len() + 2 < area.height as usize {
                lines.push(Line::from(""));
            }
            lines.push(Line::from(""));
            lines.push(self.thinking_line());
        }

        f.render_widget(Paragraph::new(lines), area);
    }

    fn status(&self, _cx: &Cx<'_>) -> Option<String> {
        let mut bits = vec![self.agent.to_string()];
        // THE HONEST WORD for a second send: it is queued behind a turn that holds
        // the backend's lock. It is linked to what is running; it does not steer it.
        match (self.phase.as_deref(), self.ahead) {
            (Some("queued"), Some(n)) => bits.push(format!("queued · ahead {n}")),
            (Some(p), _) => bits.push(p.to_string()),
            (None, _) => {}
        }
        if !self.waiting.is_empty() {
            bits.push(format!("{}{}", theme::INQUIRY, self.waiting.len()));
        }
        bits.push(
            match self.st.zoom {
                Zoom::All => "all",
                Zoom::Decisions => "decisions",
                Zoom::Roots => "roots",
            }
            .to_string(),
        );
        if self.st.model_view_only {
            bits.push("model view".into());
        }
        Some(bits.join(" · "))
    }
}

/// The payload a human's message becomes. `re` is what threads an answer onto the
/// row that asked, and it is the ONLY mechanism: there is no answer verb, because
/// nothing was ever parked waiting for one.
pub fn say_payload(to: &AgentId, text: &str, client_id: &str, re: Option<RowId>) -> Value {
    let mut p = json!({
        "type": "send",
        "text": text,
        "client_id": client_id,
        "to": [to],
    });
    if let Some(r) = re {
        p["re"] = json!(r);
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn store_with(rows: Vec<Value>) -> Store {
        let mut s = Store::default();
        s.apply_full(
            &"ai".to_string(),
            rows.into_iter()
                .map(|v| serde_json::from_value(v).unwrap())
                .collect(),
        );
        s
    }

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    #[test]
    fn a_message_carries_the_client_id_and_threads_with_re() {
        let p = say_payload(&"ai".into(), "use auth-v2", "tui-abc123", Some(423));
        assert_eq!(p["client_id"], "tui-abc123");
        assert_ne!(
            p["client_id"], "cli",
            "`cli` is the kernel's own internal id"
        );
        assert_eq!(p["re"], 423);
        assert_eq!(p["to"][0], "ai");
        let plain = say_payload(&"ai".into(), "hi", "tui-abc123", None);
        assert!(plain.get("re").is_none(), "a new thread points at nothing");
    }

    #[test]
    fn a_pin_survives_and_unpinning_closes() {
        let s = store_with(vec![
            json!({"id":1,"agent":"ai","role":"user","content":"a"}),
            json!({"id":2,"agent":"ai","role":"assistant","parent":1,"content":"b"}),
        ]);
        let mut p = AiPanel::new("ai".into());
        let seen = std::collections::HashSet::new();
        let a: AgentId = "ai".into();
        let cx = Cx {
            agent: &a,
            store: &s,
            tasks: &[],
            seen: &seen,
            reflect: None,
        };
        p.last = p.project(&cx);
        p.st.cursor = Some(1);
        let mut out = Vec::new();
        p.on_key(key(KeyCode::Char(' ')), &cx, &mut out);
        assert!(p.st.pins.contains(&1) && p.st.open.contains(&1));
        p.on_key(key(KeyCode::Char(' ')), &cx, &mut out);
        assert!(!p.st.pins.contains(&1) && !p.st.open.contains(&1));
    }

    /// Render `p` into an off-screen terminal and return the text it drew.
    fn draw_once(p: &AiPanel, cx: &Cx<'_>, w: u16, h: u16) -> String {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        term.draw(|f| p.render(f, f.area(), cx)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn render_is_a_pure_view_layout_is_where_state_changes() {
        // MVVM, pinned: `layout` derives, `render` only reads. Two draws of the
        // same state are identical and leave the panel exactly as it was.
        let s = store_with((1..=30).map(|i| json!({"id":i,"agent":"ai","role":"user","content":format!("m{i}")})).collect());
        let mut p = AiPanel::new("ai".into());
        let seen = std::collections::HashSet::new();
        let a: AgentId = "ai".into();
        let cx = Cx { agent: &a, store: &s, tasks: &[], seen: &seen, reflect: None };
        let area = Rect::new(0, 0, 60, 10);
        p.layout(area, &cx);
        let (top, cur, n) = (p.top, p.st.cursor, p.last.lines.len());
        let first = draw_once(&p, &cx, 60, 10);
        let second = draw_once(&p, &cx, 60, 10);
        assert_eq!(first, second, "a second draw of the same state must be identical");
        assert_eq!((p.top, p.st.cursor, p.last.lines.len()), (top, cur, n), "render changed state");
    }

    #[test]
    fn an_open_question_is_a_badge_not_a_stopped_thread() {
        // A question to the human never blocks the conversation: its thread is
        // drawn in its own state, with the envelope beside it.
        let s = store_with(vec![json!({"id":1,"agent":"ai","content":"[from other · msg 1]\nwhich port?"})]);
        let mut p = AiPanel::new("ai".into());
        let seen = std::collections::HashSet::new();
        let a: AgentId = "ai".into();
        let w = thread::waiting_for(&s, &a, crate::env::HUMAN);
        assert_eq!(w.len(), 1);
        let cx = Cx { agent: &a, store: &s, tasks: &[], seen: &seen, reflect: None };
        p.layout(Rect::new(0, 0, 70, 10), &cx);
        assert!(p.last.lines[0].asking);
        let drawn = draw_once(&p, &cx, 70, 10);
        assert!(drawn.contains('✉'), "the question badge is drawn:\n{drawn}");
        assert!(drawn.contains("which port?"));
    }

    #[test]
    fn layout_keeps_the_cursor_on_screen() {
        let s = store_with((1..=30).map(|i| json!({"id":i,"agent":"ai","role":"user","content":format!("m{i}")})).collect());
        let mut p = AiPanel::new("ai".into());
        let seen = std::collections::HashSet::new();
        let a: AgentId = "ai".into();
        let cx = Cx { agent: &a, store: &s, tasks: &[], seen: &seen, reflect: None };
        let area = Rect::new(0, 0, 60, 8);
        p.layout(area, &cx);
        // Put the cursor on the last line, far below an 8-row viewport.
        p.st.cursor = p.last.lines.last().map(|l| l.id);
        p.layout(area, &cx);
        let cur = p.at().and_then(|id| p.last.position(id)).unwrap();
        let body_h = (area.height as usize).saturating_sub(p.header(&p.last).len());
        assert!(cur >= p.top && cur < p.top + body_h.max(1), "cursor {cur} outside [{}, +{body_h})", p.top);
        assert!(draw_once(&p, &cx, 60, 8).contains("m30"), "the cursor's row is drawn");
    }

    #[test]
    fn zoom_cycles_through_three_and_returns() {
        let s = store_with(vec![json!({"id":1,"agent":"ai","content":"a"})]);
        let mut p = AiPanel::new("ai".into());
        let seen = std::collections::HashSet::new();
        let a: AgentId = "ai".into();
        let cx = Cx {
            agent: &a,
            store: &s,
            tasks: &[],
            seen: &seen,
            reflect: None,
        };
        let mut out = Vec::new();
        for want in [Zoom::Decisions, Zoom::Roots, Zoom::All] {
            p.on_key(key(KeyCode::Char('z')), &cx, &mut out);
            assert_eq!(p.st.zoom, want);
        }
    }

    #[test]
    fn stop_cancels_only_real_work_never_the_kernels_own_loops() {
        use crate::kern::model::Task;
        let s = store_with(vec![json!({"id":1,"agent":"ai","content":"a"})]);
        let tasks = vec![
            Task {
                id: 10,
                agent: "ai".into(),
                parent: None,
                verb: "send".into(),
                started_at: 0.0,
            },
            Task {
                id: 11,
                agent: "ai".into(),
                parent: None,
                verb: "ws_drain".into(),
                started_at: 0.0,
            },
            Task {
                id: 12,
                agent: "other".into(),
                parent: None,
                verb: "send".into(),
                started_at: 0.0,
            },
        ];
        let mut p = AiPanel::new("ai".into());
        let seen = std::collections::HashSet::new();
        let a: AgentId = "ai".into();
        let cx = Cx {
            agent: &a,
            store: &s,
            tasks: &tasks,
            seen: &seen,
            reflect: None,
        };
        let mut out = Vec::new();
        p.on_key(
            KeyEvent::new(KeyCode::Char('S'), KeyModifiers::NONE),
            &cx,
            &mut out,
        );
        let ids: Vec<_> = out
            .iter()
            .filter_map(|c| match c {
                Cmd::Cancel { task_id } => Some(*task_id),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec![10], "infra verbs and other agents are untouched");
    }

    #[test]
    fn a_status_frame_reports_queued_with_how_many_are_ahead() {
        let mut p = AiPanel::new("ai".into());
        let mut out = Vec::new();
        assert!(p.on_event(
            &json!({"type":"status","phase":"queued","detail":{"ahead":2}}),
            &mut out
        ));
        let s = Store::default();
        let seen = std::collections::HashSet::new();
        let a: AgentId = "ai".into();
        let cx = Cx {
            agent: &a,
            store: &s,
            tasks: &[],
            seen: &seen,
            reflect: None,
        };
        let line = p.status(&cx).unwrap();
        assert!(line.contains("queued · ahead 2"), "{line}");
    }

    #[test]
    fn a_turn_in_flight_flashes_thinking_and_never_shows_the_tokens() {
        let mut p = AiPanel::new("ai".into());
        let mut out = Vec::new();
        p.on_event(&json!({"type":"token","text":"<think>secret reasoning"}), &mut out);
        assert!(p.live && p.animating(), "a token starts the indicator");
        let s = Store::default();
        let seen = std::collections::HashSet::new();
        let a: AgentId = "ai".into();
        let cx = Cx { agent: &a, store: &s, tasks: &[], seen: &seen, reflect: None };
        p.layout(Rect::new(0, 0, 60, 12), &cx);
        let screen = draw_once(&p, &cx, 60, 12);
        assert!(screen.contains("thinking"), "{screen}");
        assert!(!screen.contains("secret"), "the stream is never drawn: {screen}");
        p.on_event(&json!({"type":"status","phase":"done"}), &mut out);
        assert!(!p.live && !p.animating(), "done stops it");
        p.layout(Rect::new(0, 0, 60, 12), &cx);
        assert!(!draw_once(&p, &cx, 60, 12).contains("thinking"));
    }

    #[test]
    fn a_frame_touches_only_what_is_on_screen_however_long_the_thread() {
        // THE SLOWDOWN, pinned by WORK not time: measured before the fix, a
        // 2000-message thread cost 174 ms a frame in a debug build (every
        // message wrapped twice, the outline re-projected, every frame).
        let text = "word ".repeat(150);
        let s = store_with(
            (1..=2000)
                .map(|i| json!({"id": i, "agent": "ai", "role": if i % 2 == 1 { "user" } else { "assistant" }, "content": text}))
                .collect(),
        );
        let mut p = AiPanel::new("ai".into());
        let seen = std::collections::HashSet::new();
        let a: AgentId = "ai".into();
        let cx = Cx { agent: &a, store: &s, tasks: &[], seen: &seen, reflect: None };
        let area = Rect::new(0, 0, 120, 40);
        p.layout(area, &cx);
        p.st.cursor = Some(2000);
        p.layout(area, &cx);
        draw_once(&p, &cx, 120, 40);
        assert!(p.wraps.borrow().len() <= 12, "only visible messages are wrapped: {}", p.wraps.borrow().len());
        // moving the cursor never re-projects the thread
        let key = p.proj_key;
        p.st.cursor = Some(1999);
        p.layout(area, &cx);
        assert_eq!(p.proj_key, key);
        // a resize empties the memo instead of keeping a copy per width
        p.layout(Rect::new(0, 0, 100, 40), &cx);
        draw_once(&p, &cx, 100, 40);
        assert!(p.wraps.borrow().len() <= 12);
    }

    #[test]
    fn a_line_wears_the_hue_of_what_it_is() {
        let s = store_with(vec![
            json!({"id":1,"agent":"ai","role":"user","content":"[from user]\nread it"}),
            json!({"id":2,"agent":"ai","role":"assistant","content":"on it\n<call name=\"send\"><arg name=\"target_id\">files</arg></call>"}),
            json!({"id":3,"agent":"ai","role":"user","content":"[reply to 9 · files read_stream]\nfn main() {}"}),
        ]);
        let mut p = AiPanel::new("ai".into());
        let seen = std::collections::HashSet::new();
        let a: AgentId = "ai".into();
        let cx = Cx { agent: &a, store: &s, tasks: &[], seen: &seen, reflect: None };
        p.layout(Rect::new(0, 0, 60, 20), &cx);
        let hue = |id: RowId, needle: &str| -> Option<ratatui::style::Color> {
            let l = p.last.lines.iter().find(|l| l.id == id)?;
            p.block_of(l, false, 60)
                .into_iter()
                .flat_map(|line| line.spans.into_iter())
                .find(|sp| sp.content.contains(needle))
                .and_then(|sp| sp.style.fg)
        };
        assert_eq!(hue(1, "user:"), Some(theme::SAY_HUMAN), "the human's label");
        assert_eq!(hue(2, "ai:"), Some(theme::SAY_SELF), "the agent's own label");
        assert_eq!(hue(2, "▸ send"), Some(theme::SAY_CALL), "a tool call");
        assert_eq!(hue(3, "reply"), Some(theme::SAY_REPLY), "a tool reply");
        assert_eq!(hue(2, "on it"), Some(theme::FG), "plain text");
    }

    #[test]
    fn a_long_message_wraps_whole_and_the_cursor_keeps_it_on_screen() {
        let long = "The entry point is src/main.rs. It parses the flags, seeds the directory on the first run, then boots the kernel in process and hands it to the shell.";
        let s = store_with(vec![
            json!({"id":1,"agent":"ai","role":"user","content":"where does it start?"}),
            json!({"id":2,"agent":"ai","role":"assistant","content":long}),
        ]);
        let mut p = AiPanel::new("ai".into());
        let seen = std::collections::HashSet::new();
        let a: AgentId = "ai".into();
        let cx = Cx { agent: &a, store: &s, tasks: &[], seen: &seen, reflect: None };
        let area = Rect::new(0, 0, 40, 16);
        p.layout(area, &cx);
        p.st.cursor = Some(2);
        p.layout(area, &cx);
        let screen = draw_once(&p, &cx, 40, 16);
        // every word of the answer is on screen: wrapped, not clipped
        for w in ["entry", "flags,", "directory", "kernel", "shell."] {
            assert!(screen.contains(w), "{w} missing:\n{screen}");
        }
        assert!(screen.lines().all(|l| l.chars().count() <= 40));
    }

    #[test]
    fn selecting_the_agent_reads_the_whole_conversation_and_opens_a_watch() {
        let mut p = AiPanel::new("ai".into());
        let s = Store::default();
        let seen = std::collections::HashSet::new();
        let a: AgentId = "ai".into();
        let cx = Cx {
            agent: &a,
            store: &s,
            tasks: &[],
            seen: &seen,
            reflect: None,
        };
        let mut out = Vec::new();
        p.on_select(&cx, &mut out);
        assert!(out
            .iter()
            .any(|c| matches!(c, Cmd::Rows { full: true, .. })));
        assert!(out.iter().any(|c| matches!(c, Cmd::Watch(_))));
    }

    #[test]
    fn the_cursor_moves_over_visible_lines_and_stops_at_the_ends() {
        let s = store_with(vec![
            json!({"id":1,"agent":"ai","role":"user","content":"a"}),
            json!({"id":2,"agent":"ai","role":"assistant","parent":1,"content":"b"}),
            json!({"id":3,"agent":"ai","role":"user","content":"c"}),
        ]);
        let mut p = AiPanel::new("ai".into());
        let seen = std::collections::HashSet::new();
        let a: AgentId = "ai".into();
        let cx = Cx {
            agent: &a,
            store: &s,
            tasks: &[],
            seen: &seen,
            reflect: None,
        };
        p.last = p.project(&cx);
        let n = p.last.lines.len();
        assert!(n >= 2);
        let mut out = Vec::new();
        p.on_key(key(KeyCode::Down), &cx, &mut out);
        assert_eq!(p.st.cursor, Some(p.last.lines[1].id));
        for _ in 0..50 {
            p.on_key(key(KeyCode::Down), &cx, &mut out);
        }
        assert_eq!(
            p.st.cursor,
            Some(p.last.lines[n - 1].id),
            "never past the end"
        );
        for _ in 0..50 {
            p.on_key(key(KeyCode::Up), &cx, &mut out);
        }
        assert_eq!(p.st.cursor, Some(p.last.lines[0].id));
    }
}
