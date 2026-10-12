//! ALL THE STATE, IN ONE PLACE, ON ONE STACK.
//!
//! No `Arc`, no `Mutex`, no interior mutability. One task owns this struct and
//! every other task talks to it by sending an `Ev`. That is what makes the render
//! path unable to block: there is nothing here to await on.
//!
//! The shape of the shell: the left pane is the WHOLE kernel tree, and selecting
//! an agent changes the right pane to that agent's own UI. A panel is kept per
//! agent for the life of the session, so scroll, pins and zoom survive switching
//! away and back.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Instant;

use serde_json::Value;

use crate::env::{ClientCfg, Paths};
use crate::ev::{Cmd, Ev, Got, LinkEv, What};
use crate::kern::model::{AgentId, KernelErr, Record, RowId, Task, TreeNode};
use crate::panel::activity::{ActivityPanel, ACTIVITY_ID};
use crate::panel::setup::{SetupPanel, SETUP_ID};
use crate::panel::{self, Cx, Panel, PanelKind};
use crate::presence::Presence;
use crate::kern::terminal::TERM_ID;
use crate::ui::layout::{COMPOSER_H, STATUS_H, TITLE_H, TREE_W};
use crate::seen::Seen;
use crate::store::Store;
use crate::thread;
use crate::ui::composer::{Composer, Mode};
use crate::ui::tree::{self, TreeRow};

/// Where keys go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Tree,
    Panel,
    Composer,
    /// The in-app terminal below the page has the keyboard.
    Term,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Link {
    Down { why: String },
    Up,
}

/// The verbs THIS shell's own reads send (as operator): their traffic never
/// re-arms a read — a read publishes, and re-reading on it would loop.
const OWN_READS: [&str; 3] = ["read", "rows", "reflect"];

pub struct App {
    pub cfg: ClientCfg,
    pub link: Link,

    // ── what the kernel is ───────────────────────────────────────────────
    pub root_id: AgentId,
    pub runtime: String,
    pub guid: Option<String>,
    pub tree: Option<TreeNode>,
    pub records: BTreeMap<AgentId, Record>,
    pub reflects: HashMap<AgentId, Value>,
    /// The whole screen as the last frame laid it out.
    pub screen: ratatui::layout::Rect,
    /// Which of the sidebar's three views is on (`ui/side.rs`).
    pub side: crate::ui::side::SideView,
    /// The view the rail collapsed from, for `>`.
    pub side_before_rail: crate::ui::side::SideView,
    /// FLAT: how many system agents fold into `+ N system`.
    pub system_folded: usize,
    /// The persistence provider's id, from the root reflect: plumbing, so FLAT
    /// folds it.
    pub provider: Option<AgentId>,
    /// The recipe the screen opens on (the last one named), taken on the
    /// first tree.
    pub want: Option<crate::recipe::Recipe>,
    /// `t` was pressed: hand the terminal the keyboard once it exists.
    pub term_opening: bool,

    // ── the tree pane ────────────────────────────────────────────────────
    pub collapsed: HashSet<AgentId>,
    pub trows: Vec<TreeRow>,
    pub sel: usize,
    pub filter: Option<String>,
    pub filtering: bool,

    // ── the live picture ─────────────────────────────────────────────────
    pub store: Store,
    pub tasks: Vec<Task>,
    /// Agents whose work finished since the last look, so the tree can show a
    /// settled agent differently from an idle one.
    pub done: HashSet<AgentId>,
    /// Per agent, how many messages are waiting on the human. DERIVED from rows,
    /// which is the whole of "waiting" in an async system.
    pub waiting: HashMap<AgentId, usize>,
    pub seen: Seen,

    // ── the right pane ───────────────────────────────────────────────────
    pub panels: HashMap<AgentId, Box<dyn Panel>>,
    pub focus: Focus,
    pub composer: Composer,

    // ── chrome ───────────────────────────────────────────────────────────
    /// Loud failures, in the order they happened. Never a modal, never dropped.
    pub notes: Vec<String>,
    pub help: bool,
    pub dirty: bool,
    /// Set by `R` (the human asks): the next read of every hot agent is a full one.
    pub stale: bool,
    pub quit: bool,
    /// The event being applied came from a THIN CLIENT's View (`share`): a quit
    /// gesture then detaches that client instead of ending the kernel.
    pub remote: bool,
    /// Set by `leave` under `remote`; the ViewModel detaches the client.
    pub detach: bool,
    pub mouse: bool,
    /// `Ctrl-A` armed for one key.
    pub prefix: bool,
    pub last_ctrl_c: Option<Instant>,
    pub last_q: Option<Instant>,
    pub q_streak: u8,

    // ── outbound ─────────────────────────────────────────────────────────
    out: Vec<Cmd>,
    last_rows: HashMap<AgentId, Instant>,
    /// Per agent, the newest row id any read has RETURNED — the cursor the next
    /// incremental read continues from. Kept apart from the pane store, which
    /// drops rows it does not show (execution rows), so an agent with none shown
    /// would otherwise be re-read from row zero on every tick.
    row_cursor: HashMap<AgentId, crate::kern::model::RowId>,
    /// Agents whose row read is in flight: no second read is issued until its
    /// answer lands (a slow first read would otherwise be repeated, cursorless,
    /// on every tick — measured 3× a 9 MB read while a canvas loaded).
    rows_inflight: HashSet<AgentId>,
    last_ps: Option<Instant>,
    last_tree: Option<Instant>,
    reflect_asked: HashSet<AgentId>,
    watched: HashSet<AgentId>,
}

/// One View's derived state, held by the ViewModel between that View's
/// passes: its screen (what a click hit-tests against) and each panel's own
/// slice (`Panel::take_view`). Every View is arbitrary — a terminal of any
/// size, a thin client, anything that shows the shell — and owns one of these.
#[derive(Default)]
pub struct ViewSlots {
    screen: ratatui::layout::Rect,
    panels: HashMap<AgentId, Box<dyn std::any::Any>>,
}

impl App {
    pub fn new(paths: Paths) -> Self {
        Self {
            seen: Seen::load(&paths.cwd),
            cfg: ClientCfg::here(),
            link: Link::Down {
                why: "probing".into(),
            },
            root_id: String::new(),
            runtime: String::new(),
            guid: None,
            tree: None,
            records: BTreeMap::new(),
            reflects: HashMap::new(),
            collapsed: HashSet::new(),
            trows: Vec::new(),
            sel: 0,
            filter: None,
            filtering: false,
            store: Store::default(),
            tasks: Vec::new(),
            done: HashSet::new(),
            waiting: HashMap::new(),
            panels: HashMap::new(),
            focus: Focus::Tree,
            screen: ratatui::layout::Rect::default(),
            term_opening: false,
            want: None,
            side: Default::default(),
            side_before_rail: Default::default(),
            system_folded: 0,
            provider: None,
            composer: Composer::default(),
            notes: Vec::new(),
            help: false,
            dirty: true,
            stale: false,
            quit: false,
            remote: false,
            detach: false,
            // ON by default: clicks focus. Ctrl-A m turns it off for native copy.
            mouse: true,
            prefix: false,
            last_ctrl_c: None,
            last_q: None,
            q_streak: 0,
            out: Vec::new(),
            last_rows: HashMap::new(),
            row_cursor: HashMap::new(),
            rows_inflight: HashSet::new(),
            last_ps: None,
            last_tree: None,
            reflect_asked: HashSet::new(),
            watched: HashSet::new(),
        }
    }

    pub fn push(&mut self, c: Cmd) {
        self.out.push(c);
    }

    /// The queue the runner gets. Shell-local commands are handled here and
    /// never leave the app.
    pub fn drain_cmds(&mut self) -> Vec<Cmd> {
        let mut out = Vec::new();
        // A jump can queue more (the selected panel's first reads), so loop.
        while !self.out.is_empty() {
            for c in std::mem::take(&mut self.out) {
                match c {
                    Cmd::Jump { agent, row } => self.jump(agent, row),
                    c => out.push(c),
                }
            }
        }
        out
    }

    /// Select `agent` and put its thread's cursor on `row`. A filter that hides
    /// the agent is dropped, and a collapsed ancestor is opened, because the
    /// human asked to SEE it.
    fn jump(&mut self, agent: AgentId, row: RowId) {
        if !self.trows.iter().any(|r| r.id == agent) {
            self.filter = None;
            self.filtering = false;
            if let Some(t) = &self.tree {
                for id in ancestors(t, &agent) {
                    self.collapsed.remove(&id);
                }
            }
            self.reflow();
        }
        let Some(i) = self.trows.iter().position(|r| r.id == agent) else {
            self.note(format!("{agent} is not in the tree any more."));
            return;
        };
        self.sel = i;
        let kind = self.trows[i].kind;
        self.select(agent.clone(), kind);
        if let Some(p) = self.panels.get_mut(&agent) {
            if let Some(ai) = p.as_any_mut().downcast_mut::<crate::panel::ai::AiPanel>() {
                ai.st.cursor = Some(row);
            }
        }
        self.focus = Focus::Panel;
        self.dirty = true;
    }

    /// Select agent `agent` (a filter that hides it is dropped, a collapsed
    /// ancestor opened) with its composer focused — a just-made AI agent.
    fn goto(&mut self, agent: AgentId) {
        if !self.trows.iter().any(|r| r.id == agent) {
            self.filter = None;
            self.filtering = false;
            if let Some(t) = &self.tree {
                for id in ancestors(t, &agent) {
                    self.collapsed.remove(&id);
                }
            }
            self.reflow();
        }
        let Some(i) = self.trows.iter().position(|r| r.id == agent) else {
            self.note(format!("{agent} is not in the tree yet."));
            return;
        };
        self.sel = i;
        let kind = self.trows[i].kind;
        self.select(agent, kind);
        self.focus = Focus::Composer;
        self.dirty = true;
    }

    /// Open the screen on a recipe. `web`: the `web` host. `ai`: the first ai
    /// agent with the composer focused — type and it works; with none, the
    /// set-up page (the recipe has no model to add one with yet).
    fn open_recipe(&mut self, r: crate::recipe::Recipe) {
        use crate::recipe::Recipe;
        let (target, focus) = match r {
            Recipe::Canvas => (self.trows.iter().position(|t| t.id == crate::kern::web::WEB_ID), Focus::Panel),
            // the ROOT AI first (`kern/root_ai`), else any
            Recipe::Ai => match self
                .trows
                .iter()
                .position(|t| t.id == crate::kern::root_ai::ROOT_AI)
                .or_else(|| self.trows.iter().position(|t| t.kind == PanelKind::Ai))
            {
                Some(i) => (Some(i), Focus::Composer),
                None => (self.trows.iter().position(|t| t.id == SETUP_ID), Focus::Panel),
            },
            // the terminal `run` made (with the canvas the program is a cell there)
            Recipe::Run => (self.trows.iter().position(|t| t.id == TERM_ID), Focus::Term),
        };
        let Some(i) = target else {
            self.note(format!("{r:?}: nothing to open"));
            return;
        };
        self.sel = i;
        let (id, kind) = (self.trows[i].id.clone(), self.trows[i].kind);
        self.select(id, kind);
        self.focus = focus;
        self.dirty = true;
    }

    /// Every agent's handler module, from the tree.
    fn modules(&self) -> HashMap<AgentId, String> {
        let mut flat = Vec::new();
        if let Some(t) = &self.tree {
            t.walk(0, &mut flat);
        }
        flat.into_iter()
            .filter_map(|(n, _)| n.handler_module.clone().map(|m| (n.id.clone(), m)))
            .collect()
    }

    /// The sidebar's width for its current view.
    pub fn side_w(&self) -> u16 {
        if self.side == crate::ui::side::SideView::Rail {
            crate::ui::side::RAIL_W
        } else {
            TREE_W
        }
    }

    /// `v`: tree ↔ flat. From the rail it expands to the other one.
    pub fn toggle_side(&mut self) {
        use crate::ui::side::SideView::*;
        let from = if self.side == Rail { self.side_before_rail } else { self.side };
        self.side = if from == Flat { Tree } else { Flat };
        self.reflow();
    }

    /// `<` collapses to the rail, `>` expands back to the view it came from.
    pub fn rail(&mut self, collapse: bool) {
        use crate::ui::side::SideView::*;
        if collapse && self.side != Rail {
            self.side_before_rail = self.side;
            self.side = Rail;
        } else if !collapse && self.side == Rail {
            self.side = self.side_before_rail;
        }
        self.reflow();
    }

    /// Is the in-app terminal an agent in the tree right now?
    pub fn term_open(&self) -> bool {
        self.panels.contains_key(TERM_ID)
    }

    /// `t`: open the terminal (and give it the keyboard), or close it.
    pub fn toggle_term(&mut self) {
        if self.term_open() {
            self.push(Cmd::CloseTerminal);
            return;
        }
        let (cols, rows) = self.term_size();
        self.term_opening = true;
        self.push(Cmd::OpenTerminal { cols, rows });
    }

    /// The size the terminal pane will be drawn at, so a shell started at it
    /// does not redraw its first prompt on a resize.
    fn term_size(&self) -> (u16, u16) {
        let body = self.screen.height.saturating_sub(COMPOSER_H + STATUS_H + TITLE_H);
        let rows = (body * 45 / 100).max(8).min(body.saturating_sub(4)).saturating_sub(1).max(1);
        let cols = self.screen.width.saturating_sub(self.side_w()).max(20);
        (cols, rows)
    }

    /// The tree just changed: a terminal that appeared gets its pane and its
    /// watch; one that vanished loses them.
    fn sync_term(&mut self) {
        let exists = self.all_agents().iter().any(|a| a == TERM_ID);
        if exists && !self.term_open() {
            self.select(TERM_ID.into(), PanelKind::Terminal);
            if !self.reflect_asked.contains(TERM_ID) {
                self.reflect_asked.insert(TERM_ID.into());
                self.push(Cmd::Reflect { agent: TERM_ID.into() });
            }
            // A `term` this run did not open (rehydrated from disk) has no pane
            // attached to a job: `boot` starts nothing, so the client attaches —
            // to its live job, or a fresh shell.
            if !self.term_opening {
                let (cols, rows) = self.term_size();
                self.push(Cmd::OpenTerminal { cols, rows });
            }
        }
        if !exists && self.term_open() {
            self.panels.remove(TERM_ID);
            self.watched.remove(TERM_ID);
            self.reflect_asked.remove(TERM_ID);
            if self.focus == Focus::Term {
                self.focus = Focus::Tree;
            }
        }
        if exists && self.term_opening {
            self.term_opening = false;
            self.focus = Focus::Term;
            if let Some(p) = self.panels.get_mut(TERM_ID) {
                if let Some(t) = p.as_any_mut().downcast_mut::<crate::panel::terminal::TerminalPanel>() {
                    t.takeover = true;
                }
            }
        }
    }

    /// Every agent id in the tree, collapsed or not.
    pub fn all_agents(&self) -> Vec<AgentId> {
        let mut flat = Vec::new();
        if let Some(t) = &self.tree {
            t.walk(0, &mut flat);
        }
        flat.into_iter()
            .filter(|(n, _)| n.handler_module.is_some())
            .map(|(n, _)| n.id.clone())
            .collect()
    }

    /// A loud failure. Kept, not replaced: a second problem does not erase the
    /// first, and the statusline pages through them.
    pub fn note(&mut self, s: impl Into<String>) {
        let s = s.into();
        if self.notes.last() != Some(&s) {
            self.notes.push(s);
        }
        self.dirty = true;
    }

    /// ENTER a View: its derived state goes back where the layout and the keys
    /// read it. Paired with [`App::leave`].
    pub fn enter(&mut self, mut v: ViewSlots) {
        self.screen = v.screen;
        for (id, p) in self.panels.iter_mut() {
            if let Some(s) = v.panels.remove(id) {
                p.put_view(s);
            }
        }
    }

    /// LEAVE the current View: its derived state comes out, the panels keep
    /// their defaults until the next `enter`.
    pub fn leave(&mut self) -> ViewSlots {
        ViewSlots {
            screen: std::mem::take(&mut self.screen),
            panels: self.panels.iter_mut().filter_map(|(id, p)| p.take_view().map(|s| (id.clone(), s))).collect(),
        }
    }

    /// A quit gesture: it ends the shell (and the kernel) when it comes from the
    /// owner's own terminal, and only DETACHES a thin client's View.
    pub fn quit_or_detach(&mut self) {
        if self.remote {
            self.detach = true;
        } else {
            self.quit = true;
        }
    }

    /// TRUE while something on screen is a function of the clock rather than of
    /// state. Everything here animates continuously, so the frame clock draws it
    /// unconditionally; the live shell stays event-driven and costs nothing idle.
    pub fn animating(&self) -> bool {
        self.selected().and_then(|id| self.panels.get(id)).is_some_and(|p| p.animating())
    }

    pub fn selected(&self) -> Option<&AgentId> {
        self.trows.get(self.sel).map(|r| &r.id)
    }

    pub fn selected_kind(&self) -> PanelKind {
        self.trows
            .get(self.sel)
            .map(|r| r.kind)
            .unwrap_or(PanelKind::Generic)
    }

    /// The context a panel renders from. Borrows; copies nothing.
    pub fn cx<'a>(&'a self, agent: &'a AgentId) -> Cx<'a> {
        Cx {
            agent,
            store: &self.store,
            tasks: &self.tasks,
            seen: self.seen.ids(),
            reflect: self.reflects.get(agent),
        }
    }

    /// Make the panel for an agent if it does not exist yet, and tell it it was
    /// selected. Panels persist for the session so the human's view state does.
    pub fn select(&mut self, agent: AgentId, kind: PanelKind) {
        let fresh = !self.panels.contains_key(&agent);
        if fresh {
            let mut p = panel::make(kind, &agent);
            if let Some(a) = p.as_any_mut().downcast_mut::<ActivityPanel>() {
                a.set_agents(self.all_agents());
            }
            self.panels.insert(agent.clone(), p);
        }
        // Take the panel out so `self` can be borrowed for the context.
        let Some(mut p) = self.panels.remove(&agent) else {
            return;
        };
        let mut out = Vec::new();
        {
            let cx = self.cx(&agent);
            p.on_select(&cx, &mut out);
        }
        self.panels.insert(agent, p);
        self.out.extend(out);
        self.dirty = true;
    }

    /// Re-flatten the tree. Called whenever anything it reads changes, because the
    /// flattened list is a projection and keeping it stale would show a kernel
    /// that no longer exists.
    pub fn reflow(&mut self) {
        let keep = self.selected().cloned();
        self.trows = tree::flatten(&tree::Src {
            tree: self.tree.as_ref(),
            collapsed: &self.collapsed,
            filter: self.filter.as_deref(),
            tasks: &self.tasks,
            waiting: &self.waiting,
            done: &self.done,
        });
        use crate::ui::side::SideView;
        self.system_folded = 0;
        if self.side != SideView::Tree && !self.trows.is_empty() {
            // One level. A collapse means nothing here, so it is not applied.
            let all = tree::flatten(&tree::Src {
                tree: self.tree.as_ref(),
                collapsed: &HashSet::new(),
                filter: self.filter.as_deref(),
                tasks: &self.tasks,
                waiting: &self.waiting,
                done: &self.done,
            });
            let modules = self.modules();
            let provider = self.provider.clone();
            let root = self.root_id.clone();
            let (rows, folded) = crate::ui::side::flatten(all, |r| {
                r.id == root
                    || provider.as_deref() == Some(r.id.as_str())
                    || modules.get(&r.id).is_some_and(|m| m.ends_with("_loader.tools"))
            });
            self.trows = rows;
            self.system_folded = folded;
        }
        // THE ACTIVITY ROW: right under the kernel's own row, never while
        // filtering (a filter lists agents, and this is not one).
        if self.filter.is_none() && !self.trows.is_empty() {
            let open: usize = self.waiting.values().sum();
            self.trows.insert(1, TreeRow {
                id: ACTIVITY_ID.into(),
                depth: 1,
                label: "Activity".into(),
                kind: PanelKind::Activity,
                live: false,
                has_children: false,
                collapsed: false,
                presence: if open > 0 { Presence::Blocked } else { Presence::Idle },
                inquiries: open,
            });
        }
        // THE SET-UP ROW, last: how a new ai agent gets written.
        if self.filter.is_none() && !self.trows.is_empty() {
            self.trows.push(TreeRow {
                id: SETUP_ID.into(),
                depth: 1,
                label: "add an ai agent".into(),
                kind: PanelKind::Setup,
                live: false,
                has_children: false,
                collapsed: false,
                presence: Presence::Idle,
                inquiries: 0,
            });
        }
        let agents = self.all_agents();
        if let Some(p) = self.panels.get_mut(ACTIVITY_ID) {
            if let Some(a) = p.as_any_mut().downcast_mut::<ActivityPanel>() {
                a.set_agents(agents);
            }
        }
        // The selection follows the AGENT, not the index: a new agent appearing
        // above it must not move the human's cursor onto something else.
        self.sel = keep
            .and_then(|id| self.trows.iter().position(|r| r.id == id))
            .unwrap_or_else(|| self.sel.min(self.trows.len().saturating_sub(1)));
        self.dirty = true;
    }

    // ── events ───────────────────────────────────────────────────────────

    pub fn apply(&mut self, ev: Ev) {
        match ev {
            Ev::Term(e) => crate::keys::handle(self, e),
            Ev::Link(l) => self.on_link(l),
            Ev::Got(g) => self.on_got(g),
            Ev::Note(n) => self.note(n),
        }
    }

    fn on_link(&mut self, l: LinkEv) {
        match l {
            LinkEv::State(e) => {
                // THIS shell's own reads publish too: never let them re-arm a read
                // (the loop that kept an idle canvas reading ~3×/s, measured)
                if e.sender.is_none() && e.verb.as_deref().is_some_and(|v| OWN_READS.contains(&v)) {
                    return;
                }
                self.last_ps = None;
                // The kernel publishes one event per dispatch. Both ends moved, so
                // both ends' rows are stale — and a create/delete verb means the
                // tree itself changed.
                for id in [e.sender.as_ref(), e.target.as_ref()].into_iter().flatten() {
                    self.last_rows.remove(id);
                }
                if matches!(
                    e.verb.as_deref(),
                    Some("create_agent") | Some("delete_agent") | Some("update_agent")
                ) {
                    self.last_tree = None;
                    if let Some(t) = &e.target {
                        self.reflect_asked.remove(t);
                    }
                }
                self.reflow();
            }
            // The kernel's own row tail said so. This is the ONE trigger that is
            // a fact rather than a guess: clearing the stamp makes the next
            // cadence pass re-read that agent immediately.
            LinkEv::RowsMoved { agent } => {
                self.last_rows.remove(&agent);
                self.last_ps = None;
            }
            LinkEv::AgentEvent { agent, payload } => {
                // STREAMING IS FOR THE HUMAN (owner, 2026-10-04): an AI's parts are
                // drawn only when they answer THIS client; a message between
                // agents is read whole, as its row.
                if is_part(&payload) && payload.get("client_id").and_then(Value::as_str) != Some(self.cfg.client_id.as_str()) {
                    return;
                }
                if let Some(mut p) = self.panels.remove(&agent) {
                    let mut out = Vec::new();
                    let dirty = p.on_event(&payload, &mut out);
                    self.panels.insert(agent.clone(), p);
                    self.out.extend(out);
                    self.dirty |= dirty;
                }
                // A token or a status means the log is moving; read it rather than
                // reconstructing the conversation from the stream.
                self.last_rows.remove(&agent);
            }
        }
    }

    fn on_got(&mut self, g: Got) {
        match g {
            Got::RootReflect {
                id,
                runtime,
                provider,
                guid,
            } => {
                self.root_id = id;
                self.runtime = runtime;
                self.guid = guid;
                self.provider = provider.clone();
                if provider.is_none() {
                    self.note("no store wired — rows will not survive a restart.");
                }
                self.dirty = true;
            }
            Got::Tree(t) => {
                self.tree = Some(t);
                self.last_tree = Some(Instant::now());
                self.reflow();
                self.sync_term();
                if let Some(w) = self.want.take() {
                    self.open_recipe(w);
                }
                // First tree: open the pane for whatever the cursor landed on, so
                // the shell is usable without a keystroke.
                if let (Some(a), k) = (self.selected().cloned(), self.selected_kind()) {
                    if !self.panels.contains_key(&a) {
                        self.select(a, k);
                    }
                }
            }
            Got::TermAttached { job_id, replay } => {
                if let Some(p) = self.panels.get_mut(TERM_ID) {
                    if let Some(t) = p.as_any_mut().downcast_mut::<crate::panel::terminal::TerminalPanel>() {
                        t.attach(job_id, &replay);
                    }
                }
                self.dirty = true;
            }
            Got::Agents { records } => {
                self.records = records.into_iter().map(|r| (r.id.clone(), r)).collect();
                self.dirty = true;
            }
            Got::Ps(tasks) => {
                let busy_before: HashSet<AgentId> = self
                    .tasks
                    .iter()
                    .filter(|t| crate::presence::is_work(t))
                    .map(|t| t.agent.clone())
                    .collect();
                let busy_now: HashSet<AgentId> = tasks
                    .iter()
                    .filter(|t| crate::presence::is_work(t))
                    .map(|t| t.agent.clone())
                    .collect();
                // An agent that was working and is not any more just finished.
                for a in busy_before.difference(&busy_now) {
                    self.done.insert(a.clone());
                    self.last_rows.remove(a);
                }
                for a in &busy_now {
                    self.done.remove(a);
                }
                self.tasks = tasks;
                self.last_ps = Some(Instant::now());
                self.reflow();
            }
            Got::Rows { agent, rows, full, loaded } => {
                self.rows_inflight.remove(&agent);
                if loaded == Some(false) {
                    self.store.unloaded.insert(agent.clone());
                } else {
                    self.store.unloaded.remove(&agent);
                }
                if let Some(newest) = rows.iter().map(|r| r.id).max() {
                    let c = self.row_cursor.entry(agent.clone()).or_insert(newest);
                    *c = (*c).max(newest);
                }
                if full {
                    self.store.apply_full(&agent, rows);
                } else {
                    self.store.apply_page(&agent, rows);
                }
                // "Waiting on you" is a QUERY over rows. Recomputed here, where the
                // rows changed, and nowhere else.
                let n = thread::waiting_for(&self.store, &agent, &self.cfg.user).len();
                self.waiting.insert(agent.clone(), n);
                self.last_rows.insert(agent, Instant::now());
                self.reflow();
            }
            Got::Reflect { agent, reply } => {
                self.reflects.insert(agent.clone(), reply);
                self.reflect_asked.insert(agent);
                self.dirty = true;
            }
            Got::Sent { target, reply } => {
                self.on_reply(&target, &reply);
            }
            Got::Cancelled { killed } => {
                if killed.is_empty() {
                    self.note("nothing to cancel — that task had already settled.");
                } else {
                    self.note(format!("cancelled {} task(s).", killed.len()));
                }
            }
            Got::RosterBooted { created, reapplied } => {
                self.note(format!(
                    "roster booted: created {}, reapplied {}.",
                    if created.is_empty() {
                        "nothing".into()
                    } else {
                        created.join(", ")
                    },
                    reapplied.len()
                ));
                self.last_tree = None;
            }
            Got::Backends(b) => {
                if let Some(p) = self.panels.get_mut(SETUP_ID) {
                    if let Some(s) = p.as_any_mut().downcast_mut::<SetupPanel>() {
                        s.set_backends(b);
                    }
                }
                self.dirty = true;
            }
            Got::KeyMode(m) => {
                if let Some(p) = self.panels.get_mut(SETUP_ID) {
                    if let Some(s) = p.as_any_mut().downcast_mut::<SetupPanel>() {
                        s.set_key_mode(m);
                    }
                }
                self.dirty = true;
            }
            Got::Saved(t) => {
                if let Some(p) = self.panels.get_mut(SETUP_ID) {
                    if let Some(s) = p.as_any_mut().downcast_mut::<SetupPanel>() {
                        s.set_shelf(t);
                    }
                }
                self.dirty = true;
            }
            Got::Keys(k) => {
                if let Some(p) = self.panels.get_mut(SETUP_ID) {
                    if let Some(s) = p.as_any_mut().downcast_mut::<SetupPanel>() {
                        s.set_keys(k);
                    }
                }
                self.dirty = true;
            }
            Got::Created(r) => {
                if let Some(p) = self.panels.get_mut(SETUP_ID) {
                    if let Some(s) = p.as_any_mut().downcast_mut::<SetupPanel>() {
                        s.set_created(&r);
                    }
                }
                // the new agent selected, its chat open, the composer ready
                if let Ok(id) = r {
                    self.goto(id.as_str().into());
                }
                self.dirty = true;
            }
            Got::Setup(r) => {
                if let Some(p) = self.panels.get_mut(SETUP_ID) {
                    if let Some(s) = p.as_any_mut().downcast_mut::<SetupPanel>() {
                        s.set_result(r);
                    }
                }
                self.dirty = true;
            }
            Got::Refused { what, err } => self.on_refusal(what, err),
        }
    }

    /// A reply to something the human sent. Routed to the panel that asked, which
    /// is why every panel parses its own replies.
    fn on_reply(&mut self, target: &AgentId, reply: &Value) {
        // A refusal INSIDE a reply is still data and still belongs on screen.
        if let Some(e) = reply.get("error").and_then(Value::as_str) {
            self.note(format!("{target}: {e}"));
        }
        let mut out = Vec::new();
        if let Some(mut p) = self.panels.remove(target) {
            p.on_reply(reply, &mut out);
            self.panels.insert(target.clone(), p);
        }
        self.out.extend(out);
        self.last_rows.remove(target);
        self.dirty = true;
    }

    fn on_refusal(&mut self, what: What, err: KernelErr) {
        // The pane that asked shows the answer. A refusal IS the answer, and the
        // statusline can only hold one truncated line of it.
        if let KernelErr::Refused(r) = &err {
            if let Ok(v) = serde_json::to_value(r.as_ref()) {
                if let Some(agent) = self.selected().cloned() {
                    if let Some(mut p) = self.panels.remove(&agent) {
                        let mut out = Vec::new();
                        p.on_reply(&v, &mut out);
                        self.panels.insert(agent, p);
                        self.out.extend(out);
                    }
                }
            }
        }
        // The kernel ships a `hint` on an authorization refusal, and it names the
        // exact grant that is missing — discovery through denial. Show it
        // verbatim; this shell must never paraphrase the kernel's own advice.
        let extra = match (&what, &err) {
            (What::Send, KernelErr::Refused(r)) if r.side.as_deref() == Some("egress") => format!(
                " Name it in {}.can_call in .fantastic/kernel.yaml, then boot the roster.",
                self.cfg.user
            ),
            (What::Send, KernelErr::Refused(r)) if r.side.as_deref() == Some("ingress") => {
                " The target does not accept this sender: name it in that agent's \
                 callable_by, then boot the roster."
                    .to_string()
            }
            _ => String::new(),
        };
        self.note(format!("{err}{extra}"));
    }

    // ── the clock ────────────────────────────────────────────────────────

    pub fn on_tick(&mut self) {
        // The composer's caret blinks and presence can change with nothing
        // arriving, so a tick is always a redraw.
        self.dirty = true;
        let due = self.due();
        self.out.extend(due);
    }

    /// WHAT TO FETCH NOW. The hot set is the root, the selection, anything with a
    /// backlog, and anything with real work in flight. The root is always hot
    /// because SETTLE ROWS LIVE THERE — an agent's own log never records that its
    /// call finished.
    fn due(&mut self) -> Vec<Cmd> {
        if self.link != Link::Up {
            return Vec::new();
        }
        let now = Instant::now();
        let mut cmds = Vec::new();
        // `R`: everything is read again, whole
        let full = std::mem::take(&mut self.stale);
        if full {
            self.last_rows.clear();
            self.last_ps = None;
            self.last_tree = None;
        }

        let mut hot: Vec<AgentId> = Vec::new();
        if !self.root_id.is_empty() {
            hot.push(self.root_id.clone());
        }
        match self.selected().cloned() {
            // The Activity page reads every agent's rows, so while it is on
            // screen every agent is hot.
            Some(a) if a == ACTIVITY_ID => hot.extend(self.all_agents()),
            // Other pages that are not agents read nothing of their own.
            Some(a) if a.starts_with('#') => {}
            Some(a) => hot.push(a),
            None => {}
        }
        for t in &self.tasks {
            if crate::presence::is_work(t) {
                hot.push(t.agent.clone());
            }
        }
        hot.sort();
        hot.dedup();

        for a in hot {
            // A panel that keeps no conversation is not polled for rows. A terminal
            // and a file surface are streams and listings, not logs.
            let wants_rows = self.panels.get(&a).map(|p| p.needs().rows).unwrap_or(true);
            if !wants_rows && Some(&a) != self.selected() && self.selected().map(String::as_str) != Some(ACTIVITY_ID) {
                continue;
            }
            // EVENT-DRIVEN (owner 2026-10-08: no idle loops): an agent's rows are
            // read when its stamp is gone — never read yet, or an event said they
            // moved (`on_link`) — never on elapsed time
            if !self.rows_inflight.contains(&a) && !self.last_rows.contains_key(&a) {
                self.rows_inflight.insert(a.clone());
                cmds.push(Cmd::Rows {
                    agent: a.clone(),
                    full,
                    after: if full { None } else { self.row_cursor.get(&a).copied() },
                });
                // Reserve the slot so a slow page does not queue a second read.
                self.last_rows.insert(a.clone(), now);
            }
            if !self.reflect_asked.contains(&a) {
                self.reflect_asked.insert(a.clone());
                cmds.push(Cmd::Reflect { agent: a.clone() });
            }
        }

        // the tasks and the tree, likewise: read when an event cleared them
        if self.last_ps.is_none() {
            self.last_ps = Some(now);
            cmds.push(Cmd::Ps);
        }
        if self.last_tree.is_none() {
            self.last_tree = Some(now);
            cmds.push(Cmd::Tree);
            cmds.push(Cmd::Agents);
        }

        // One watch for the selection, and one only: a watch frame carries no
        // source id, so a socket is per agent and the cap is real.
        if let Some(a) = self.selected().cloned() {
            let wants = self
                .panels
                .get(&a)
                .map(|p| p.needs().watch)
                .unwrap_or(false);
            if wants && self.watched.insert(a.clone()) {
                cmds.push(Cmd::Watch(a));
            }
        }

        cmds
    }

    /// Queue what the last events made due (`due`).
    pub fn pump(&mut self) {
        let due = self.due();
        self.out.extend(due);
    }

    /// Send whatever is in the composer. The rule for `re` is the async law made
    /// concrete: an answer is an ordinary send that points back at the row which
    /// asked, and there is no other kind of answer.
    pub fn submit(&mut self) {
        if self.composer.mode == Mode::Locked {
            self.note("acts are unavailable in this kernel — see the note above.");
            return;
        }
        let Some(target) = self.selected().cloned().filter(|a| !a.starts_with('#')) else {
            self.note("select an agent first.");
            return;
        };
        let verb_mode = self.composer.mode == Mode::Verb || self.composer.text.starts_with('/');
        let re = self.composer.re.take();
        let Some(line) = self.composer.take() else {
            return;
        };

        if verb_mode && !line.starts_with("/q ") {
            match Composer::parse_verb(&line) {
                Ok(payload) => self.out.push(Cmd::Send { target, payload }),
                Err(e) => self.note(format!("not a verb call: {e}")),
            }
            return;
        }
        let text = Composer::payload_text(&line).to_string();
        let client_id = self.cfg.client_id.clone();
        let payload = crate::panel::ai::say_payload(&target, &text, &client_id, re);
        self.out.push(Cmd::Send { target, payload });
    }

    /// Mark the rows the human has now looked at, so a `✉` clears when it should.
    pub fn mark_seen(&mut self, ids: impl IntoIterator<Item = RowId>) {
        for id in ids {
            self.seen.mark(id);
        }
        self.seen.flush();
    }
}
/// The ids from the root down to `id`'s parent.
fn ancestors(t: &TreeNode, id: &str) -> Vec<AgentId> {
    fn go(n: &TreeNode, id: &str, path: &mut Vec<AgentId>) -> bool {
        if n.id == id {
            return true;
        }
        path.push(n.id.clone());
        if n.children.iter().any(|c| go(c, id, path)) {
            return true;
        }
        path.pop();
        false
    }
    let mut path = Vec::new();
    go(t, id, &mut path);
    path
}

/// Whether an agent event is a streamed part (`part_start` / `_delta` / `_end` /
/// `_abort`).
fn is_part(payload: &Value) -> bool {
    payload.get("type").and_then(Value::as_str).is_some_and(|t| t.starts_with("part_"))
}
