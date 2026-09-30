//! A LIVE PTY, drawn by a real vt100 emulator.
//!
//! The pane draws ONE exec_bridge PTY job (`kern/terminal` starts or finds it and
//! hands over its `job_id`); every call it makes names that job, and a frame of
//! any other job is not its to draw. On attach the job's `output` ring is fed
//! first, so a re-attached shell shows what it printed before.
//!
//! Output arrives as watch frames (`{type:"output", job_id, data}`), not as rows: a
//! terminal's bytes are a stream, and storing them in the log would make every
//! keystroke a durable record. The frames feed a `vt100::Parser`, so a
//! full-screen CLI (an AI coder, an editor) draws exactly as it would in a
//! terminal of the pane's size, and the pane tells the PTY that size (`resize`)
//! whenever it changes.
//!
//! FLOW CONTROL: the bridge pauses its reader once too many emitted chars are
//! unacknowledged. Every frame fed is acked, as operator housekeeping
//! (`Cmd::Stream`), so a busy screen never stalls.
//!
//! While the pane has the keyboard every key is encoded and sent as `write`
//! with a STRING payload — Ctrl-C included, since that belongs to the program
//! running in it. `Esc` hands the keyboard back; `Shift-PgUp/PgDn` scroll back.
//! When the shell exits (`job_done`), `Enter` starts a new one.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use serde_json::{json, Value};

use crate::ev::Cmd;
use crate::kern::model::AgentId;
use crate::panel::{Cx, Handled, Needs, Panel, PanelKind};
use crate::theme;

/// Rows of scrollback the emulator keeps. A stream is not a log; this bound is
/// the honest one.
const SCROLLBACK: usize = 2_000;

pub struct TerminalPanel {
    agent: AgentId,
    /// The PTY job drawn here. `None` until `kern/terminal` attaches one.
    job: Option<i64>,
    screen: vt100::Parser,
    /// The size the PTY was last told, so a resize is sent once per change.
    told: Option<(u16, u16)>,
    /// Commands a `layout` step produced (it has no out-queue of its own).
    pending: Vec<Cmd>,
    /// Rows scrolled back from the live screen. Zero means following.
    pub scrollback: usize,
    /// Whether the pane has the keyboard.
    pub takeover: bool,
    /// Bytes fed so far, for the statusline and the tests.
    fed: usize,
    closed: bool,
}

impl TerminalPanel {
    pub fn new(agent: AgentId) -> Self {
        Self {
            agent,
            job: None,
            screen: vt100::Parser::new(24, 80, SCROLLBACK),
            told: None,
            pending: Vec::new(),
            scrollback: 0,
            takeover: false,
            fed: 0,
            closed: false,
        }
    }

    /// Draw `job_id` from now on, starting from a clean screen fed with what the
    /// job printed so far. A size told before the job existed is told again.
    pub fn attach(&mut self, job_id: i64, replay: &str) {
        let (rows, cols) = self.screen.screen().size();
        self.screen = vt100::Parser::new(rows, cols, SCROLLBACK);
        self.scrollback = 0;
        self.fed = 0;
        self.closed = false;
        self.job = Some(job_id);
        self.told = None;
        if !replay.is_empty() {
            self.feed(replay.as_bytes());
        }
    }

    /// Absorb a chunk.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.screen.process(bytes);
        self.fed += bytes.len();
    }

    /// The emulator's screen as plain text, row per line.
    pub fn text(&self) -> String {
        self.screen.screen().contents()
    }

    /// The key encoding sent to the PTY. Every byte emitted is ASCII, so a
    /// `write` carrying a string is exact.
    pub fn encode_key(k: crossterm::event::KeyEvent) -> Option<String> {
        use crossterm::event::{KeyCode as K, KeyModifiers as M};
        let ctrl = k.modifiers.contains(M::CONTROL);
        Some(match k.code {
            K::Char(c) if ctrl => {
                // Ctrl-A..Ctrl-Z → 0x01..0x1A, the control codes a shell expects.
                let u = c.to_ascii_uppercase();
                if !u.is_ascii_uppercase() {
                    return None;
                }
                ((u as u8 - b'A' + 1) as char).to_string()
            }
            K::Char(c) if c.is_ascii() => c.to_string(),
            K::Enter => "\r".to_string(),
            K::Tab => "\t".to_string(),
            K::BackTab => "\x1b[Z".to_string(),
            K::Backspace => "\x7f".to_string(),
            K::Esc => "\x1b".to_string(),
            K::Up => "\x1b[A".to_string(),
            K::Down => "\x1b[B".to_string(),
            K::Right => "\x1b[C".to_string(),
            K::Left => "\x1b[D".to_string(),
            K::Home => "\x1b[H".to_string(),
            K::End => "\x1b[F".to_string(),
            K::Delete => "\x1b[3~".to_string(),
            K::PageUp => "\x1b[5~".to_string(),
            K::PageDown => "\x1b[6~".to_string(),
            _ => return None,
        })
    }

    /// A key's bytes: the ASCII encoding, or the character itself as UTF-8 (a
    /// non-ASCII letter is still a string `write`).
    fn bytes_of(k: crossterm::event::KeyEvent) -> Option<String> {
        use crossterm::event::{KeyCode as K, KeyModifiers as M};
        match k.code {
            K::Char(c) if !c.is_ascii() && !k.modifiers.contains(M::CONTROL) => Some(c.to_string()),
            _ => Self::encode_key(k),
        }
    }

    fn write(&self, data: String, out: &mut Vec<Cmd>) {
        // Keys before a job exists, or after it exited, have nowhere to go.
        let Some(job) = self.job.as_ref().filter(|_| !self.closed) else { return };
        out.push(Cmd::Send {
            target: self.agent.clone(),
            payload: json!({"type":"write","job_id":job,"data":data}),
        });
    }

    /// Is `payload` a frame of the job drawn here?
    fn mine(&self, payload: &Value) -> bool {
        self.job.is_some() && payload.get("job_id").and_then(Value::as_i64) == self.job
    }
}

fn colour(c: vt100::Color, default: Color) -> Color {
    match c {
        vt100::Color::Default => default,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

impl Panel for TerminalPanel {
    fn kind(&self) -> PanelKind {
        PanelKind::Terminal
    }

    fn needs(&self) -> Needs {
        // The stream IS the content, so the watch matters and rows do not.
        Needs { rows: false, reflect: true, watch: true }
    }

    fn on_key(&mut self, k: crossterm::event::KeyEvent, _cx: &Cx<'_>, out: &mut Vec<Cmd>) -> Handled {
        use crossterm::event::{KeyCode as K, KeyModifiers as M};
        if k.modifiers.contains(M::SHIFT) && matches!(k.code, K::PageUp | K::PageDown) {
            let (rows, _) = self.screen.screen().size();
            let step = (rows as usize / 2).max(1);
            self.scrollback = if k.code == K::PageUp {
                self.scrollback + step
            } else {
                self.scrollback.saturating_sub(step)
            };
            self.screen.set_scrollback(self.scrollback);
            // What the emulator actually keeps is the bound.
            self.scrollback = self.screen.screen().scrollback();
            return Handled::Yes;
        }
        if self.takeover {
            if k.code == K::Esc {
                self.takeover = false;
                return Handled::Yes;
            }
            if let Some(data) = Self::bytes_of(k) {
                if self.scrollback > 0 {
                    self.scrollback = 0;
                    self.screen.set_scrollback(0);
                }
                self.write(data, out);
            }
            return Handled::Yes;
        }
        match k.code {
            K::Enter if self.closed => {
                // A new shell, at the size the pane was drawn.
                let (rows, cols) = self.screen.screen().size();
                out.push(Cmd::OpenTerminal { cols, rows });
                self.takeover = true;
                Handled::Yes
            }
            K::Enter => {
                self.takeover = true;
                Handled::Yes
            }
            _ => Handled::No,
        }
    }

    fn on_event(&mut self, payload: &Value, out: &mut Vec<Cmd>) -> bool {
        match payload.get("type").and_then(Value::as_str) {
            Some("output") if self.mine(payload) => {}
            Some("job_done") if self.mine(payload) => {
                self.closed = true;
                self.takeover = false;
                return true;
            }
            _ => return false,
        }
        let (bytes, chars) = match payload.get("data") {
            Some(Value::String(s)) => (s.as_bytes().to_vec(), s.chars().count()),
            // A bytes-valued frame arrives decoded as an array of bytes.
            Some(Value::Array(a)) => {
                let b: Vec<u8> = a.iter().filter_map(Value::as_u64).map(|n| n as u8).collect();
                let n = String::from_utf8_lossy(&b).chars().count();
                (b, n)
            }
            _ => return false,
        };
        self.feed(&bytes);
        out.push(Cmd::Stream {
            agent: self.agent.clone(),
            payload: json!({"type":"ack","job_id":self.job,"chars":chars}),
        });
        true
    }

    /// A keystroke's `{written:n}` is this pane's own echo; it must not land
    /// on whatever page is above as that page's "last call".
    fn on_reply(&mut self, reply: &Value, _out: &mut Vec<Cmd>) -> bool {
        reply.get("written").is_some()
    }

    fn on_select(&mut self, _cx: &Cx<'_>, out: &mut Vec<Cmd>) {
        out.push(Cmd::Watch(self.agent.clone()));
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn layout(&mut self, area: Rect, _cx: &Cx<'_>) {
        let want = (area.height.max(1), area.width.max(1));
        if self.screen.screen().size() != want {
            self.screen.set_size(want.0, want.1);
        }
        // Told once per change, and only to a live job: the size a job starts
        // at comes with its `start`.
        let Some(job) = self.job.as_ref().filter(|_| !self.closed) else { return };
        if self.told != Some(want) {
            self.told = Some(want);
            self.pending.push(Cmd::Stream {
                agent: self.agent.clone(),
                payload: json!({"type":"resize","job_id":job,"rows":want.0,"cols":want.1}),
            });
        }
    }

    fn take_cmds(&mut self) -> Vec<Cmd> {
        std::mem::take(&mut self.pending)
    }

    fn render(&self, f: &mut Frame<'_>, area: Rect, _cx: &Cx<'_>) {
        if self.fed == 0 {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("  starting {} …", self.agent),
                    Style::default().fg(theme::DIM),
                ))),
                area,
            );
            return;
        }
        draw_screen(self.screen.screen(), f.buffer_mut(), area, self.takeover && self.scrollback == 0);
    }

    fn status(&self, _cx: &Cx<'_>) -> Option<String> {
        Some(if self.closed {
            format!("{} · the shell exited · Enter starts a new one", self.agent)
        } else if self.takeover {
            format!("{} · typing into the terminal · Esc hands the keyboard back", self.agent)
        } else {
            format!("{} · Enter to type into it", self.agent)
        })
    }
}

/// Paint the emulator's cells into the frame buffer, cell for cell.
fn draw_screen(s: &vt100::Screen, buf: &mut Buffer, area: Rect, cursor: bool) {
    let (rows, cols) = s.size();
    for r in 0..rows.min(area.height) {
        for c in 0..cols.min(area.width) {
            let Some(cell) = s.cell(r, c) else { continue };
            if cell.is_wide_continuation() {
                continue;
            }
            let mut st = Style::default()
                .fg(colour(cell.fgcolor(), theme::FG))
                .bg(colour(cell.bgcolor(), theme::BG));
            if cell.bold() {
                st = st.add_modifier(Modifier::BOLD);
            }
            if cell.italic() {
                st = st.add_modifier(Modifier::ITALIC);
            }
            if cell.underline() {
                st = st.add_modifier(Modifier::UNDERLINED);
            }
            if cell.inverse() {
                st = st.add_modifier(Modifier::REVERSED);
            }
            let text = cell.contents();
            if let Some(out) = buf.cell_mut((area.x + c, area.y + r)) {
                out.set_symbol(if text.is_empty() { " " } else { &text });
                out.set_style(st);
            }
        }
    }
    if cursor && !s.hide_cursor() {
        let (r, c) = s.cursor_position();
        if r < area.height && c < area.width {
            if let Some(out) = buf.cell_mut((area.x + c, area.y + r)) {
                out.set_style(Style::default().add_modifier(Modifier::REVERSED));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn panel() -> TerminalPanel {
        let mut p = TerminalPanel::new("term".into());
        p.attach(11, "");
        p
    }

    fn cx_parts() -> (crate::store::Store, std::collections::HashSet<crate::kern::model::RowId>, AgentId) {
        (crate::store::Store::default(), std::collections::HashSet::new(), "term".into())
    }

    #[test]
    fn output_draws_through_the_emulator_and_every_frame_is_acked() {
        let mut p = panel();
        let mut out = Vec::new();
        // A cursor move and an overwrite: a line log would show both, a terminal
        // shows the result.
        assert!(p.on_event(&json!({"type":"output","job_id":11,"data":"hello\r\x1b[2Kbye"}), &mut out));
        assert!(p.text().starts_with("bye"), "{:?}", p.text());
        assert!(!p.text().contains("hello"));
        match &out[0] {
            Cmd::Stream { payload, .. } => {
                assert_eq!(payload["type"], "ack");
                assert_eq!(payload["job_id"], 11);
                assert_eq!(payload["chars"], 13);
            }
            other => panic!("{other:?}"),
        }
        assert!(!p.on_event(&json!({"type":"token","text":"a"}), &mut out));
        // Another job's frames are not this pane's to draw.
        assert!(!p.on_event(&json!({"type":"output","job_id":10,"data":"x"}), &mut out));
        assert!(!p.on_event(&json!({"type":"job_done","job_id":10,"exit_code":0}), &mut out));
        assert!(!p.closed);
        assert!(p.on_event(&json!({"type":"job_done","job_id":11,"exit_code":0,"ok":true}), &mut out));
        assert!(p.closed);
    }

    #[test]
    fn a_new_size_is_told_to_the_pty_once() {
        let mut p = panel();
        let (s, seen, a) = cx_parts();
        let cx = Cx { agent: &a, store: &s, tasks: &[], seen: &seen, reflect: None };
        p.layout(Rect::new(0, 0, 70, 12), &cx);
        p.layout(Rect::new(0, 0, 70, 12), &cx);
        let cmds = p.take_cmds();
        assert_eq!(cmds.len(), 1, "one resize per change");
        match &cmds[0] {
            Cmd::Stream { payload, .. } => {
                assert_eq!(payload["type"], "resize");
                assert_eq!(payload["job_id"], 11);
                assert_eq!((payload["rows"].as_u64(), payload["cols"].as_u64()), (Some(12), Some(70)));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(p.screen.screen().size(), (12, 70));
    }

    #[test]
    fn control_keys_encode_to_their_control_codes() {
        let ctrl = |c: char| TerminalPanel::encode_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
        assert_eq!(ctrl('c').unwrap(), "\u{3}");
        assert_eq!(ctrl('d').unwrap(), "\u{4}");
        assert_eq!(TerminalPanel::encode_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)).unwrap(), "\r");
        assert_eq!(TerminalPanel::encode_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)).unwrap(), "\x1b[A");
    }

    #[test]
    fn every_encoded_key_is_ascii_so_a_string_write_is_exact() {
        for code in [
            KeyCode::Enter, KeyCode::Tab, KeyCode::Backspace, KeyCode::Esc, KeyCode::Up, KeyCode::Down,
            KeyCode::Left, KeyCode::Right, KeyCode::Home, KeyCode::End, KeyCode::Delete, KeyCode::PageUp,
            KeyCode::PageDown, KeyCode::Char('x'),
        ] {
            let s = TerminalPanel::encode_key(KeyEvent::new(code, KeyModifiers::NONE)).unwrap();
            assert!(s.is_ascii(), "{code:?} encoded to non-ascii {s:?}");
        }
    }

    #[test]
    fn takeover_sends_a_string_and_esc_hands_the_keyboard_back() {
        let mut p = panel();
        p.takeover = true;
        let (s, seen, a) = cx_parts();
        let cx = Cx { agent: &a, store: &s, tasks: &[], seen: &seen, reflect: None };
        let mut out = Vec::new();
        p.on_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &cx, &mut out);
        p.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL), &cx, &mut out);
        let data: Vec<&str> = out
            .iter()
            .map(|c| match c {
                Cmd::Send { payload, .. } => payload["data"].as_str().unwrap(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(data, vec!["l", "\u{3}"], "Ctrl-C belongs to the program in the terminal");
        p.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &cx, &mut out);
        assert!(!p.takeover);
    }

    #[test]
    fn a_reattach_replays_the_ring_and_a_finished_shell_restarts_on_enter() {
        let mut p = TerminalPanel::new("term".into());
        let (s, seen, a) = cx_parts();
        let cx = Cx { agent: &a, store: &s, tasks: &[], seen: &seen, reflect: None };
        let mut out = Vec::new();
        // No job yet: keys and sizes go nowhere.
        p.takeover = true;
        p.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), &cx, &mut out);
        p.layout(Rect::new(0, 0, 40, 10), &cx);
        assert!(out.is_empty() && p.take_cmds().is_empty());
        p.attach(12, "$ ls\r\nREADME\r\n$ ");
        assert!(p.text().contains("README"), "{:?}", p.text());
        // The size is told to the job once it exists.
        p.layout(Rect::new(0, 0, 40, 10), &cx);
        assert_eq!(p.take_cmds().len(), 1);
        p.on_event(&json!({"type":"job_done","job_id":12,"exit_code":0,"ok":true}), &mut out);
        p.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &cx, &mut out);
        assert!(matches!(out.last(), Some(Cmd::OpenTerminal { cols: 40, rows: 10 })), "{out:?}");
    }
}
