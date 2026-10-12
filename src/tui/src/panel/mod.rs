//! WHICH UI AN AGENT GETS, decided by what the agent IS.
//!
//! Selecting an agent in the tree changes the right pane to THAT agent's own
//! interface. The key is `handler_module`, taken from the tree reflect — the
//! distilled roster does not carry it, which is why the tree is read with
//! `detail:"full"`.
//!
//! An unrecognised module is NOT an error and NOT a blank pane: it gets the
//! generic panel, which shows the agent's reflect and lets the human call any
//! verb it declares. That is what makes a kernel the shell has never seen usable
//! the day it is written, and it is the whole reason panels key off a module name
//! rather than a hard-coded list of blessed agents.

// `PanelKind::label`, `Panel::kind`, `as_any_mut` and `on_event` complete the
// panel contract. `on_event` is the watch leg's consumer and is unreachable until
// a streaming agent is in the roster (see `ev.rs`); the other three are how a pane
// is identified and how a reply reaches a concrete type. Declared whole so a new
// panel has one contract to read.
#![allow(dead_code)]

pub mod activity;
pub mod ai;
pub mod files;
pub mod generic;
pub mod setup;
pub mod wizard;
pub mod terminal;

use serde_json::Value;

use crate::ev::Cmd;
use crate::kern::model::AgentId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PanelKind {
    /// A model: conversation, gutter, folds, the waiting query.
    Ai,
    /// A live byte stream with a takeover mode.
    Terminal,
    /// A clamped disk surface, browsed.
    Files,
    /// Anything else: reflect plus verb entry.
    Generic,
    /// Not an agent: every open question to the human, across all of them.
    Activity,
    /// Not an agent: the page that writes a new ai agent's record.
    Setup,
}

impl PanelKind {
    /// The classification. Suffix matching on the module is deliberate: a kernel
    /// may ship `vllm_backend.tools` or `responses_backend.tools` and both are
    /// the same UI, so the rule is "ends with `_backend.tools`", not a list of
    /// provider names this shell would have to be taught.
    ///
    /// The one exception is by ID: the shell's own terminal `term` is an
    /// `exec_bridge` like any command runner, and only this shell knows it is
    /// the PTY it drew. Every other exec agent is a generic one.
    pub fn of(id: &str, handler_module: &str) -> Self {
        let m = handler_module;
        if id == crate::kern::terminal::TERM_ID && m == fantastic_exec_bridge::HANDLER_MODULE {
            return PanelKind::Terminal;
        }
        if m.ends_with("_backend.tools") || m == "ai_core.tools" {
            return PanelKind::Ai;
        }
        match m {
            "file_bridge.tools" => PanelKind::Files,
            _ => PanelKind::Generic,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            PanelKind::Ai => "ai",
            PanelKind::Terminal => "terminal",
            PanelKind::Files => "files",
            PanelKind::Generic => "agent",
            PanelKind::Activity => "activity",
            PanelKind::Setup => "setup",
        }
    }
}

/// What a panel needs from the shell each frame. Read-only: a panel mutates
/// itself and pushes commands, and it never reaches the wire.
pub struct Cx<'a> {
    pub agent: &'a AgentId,
    pub store: &'a crate::store::Store,
    pub tasks: &'a [crate::kern::model::Task],
    pub seen: &'a std::collections::HashSet<crate::kern::model::RowId>,
    pub reflect: Option<&'a Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handled {
    Yes,
    No,
}

/// What a panel wants polled while it is on screen, so the cadence follows the
/// human's attention rather than a fixed list.
#[derive(Debug, Clone, Default)]
pub struct Needs {
    pub rows: bool,
    pub reflect: bool,
    pub watch: bool,
}

pub trait Panel {
    fn kind(&self) -> PanelKind;
    /// What this panel wants kept fresh.
    fn needs(&self) -> Needs;
    /// A key, already filtered by focus. Returns whether it was consumed, so an
    /// unhandled key can fall through to the global map.
    fn on_key(&mut self, k: crossterm::event::KeyEvent, cx: &Cx<'_>, out: &mut Vec<Cmd>)
        -> Handled;
    /// A watch frame for this agent. Returns whether the pane must redraw.
    fn on_event(&mut self, payload: &Value, out: &mut Vec<Cmd>) -> bool;
    /// The agent just became the selection.
    fn on_select(&mut self, cx: &Cx<'_>, out: &mut Vec<Cmd>);
    /// A reply to something this panel asked for. Returns whether it took it.
    ///
    /// This is how a pane shows what a call answered. Sending a verb and seeing
    /// nothing is the worst outcome available: the human cannot tell a refusal
    /// from a success from a dropped keystroke.
    fn on_reply(&mut self, _reply: &Value, _out: &mut Vec<Cmd>) -> bool {
        false
    }
    /// Rows the human has now LOOKED AT. Per-client state, which is why it is the
    /// only thing this shell stores for itself: two people watching one kernel
    /// disagree about what is new, correctly.
    fn seen_now(&self) -> Vec<crate::kern::model::RowId> {
        Vec::new()
    }
    /// Downcast access, for anything the trait cannot express.
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
    /// A TEXT FIELD of this panel is open (being typed into): every key but
    /// Ctrl-C goes to it — no global shortcut (`R`, `?`, Tab, holding `q`)
    /// may swallow a character of what is typed (a key, a URL, a name).
    fn text_input(&self) -> bool {
        false
    }
    /// THE VIEWMODEL STEP for this frame: bring every DERIVED field up to date —
    /// a projected outline, a scroll position clamped to the viewport. The
    /// runtime calls it before every draw, and it is the ONLY place derived state
    /// changes, so what a key acts on is what this step computed, never whatever
    /// a draw happened to leave behind.
    fn layout(&mut self, _area: ratatui::layout::Rect, _cx: &Cx<'_>) {}
    /// THIS VIEW's derived state (a scroll window, a wrap memo): every View —
    /// a terminal, a thin client, anything that shows the shell — keeps its
    /// own, because each has its own size. The ViewModel TAKES it after a
    /// View's pass and PUTS it back before that View's next one (`App::enter`
    /// / `App::leave`); between passes the panel holds its defaults, so a View
    /// it has never shown starts fresh. Shared state (the cursor, the fold,
    /// the rows) never moves. Default: nothing per View.
    fn take_view(&mut self) -> Option<Box<dyn std::any::Any>> {
        None
    }
    fn put_view(&mut self, _view: Box<dyn std::any::Any>) {}
    /// Whether this panel's state HAS a size of its own (a terminal: its PTY and
    /// its screen are one size). Such a panel is laid out only for the
    /// AUTHORITATIVE View (the latest, `vm::effective`); every other View draws
    /// it as it is. Default: no — the panel reflows to any size.
    fn sized(&self) -> bool {
        false
    }
    /// Commands the `layout` step produced (a terminal telling its PTY a new
    /// size). The runtime drains them right after `layout`.
    fn take_cmds(&mut self) -> Vec<Cmd> {
        Vec::new()
    }
    /// THE VIEW. Read-only: it draws the state `layout` left, and changes nothing.
    fn render(&self, f: &mut ratatui::Frame<'_>, area: ratatui::layout::Rect, cx: &Cx<'_>);
    /// One line for the statusline, or nothing.
    /// Whether this panel is animating right now, so the frame loop keeps
    /// drawing it (a `thinking` indicator, a spinner). Default: still.
    fn animating(&self) -> bool {
        false
    }

    fn status(&self, _cx: &Cx<'_>) -> Option<String> {
        None
    }
}

/// Build the panel an agent's module calls for.
pub fn make(kind: PanelKind, agent: &AgentId) -> Box<dyn Panel> {
    match kind {
        PanelKind::Ai => Box::new(ai::AiPanel::new(agent.clone())),
        PanelKind::Terminal => Box::new(terminal::TerminalPanel::new(agent.clone())),
        PanelKind::Files => Box::new(files::FilesPanel::new(agent.clone())),
        PanelKind::Generic => Box::new(generic::GenericPanel::new(agent.clone())),
        PanelKind::Activity => Box::new(activity::ActivityPanel::new()),
        PanelKind::Setup => Box::new(setup::SetupPanel::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_backend_is_an_ai_panel_without_being_named() {
        for m in [
            "vllm_backend.tools",
            "responses_backend.tools",
            "ollama_backend.tools",
            "foundation_models_backend.tools",
            // A provider this shell has never heard of still gets the AI panel.
            "some_future_backend.tools",
            "ai_core.tools",
        ] {
            assert_eq!(PanelKind::of("x", m), PanelKind::Ai, "{m}");
        }
    }

    #[test]
    fn the_shells_terminal_is_known_by_its_id_not_its_module() {
        assert_eq!(PanelKind::of("term", "exec_bridge.tools"), PanelKind::Terminal);
        // Any other command runner is not the shell's terminal.
        assert_eq!(PanelKind::of("exec", "exec_bridge.tools"), PanelKind::Generic);
    }

    #[test]
    fn the_known_surfaces_map_to_their_own_panels() {
        assert_eq!(PanelKind::of("x", "file_bridge.tools"), PanelKind::Files);
    }

    #[test]
    fn an_unknown_module_gets_the_generic_panel_not_a_blank_one() {
        for m in [
            "kernel_state.tools",
            "web_rest.tools",
            "apple_kvs.tools",
            "",
        ] {
            assert_eq!(PanelKind::of("x", m), PanelKind::Generic, "{m}");
        }
    }
}
