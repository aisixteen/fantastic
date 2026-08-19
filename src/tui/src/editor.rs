//! The **in-frame code editor** — a native text editor (`tui_textarea::TextArea`)
//! for authoring a custom JS connector agent (design `28_add_agent_connectors.md`).
//! It frames the chat inset like the flow control / question card (NOT an
//! alt-screen takeover): a title bar `connector.js — <id>`, the editable body, and
//! a `^S save · esc close` footer.
//!
//! **Save is a `send`.** `^S` yields [`EditorAction::Save`] carrying the current
//! text; the TUI `send`s `{type:"start", code:<text>}` so the js_runtime persists it
//! onto the agent record — the connector is live the moment you save. `Esc` cancels
//! ([`EditorAction::Cancel`]) with nothing saved.
//!
//! Driven by EXPLICIT `TextArea` edit methods mapped from ratatui's re-exported
//! [`KeyCode`] — never a crossterm `Event` into `TextArea::input` (which would tie us
//! to tui-textarea's own crossterm and risk a version mismatch). Pure-testable:
//! [`CodeEditor::from_code`], [`CodeEditor::on_key`], and [`CodeEditor::text`] need no
//! live terminal; [`CodeEditor::render`] lives here so `lib.rs` stays a thin shell.

use ratatui::crossterm::event::{KeyCode, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders};
use tui_textarea::{CursorMove, TextArea};

/// The outcome of feeding a key to the editor.
pub enum EditorAction {
    /// The key edited the buffer (or was ignored); the editor stays open.
    Pending,
    /// `^S` — carry the current buffer text to persist (`send {type:"start", code}`).
    Save(String),
    /// `Esc` — close without saving.
    Cancel,
}

/// A live in-frame editor over one agent's `code`. Built with [`CodeEditor::from_code`];
/// keys drive it via [`CodeEditor::on_key`]; the text is [`CodeEditor::text`].
pub struct CodeEditor {
    textarea: TextArea<'static>,
    /// The agent whose `code` this edits (the `send` target on save).
    pub agent_id: String,
    /// The editor title (e.g. `connector.js`).
    pub title: String,
}

impl CodeEditor {
    /// Seed the editor with an agent's `code` (one buffer line per `\n`).
    pub fn from_code(agent_id: impl Into<String>, title: impl Into<String>, code: &str) -> Self {
        let lines: Vec<String> = code.split('\n').map(str::to_string).collect();
        CodeEditor {
            textarea: TextArea::new(lines),
            agent_id: agent_id.into(),
            title: title.into(),
        }
    }

    /// The current buffer as one `\n`-joined string (what a save persists).
    pub fn text(&self) -> String {
        self.textarea.lines().join("\n")
    }

    /// Drive the editor from a key. `^S` → [`EditorAction::Save`] (current text);
    /// `Esc` → [`EditorAction::Cancel`]; everything else edits the buffer and stays
    /// [`EditorAction::Pending`]. Keys are mapped to EXPLICIT `TextArea` methods.
    pub fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) -> EditorAction {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        match code {
            KeyCode::Char('s') if ctrl => return EditorAction::Save(self.text()),
            KeyCode::Esc => return EditorAction::Cancel,
            // Other Ctrl combos are ignored (they don't edit) so the modal stays put.
            KeyCode::Char(_) if ctrl => {}
            KeyCode::Char(c) => self.textarea.insert_char(c),
            KeyCode::Enter => self.textarea.insert_newline(),
            KeyCode::Tab => {
                self.textarea.insert_tab();
            }
            KeyCode::Backspace => {
                self.textarea.delete_char();
            }
            KeyCode::Delete => {
                self.textarea.delete_next_char();
            }
            KeyCode::Left => self.textarea.move_cursor(CursorMove::Back),
            KeyCode::Right => self.textarea.move_cursor(CursorMove::Forward),
            KeyCode::Up => self.textarea.move_cursor(CursorMove::Up),
            KeyCode::Down => self.textarea.move_cursor(CursorMove::Down),
            KeyCode::Home => self.textarea.move_cursor(CursorMove::Head),
            KeyCode::End => self.textarea.move_cursor(CursorMove::End),
            _ => {}
        }
        EditorAction::Pending
    }

    /// Render the editor framing the chat inset `area`: an opaque black panel with a
    /// magenta border, `connector.js — <id>` on the top rail, the textarea inside,
    /// and `^S save · esc close` on the bottom rail. Mirrors the question-card /
    /// help overlay style (in-frame, not fullscreen).
    pub fn render(&self, f: &mut Frame, area: Rect) {
        let mag = Style::default()
            .fg(Color::LightMagenta)
            .add_modifier(Modifier::BOLD);
        let dim = Style::default().fg(Color::DarkGray);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(mag)
            .title(Span::styled(
                format!(" {} — {} ", self.title, self.agent_id),
                mag,
            ))
            .title_bottom(Line::from(Span::styled(" ^S save · esc close ", dim)).right_aligned());
        let inner = block.inner(area);
        // Opaque: black the panel out first so the transcript/stars don't bleed.
        crate::fill_black(f.buffer_mut(), area);
        f.render_widget(block, area);
        f.render_widget(&self.textarea, inner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none() -> KeyModifiers {
        KeyModifiers::NONE
    }

    #[test]
    fn from_code_seeds_the_buffer_verbatim() {
        let ed = CodeEditor::from_code("conn_ab12", "connector.js", "line one\nline two");
        assert_eq!(ed.text(), "line one\nline two");
        assert_eq!(ed.agent_id, "conn_ab12");
        assert_eq!(ed.title, "connector.js");
    }

    #[test]
    fn on_key_types_and_newlines_edit_the_buffer() {
        let mut ed = CodeEditor::from_code("c", "t", "");
        assert!(matches!(
            ed.on_key(KeyCode::Char('a'), none()),
            EditorAction::Pending
        ));
        ed.on_key(KeyCode::Enter, none());
        ed.on_key(KeyCode::Char('b'), none());
        assert_eq!(ed.text(), "a\nb");
    }

    #[test]
    fn backspace_deletes_before_the_caret() {
        let mut ed = CodeEditor::from_code("c", "t", "");
        for ch in "abc".chars() {
            ed.on_key(KeyCode::Char(ch), none());
        }
        ed.on_key(KeyCode::Backspace, none());
        assert_eq!(ed.text(), "ab");
    }

    #[test]
    fn ctrl_s_saves_the_current_text() {
        let mut ed = CodeEditor::from_code("c", "t", "hello");
        ed.on_key(KeyCode::Char('!'), none()); // caret at head → "!hello"
        match ed.on_key(KeyCode::Char('s'), KeyModifiers::CONTROL) {
            EditorAction::Save(s) => assert_eq!(s, "!hello"),
            _ => panic!("Ctrl+S must Save the current text"),
        }
    }

    #[test]
    fn esc_cancels_without_saving() {
        let mut ed = CodeEditor::from_code("c", "t", "hi");
        assert!(matches!(
            ed.on_key(KeyCode::Esc, none()),
            EditorAction::Cancel
        ));
    }

    #[test]
    fn other_ctrl_combos_are_inert() {
        let mut ed = CodeEditor::from_code("c", "t", "hi");
        // Ctrl+F etc. must not type an 'f' into the buffer nor close the editor.
        assert!(matches!(
            ed.on_key(KeyCode::Char('f'), KeyModifiers::CONTROL),
            EditorAction::Pending
        ));
        assert_eq!(ed.text(), "hi");
    }
}
