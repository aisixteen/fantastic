//! The LIVE **plan panel** — a mind's loop, made visible and cancelable.
//!
//! A mind narrates its own loop by EMITTING events on itself; the TUI renders
//! whatever arrives and hard-codes no loop of its own (exactly how
//! [`crate::question_card`] works — the card is whatever a `{type:"question"}` says
//! it is):
//!
//! - `{type:"plan", steps:[…], source, client_id}` → opens a [`PlanPanel`].
//! - `{type:"step", index, total, text, status:"running"|"done", source}` → advances it.
//! - `{type:"done"}` (or a final `say`) from that source → settles / closes it.
//!
//! Anything that doesn't parse falls through to the normal transcript rendering as a
//! dim note — a malformed event is still SHOWN, never swallowed.
//!
//! ## Cancel is `ps` then `kill` — exactly like a shell
//!
//! A mind is BLOCKED inside its own `kernel.send('brain', …)`, so it can neither
//! cancel itself nor learn its own task id (the kernel mints it). The UI therefore
//! DISCOVERS the work: **`ps`** the kernel's task table, pick the in-flight rows,
//! **`cancel`** them ([`cancel_targets`]). Killing the brain call makes the mind's
//! blocked `send` resolve to `{"cancelled": true}` — its loop stops cleanly and the
//! script STAYS ALIVE for the next turn. Killing the mind's own job would kill the
//! script itself, which is why [`cancel_targets`] never returns a driver's own row.
//!
//! The mind's script IS a row (the script runtimes register it with the kernel's
//! `spawn_task`), so the table is a real tree and "the mind's work" is simply "the
//! children of the mind's rows" — the shell reads the tree, it does not guess at it.
//!
//! Pure-testable like its siblings: [`PlanPanel::from_event`], [`PlanPanel::on_step`],
//! [`PlanPanel::settle`] and [`cancel_targets`] need no terminal and no kernel;
//! [`PlanPanel::render`] lives here too, so `lib.rs` keeps only the wiring (one field,
//! two event arms, one key shim, one render call).

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};
use serde_json::{json, Value};

use crate::chat::{Body, State};
use crate::kernels::ActiveKernel;
use crate::onboard::OnboardEvent;
use crate::{App, BRAIN_ID};
use fantastic_kernel::AgentId;

/// Where one step of the plan stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Not started yet.
    Pending,
    /// The step the mind is executing right now.
    Running,
    /// Finished.
    Done,
}

/// One step of a live plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub text: String,
    pub status: Status,
}

/// The panel's own lifecycle. `Live` while the mind works; `Cancelling` from the
/// moment the cancel key is pressed until the kernel answers; then `Cancelled` (the
/// kill landed) or `Settled` (the mind finished on its own).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelState {
    Live,
    Cancelling,
    Cancelled,
    Settled,
}

/// A live plan, as narrated by the agent running it. Built from a `plan` event
/// ([`PlanPanel::from_event`]); advanced by `step` events ([`PlanPanel::on_step`]).
pub struct PlanPanel {
    /// The agent whose loop this is — the `ps`/`cancel` driver, and the source whose
    /// `done`/`say` settles the panel.
    pub source: String,
    /// Carried through from the event (multi-client routing parity with the card);
    /// the panel itself never sends, so it is parsed but not otherwise read.
    #[allow(dead_code)]
    pub client_id: String,
    pub steps: Vec<Step>,
    pub state: PanelState,
    /// The task ids the kernel actually killed (shown in the panel's footer, so a
    /// cancel that hit nothing is never mistaken for one that worked).
    pub killed: Vec<String>,
}

impl PlanPanel {
    /// Parse a `{type:"plan", steps:[…], source}` event. `None` if it isn't a plan, if
    /// `source` is missing/empty (nothing to cancel, nothing to settle it), or if it
    /// carries no steps — a plan with no steps is not a plan, and must fall through to
    /// the transcript rather than open an empty panel.
    ///
    /// `steps` may be strings (`["a","b"]`) or objects with a `text`/`step` field —
    /// duck-typed, because the mind that emits them is authored code, not a built-in.
    pub fn from_event(ev: &Value) -> Option<PlanPanel> {
        if ev.get("type").and_then(Value::as_str) != Some("plan") {
            return None;
        }
        let source = non_empty(ev.get("source"))?;
        let client_id = ev
            .get("client_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let steps: Vec<Step> = ev
            .get("steps")
            .and_then(Value::as_array)?
            .iter()
            .filter_map(step_text)
            .map(|text| Step {
                text,
                status: Status::Pending,
            })
            .collect();
        (!steps.is_empty()).then(|| PlanPanel {
            source,
            client_id,
            steps,
            state: PanelState::Live,
            killed: Vec::new(),
        })
    }

    /// Apply a `{type:"step", index, text, status}` event: highlight the running step
    /// and mark everything before it done (so a dropped event can never leave the
    /// panel stuck behind the mind). `text` REFRESHES the step's label when present —
    /// a mind may narrate a step it never planned.
    ///
    /// Returns `false` when the event isn't a step for THIS panel (wrong type, wrong
    /// source, index out of range) — the caller then renders it as a normal note.
    pub fn on_step(&mut self, ev: &Value) -> bool {
        if ev.get("type").and_then(Value::as_str) != Some("step") {
            return false;
        }
        if let Some(src) = ev.get("source").and_then(Value::as_str) {
            if src != self.source {
                return false;
            }
        }
        let Some(i) = ev.get("index").and_then(Value::as_u64).map(|i| i as usize) else {
            return false;
        };
        let Some(step) = self.steps.get_mut(i) else {
            return false;
        };
        if let Some(t) = ev
            .get("text")
            .and_then(Value::as_str)
            .filter(|t| !t.trim().is_empty())
        {
            step.text = t.to_string();
        }
        let done = ev.get("status").and_then(Value::as_str) == Some("done");
        step.status = if done { Status::Done } else { Status::Running };
        // Everything BEFORE the step in flight is finished, whatever we missed.
        for s in self.steps.iter_mut().take(i) {
            s.status = Status::Done;
        }
        true
    }

    /// The mind finished on its own (`done`, or its final `say`): every step is done
    /// and the panel settles. A CANCELLED panel is left alone — the mind's own
    /// `(cancelled)` say must not repaint it as a success.
    pub fn settle(&mut self) {
        if matches!(self.state, PanelState::Cancelled) {
            return;
        }
        for s in &mut self.steps {
            s.status = Status::Done;
        }
        self.state = PanelState::Settled;
    }

    /// True once every step has finished — the discriminator between the mind's FINAL
    /// say (the loop is over → the panel settles) and a say it makes MID-loop (chatter
    /// → the panel keeps running, and Esc still cancels).
    pub fn is_complete(&self) -> bool {
        self.steps.iter().all(|s| s.status == Status::Done)
    }

    /// The cancel key was pressed — `ps`/`cancel` is in flight.
    pub fn cancelling(&mut self) {
        self.state = PanelState::Cancelling;
    }

    /// The kernel answered the cancel: `killed` names the tasks it actually stopped
    /// (empty = nothing was in flight — reported, never faked).
    pub fn cancelled(&mut self, killed: Vec<String>) {
        self.state = PanelState::Cancelled;
        self.killed = killed;
    }

    /// True while the loop is running (or being cancelled) — i.e. while the cancel key
    /// still means something.
    pub fn is_live(&self) -> bool {
        matches!(self.state, PanelState::Live | PanelState::Cancelling)
    }

    /// Rows the panel WANTS — a border pair + one row per step + the footer rail —
    /// clamped to the rows it actually HAS (`cap`). Never more: a panel that overran the
    /// frame would index outside the buffer.
    pub(crate) fn height(&self, cap: u16) -> u16 {
        (self.steps.len() as u16 + 3).min(cap)
    }

    /// Render the panel DOCKED to the top of the chat inset (below the header rail):
    /// opaque, so the transcript never bleeds through, and out of the way of the
    /// centered question card — the user reads the question IN THE CONTEXT of the plan.
    /// Drawn before the card, so a card that does reach it layers over it. A frame with
    /// no room below the header draws NOTHING (clamped, never a panic).
    pub fn render(&self, f: &mut Frame, within: Rect) {
        let mag = Color::Indexed(165);
        let dim = Style::default().fg(Color::DarkGray);
        let (title, title_st) = match self.state {
            PanelState::Live => (
                format!(" plan — {} ", self.source),
                Style::default()
                    .fg(Color::LightMagenta)
                    .add_modifier(Modifier::BOLD),
            ),
            PanelState::Cancelling => (
                format!(" plan — {} · cancelling… ", self.source),
                Style::default().fg(Color::Yellow),
            ),
            PanelState::Cancelled => (
                format!(" plan — {} · cancelled ", self.source),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            PanelState::Settled => (
                format!(" plan — {} · done ", self.source),
                Style::default().fg(Color::DarkGray),
            ),
        };
        // The panel spans the inset, docked under the 3-row header rail — and takes only
        // the rows that are actually there (a cramped frame simply gets no panel).
        let h = self.height(within.height.saturating_sub(3));
        if h == 0 || within.width == 0 {
            return;
        }
        let area = Rect {
            x: within.x,
            y: within.y + 3,
            width: within.width,
            height: h,
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(title_st)
            .title(Span::styled(title, title_st))
            .title_bottom(Line::from(Span::styled(self.footer(), dim)).right_aligned());
        let inner = block.inner(area);

        let mut lines: Vec<Line> = Vec::new();
        for (i, s) in self.steps.iter().enumerate() {
            let (mark, st) = match s.status {
                Status::Done => (
                    "✔ ",
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::DIM),
                ),
                Status::Running => ("▸ ", Style::default().fg(mag).add_modifier(Modifier::BOLD)),
                Status::Pending => ("· ", Style::default().fg(Color::Gray)),
            };
            lines.push(Line::from(vec![
                Span::styled(format!(" {} ", i + 1), dim),
                Span::styled(mark, st),
                Span::styled(s.text.clone(), st),
            ]));
        }
        crate::fill_black(f.buffer_mut(), area);
        f.render_widget(block, area);
        f.render_widget(Paragraph::new(lines), inner);
    }

    /// The bottom rail: the key while it's live, the outcome once it isn't.
    fn footer(&self) -> String {
        match self.state {
            PanelState::Live => " esc cancels ".to_string(),
            PanelState::Cancelling => " ⟳ cancelling… ".to_string(),
            PanelState::Cancelled if self.killed.is_empty() => {
                " ⊘ nothing was in flight ".to_string()
            }
            PanelState::Cancelled => format!(" ⊘ killed {} ", self.killed.join(" · ")),
            PanelState::Settled => " ✔ done ".to_string(),
        }
    }
}

/// A present, non-empty string field, else `None`.
fn non_empty(v: Option<&Value>) -> Option<String> {
    let s = v.and_then(Value::as_str)?;
    (!s.is_empty()).then(|| s.to_string())
}

/// One `steps[i]` → its label. A string, or an object carrying `text` / `step`.
fn step_text(v: &Value) -> Option<String> {
    let s = match v {
        Value::String(s) => s.clone(),
        Value::Object(_) => v
            .get("text")
            .or_else(|| v.get("step"))
            .and_then(Value::as_str)?
            .to_string(),
        _ => return None,
    };
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

// ── ps → cancel ─────────────────────────────────────────────────────────────

/// **The discovery**: which task ids to kill so the in-flight turn stops, given the `ps`
/// reply's rows, the agent whose loop it is (`driver` — a mind, or `""` for a bare `@ai`
/// turn) and the agent the turn EXECUTES in (`executor` — the brain).
///
/// There is ONE rule per KIND OF CALLER, and which one applies is decided by `driver`
/// alone — never by what happens to be in the table:
///
/// ① **A MIND** (`driver` names an agent). Its script is a TASK (`{agent:<mind>,
///    verb:"script"}` — the js/lua runtimes register it with `spawn_task`), and
///    everything it is blocked in is a CHILD of that row. Kill the CHILDREN — never the
///    mind's own rows, which ARE its script: killing one is `stop`, and the mind must
///    survive to answer the next turn. Nothing in flight under the mind ⇒ nothing to
///    kill: we do NOT fall through to ②, because the root-most dispatch on the brain may
///    belong to somebody else entirely (another mind, or the `@ai` chat) and Esc must
///    never reach across chats.
///
/// ② **THE TUI ITSELF** (`driver` is empty — a bare `@ai` turn). The blocked caller is
///    the shell, which is not an agent and holds no row, so there are no children to
///    look under: the work is the root-most in-flight dispatch on the `executor`. This
///    is the ONLY case ② serves.
///
/// Either way the killed task's caller — the mind, or the TUI — sees its `send` resolve
/// to `{"cancelled": true}` and stops cleanly; the kernel kills the subtree, so nothing
/// the turn spawned survives it.
///
/// Returns the TOP-MOST ids only (the kernel kills each id's whole subtree — passing a
/// descendant too would just be a `{"cancelled": false}` on an already-dead id).
pub fn cancel_targets(rows: &[Value], driver: &str, executor: &str) -> Vec<String> {
    let row_id = |r: &Value| r.get("id").and_then(Value::as_str).map(str::to_string);
    let agent_of = |r: &Value| {
        r.get("agent")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let parent_of = |r: &Value| r.get("parent").and_then(Value::as_str).map(str::to_string);

    // ② The TUI's own turn: it has no row, so the work is the executor's root-most
    //    in-flight dispatch.
    if driver.is_empty() {
        let on_executor: Vec<String> = rows
            .iter()
            .filter(|r| agent_of(r) == executor)
            .filter_map(row_id)
            .collect();
        return top_most(rows, on_executor);
    }

    // ① A mind: its OWN rows are its script — never a target. What it is blocked in is
    //    exactly their children.
    let own: Vec<String> = rows
        .iter()
        .filter(|r| agent_of(r) == driver)
        .filter_map(row_id)
        .collect();
    let kids: Vec<String> = rows
        .iter()
        .filter(|r| parent_of(r).is_some_and(|p| own.contains(&p)))
        .filter_map(row_id)
        .collect();
    top_most(rows, kids)
}

/// Drop any id whose parent chain already leads to another id in the set — the kernel
/// kills subtrees, so only the top-most ids are worth sending.
fn top_most(rows: &[Value], ids: Vec<String>) -> Vec<String> {
    let parent = |id: &str| -> Option<String> {
        rows.iter()
            .find(|r| r.get("id").and_then(Value::as_str) == Some(id))
            .and_then(|r| r.get("parent").and_then(Value::as_str).map(str::to_string))
    };
    ids.iter()
        .filter(|id| {
            let mut p = parent(id);
            while let Some(pid) = p {
                if ids.contains(&pid) {
                    return false; // an ancestor is already in the kill set
                }
                p = parent(&pid);
            }
            true
        })
        .cloned()
        .collect()
}

/// The cancel gesture: **`ps` the kernel, then `cancel` what the turn is blocked in**
/// ([`cancel_targets`]) — async, on the module discipline the whole shell follows: a
/// spawned task owns a cloned kernel handle and reports back through `onboard_tx` →
/// the `select!` loop → [`on_cancelled`], the ONLY place the result touches UI state.
///
/// Both dispatches ride the QUIET [`ActiveKernel::send`] (never `dispatch`, which
/// subscribes the client inbox to the target and would echo the plumbing into the
/// chat — see the invisibility discipline in [`crate::onboard`]).
///
/// `driver` is the agent whose loop is being stopped (a mind), or `""` for a bare
/// `@ai` turn, where the blocked caller is the TUI itself.
pub(crate) fn request_cancel(app: &mut App, driver: String) {
    if let Some(panel) = app.plan.as_mut() {
        if panel.source == driver {
            panel.cancelling();
        }
    }
    let active = app.active.clone();
    let tx = app.onboard_tx.clone();
    tokio::spawn(async move {
        let ev = cancel_turn(&active, &driver, BRAIN_ID).await;
        let _ = tx.send(ev);
    });
}

/// One `ps` → `cancel` round-trip against a kernel. Split out of [`request_cancel`] so
/// the whole discovery can be driven in a test without a terminal.
pub(crate) async fn cancel_turn(
    active: &ActiveKernel,
    driver: &str,
    executor: &str,
) -> OnboardEvent {
    // `ps` — any agent serves the universal system verb; ask the kernel itself.
    let table = active
        .send(&AgentId::from("kernel"), json!({"type": "ps"}))
        .await;
    let rows: Vec<Value> = table
        .get("tasks")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let targets = cancel_targets(&rows, driver, executor);
    let mut killed: Vec<String> = Vec::new();
    for id in targets {
        let reply = active
            .send(
                &AgentId::from("kernel"),
                json!({"type": "cancel", "task_id": id}),
            )
            .await;
        // The kernel reports what it ACTUALLY killed (the id + its subtree); a stale
        // id answers `{cancelled:false}` and adds nothing — cancel never pretends.
        if let Some(arr) = reply.get("killed").and_then(Value::as_array) {
            killed.extend(arr.iter().filter_map(Value::as_str).map(str::to_string));
        }
    }
    OnboardEvent::Cancelled(driver.to_string(), killed)
}

/// The `ps`→`cancel` result came back (from the `select!` loop): stamp the panel with
/// the outcome and say what was killed, in the chat of the agent whose loop it was
/// (the `@ai` chat for a bare AI turn). The mind itself, whose blocked `send` now
/// resolves `{"cancelled":true}`, says `(cancelled)` on its own — this note is the
/// SHELL's half: which kernel tasks died.
pub(crate) fn on_cancelled(app: &mut App, driver: String, killed: Vec<String>) {
    if let Some(panel) = app.plan.as_mut() {
        if panel.source == driver {
            panel.cancelled(killed.clone());
        }
    }
    let chat = if driver.is_empty() {
        "ai"
    } else {
        driver.as_str()
    };
    let note = if killed.is_empty() {
        "⊘ nothing in flight to cancel".to_string()
    } else {
        format!("⊘ cancelled {}", killed.join(" · "))
    };
    app.tabs
        .deliver(chat)
        .push("system", "you", Body::Note(note), State::Done);
}

/// Route one live event into the plan panel (called from `on_brain_event`). Returns
/// `true` when the panel consumed it — the caller then does NOT render it into the
/// transcript. Anything it refuses (a malformed `plan`, a `step` with no panel) falls
/// through to the normal dim note: an event we can't render is still SHOWN.
///
/// - `plan` opens the panel (replacing whatever was there — a new turn, a new plan).
/// - `step` advances it.
/// - a `say` from that source SETTLES it, but only once every step is done: that is
///   what tells the mind's FINAL answer apart from chatter mid-loop (a settled panel
///   no longer takes the cancel key, and a running loop must never lose it).
/// - `done` from that source CLOSES it — unless it was cancelled, in which case the
///   panel stays as the record of the kill until Esc dismisses it or the next plan
///   replaces it. (`say`/`done` are never consumed: the answer belongs in the
///   transcript too.)
pub(crate) fn on_event(app: &mut App, ev: &Value) -> bool {
    let ty = ev.get("type").and_then(Value::as_str).unwrap_or("");
    match ty {
        "plan" => match PlanPanel::from_event(ev) {
            Some(panel) => {
                app.plan = Some(panel);
                true
            }
            None => false,
        },
        "step" => app.plan.as_mut().is_some_and(|p| p.on_step(ev)),
        "say" | "done" => {
            let source = ev.get("source").and_then(Value::as_str).unwrap_or("");
            let Some(p) = app.plan.as_mut().filter(|p| p.source == source) else {
                return false;
            };
            match ty {
                "done" if p.state != PanelState::Cancelled => app.plan = None,
                "say" if p.is_complete() => p.settle(),
                _ => {}
            }
            false
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn plan_ev() -> Value {
        json!({
            "type":"plan",
            "steps":["identify_requirements","evaluate_options","provide_recommendation"],
            "source":"mind_abc123","client_id":"fantastic"
        })
    }

    #[test]
    fn from_event_opens_a_panel_with_pending_steps() {
        let p = PlanPanel::from_event(&plan_ev()).expect("a plan opens a panel");
        assert_eq!(p.source, "mind_abc123");
        assert_eq!(p.steps.len(), 3);
        assert_eq!(p.steps[0].text, "identify_requirements");
        assert!(p.steps.iter().all(|s| s.status == Status::Pending));
        assert_eq!(p.state, PanelState::Live);
        assert!(p.is_live());
    }

    #[test]
    fn from_event_takes_object_steps_and_trims() {
        let p = PlanPanel::from_event(&json!({
            "type":"plan","source":"m",
            "steps":[{"text":" first "},{"step":"second"},{"nope":1},"  third  "]
        }))
        .expect("object steps are duck-typed too");
        assert_eq!(
            p.steps.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(),
            ["first", "second", "third"]
        );
    }

    #[test]
    fn from_event_none_on_wrong_type_no_source_or_no_steps() {
        // Not a plan.
        assert!(PlanPanel::from_event(&json!({"type":"say","source":"m"})).is_none());
        // No source → nothing to cancel, nothing to settle it.
        assert!(PlanPanel::from_event(&json!({"type":"plan","steps":["a"]})).is_none());
        assert!(PlanPanel::from_event(&json!({"type":"plan","steps":["a"],"source":""})).is_none());
        // A plan with no steps is not a plan (it must fall through to a dim note).
        assert!(PlanPanel::from_event(&json!({"type":"plan","steps":[],"source":"m"})).is_none());
        assert!(PlanPanel::from_event(&json!({"type":"plan","source":"m"})).is_none());
    }

    #[test]
    fn on_step_advances_and_marks_earlier_steps_done() {
        let mut p = PlanPanel::from_event(&plan_ev()).unwrap();
        assert!(p.on_step(&json!({
            "type":"step","index":0,"total":3,"text":"identify_requirements",
            "status":"running","source":"mind_abc123"
        })));
        assert_eq!(p.steps[0].status, Status::Running);
        assert_eq!(p.steps[1].status, Status::Pending);

        assert!(p.on_step(&json!({
            "type":"step","index":0,"total":3,"status":"done","source":"mind_abc123"
        })));
        assert_eq!(p.steps[0].status, Status::Done);

        // A step jumping ahead still marks everything before it done (a dropped event
        // can never leave the panel stuck behind the mind).
        assert!(p.on_step(&json!({
            "type":"step","index":2,"total":3,"status":"running","source":"mind_abc123"
        })));
        assert_eq!(p.steps[1].status, Status::Done);
        assert_eq!(p.steps[2].status, Status::Running);
    }

    #[test]
    fn on_step_refuses_foreign_out_of_range_and_non_steps() {
        let mut p = PlanPanel::from_event(&plan_ev()).unwrap();
        // Another agent's step.
        assert!(!p.on_step(&json!({"type":"step","index":0,"source":"other"})));
        // Out of range.
        assert!(!p.on_step(&json!({"type":"step","index":9,"source":"mind_abc123"})));
        // Not a step / no index.
        assert!(!p.on_step(&json!({"type":"say","index":0,"source":"mind_abc123"})));
        assert!(!p.on_step(&json!({"type":"step","source":"mind_abc123"})));
        assert!(p.steps.iter().all(|s| s.status == Status::Pending));
    }

    #[test]
    fn on_step_text_names_a_step_the_mind_renamed() {
        let mut p = PlanPanel::from_event(&plan_ev()).unwrap();
        p.on_step(&json!({
            "type":"step","index":1,"text":"weigh Postgres vs SQLite","status":"running",
            "source":"mind_abc123"
        }));
        assert_eq!(p.steps[1].text, "weigh Postgres vs SQLite");
    }

    #[test]
    fn settle_finishes_every_step() {
        let mut p = PlanPanel::from_event(&plan_ev()).unwrap();
        p.on_step(&json!({"type":"step","index":0,"status":"running","source":"mind_abc123"}));
        p.settle();
        assert!(p.steps.iter().all(|s| s.status == Status::Done));
        assert_eq!(p.state, PanelState::Settled);
        assert!(!p.is_live());
    }

    #[test]
    fn a_cancelled_panel_never_repaints_as_a_success() {
        let mut p = PlanPanel::from_event(&plan_ev()).unwrap();
        p.cancelling();
        assert!(
            p.is_live(),
            "the cancel key still means something in flight"
        );
        p.cancelled(vec!["brain:3".into()]);
        assert_eq!(p.state, PanelState::Cancelled);
        assert_eq!(p.killed, ["brain:3"]);
        // The mind's own `(cancelled)` say arrives next — it must not settle the panel
        // as if the plan had completed.
        p.settle();
        assert_eq!(p.state, PanelState::Cancelled);
        assert!(p.footer().contains("killed brain:3"));
    }

    #[test]
    fn a_cancel_that_hit_nothing_says_so() {
        let mut p = PlanPanel::from_event(&plan_ev()).unwrap();
        p.cancelled(Vec::new());
        assert!(p.footer().contains("nothing was in flight"));
    }

    // ── cancel_targets ──────────────────────────────────────────────────────

    fn row(id: &str, agent: &str, parent: Option<&str>) -> Value {
        json!({"id":id,"agent":agent,"parent":parent,"verb":"send","started_at":1.0})
    }

    fn script_row(id: &str, agent: &str) -> Value {
        json!({"id":id,"agent":agent,"parent":null,"verb":"script","started_at":1.0})
    }

    /// ① The REAL table a live driver agent produces (rows pasted from a running
    /// kernel): the driver is a TASK of its own, and the brain call it is blocked in is
    /// that row's CHILD. Esc kills the CHILD — the driver's `send` resolves
    /// `{"cancelled":true}`, and its own row survives to take the next turn.
    #[test]
    fn a_mind_cancels_the_brain_call_under_its_script_row() {
        let rows = vec![
            script_row("mind_abc123:2", "mind_abc123"), // the mind itself — protected
            row("brain:3", "brain", Some("mind_abc123:2")),
            row("ollama:1", "ollama", Some("brain:3")),
            row("core:4", "core", None), // our own `ps` — never a target
        ];
        let t = cancel_targets(&rows, "mind_abc123", "brain");
        assert_eq!(
            t,
            ["brain:3"],
            "the mind's child, subtree-killed (never the mind's own script row)"
        );
        assert!(!t.contains(&"mind_abc123:2".to_string()));
    }

    /// Same rule, whatever the mind is blocked in: a driver with a row of its own is
    /// cancelled by killing its children — never its own row, which is the job itself.
    #[test]
    fn a_drivers_own_job_row_is_never_killed_only_its_children() {
        let rows = vec![
            script_row("mind_x:1", "mind_x"), // the mind's job — protected
            row("brain:7", "brain", Some("mind_x:1")),
            row("ollama:2", "ollama", Some("brain:7")),
        ];
        let t = cancel_targets(&rows, "mind_x", "brain");
        assert_eq!(
            t,
            ["brain:7"],
            "the child of the mind's job, subtree-killed"
        );
        assert!(!t.contains(&"mind_x:1".to_string()));
    }

    /// ② The ONLY case ② serves: a bare `@ai` turn. The blocked caller is the TUI —
    /// not an agent, so it holds no row and there are no children to look under. The
    /// brain's own root dispatch is the target (that is what Esc interrupts).
    #[test]
    fn a_bare_ai_turn_cancels_the_brains_root_dispatch() {
        let rows = vec![
            row("brain:1", "brain", None),
            row("ollama:1", "ollama", Some("brain:1")),
        ];
        assert_eq!(cancel_targets(&rows, "", "brain"), ["brain:1"]);
    }

    /// A mind with nothing in flight kills NOTHING — it must never reach across chats to
    /// a brain dispatch that isn't its own. (Before the task table told the truth about
    /// scripts, a mind with no rows fell through to "the brain's root-most dispatch",
    /// which is somebody ELSE's turn: the `@ai` chat's, or another mind's.)
    #[test]
    fn a_mind_with_nothing_in_flight_never_reaches_another_chats_brain_call() {
        let rows = vec![
            script_row("mind_x:1", "mind_x"), // idle: no children
            row("brain:9", "brain", None),    // the @ai chat's turn — NOT ours
            row("ollama:3", "ollama", Some("brain:9")),
        ];
        assert!(
            cancel_targets(&rows, "mind_x", "brain").is_empty(),
            "an idle mind's Esc must not kill the @ai chat's brain call"
        );
    }

    #[test]
    fn nothing_in_flight_yields_no_targets() {
        let rows = vec![row("core:9", "core", None)];
        assert!(cancel_targets(&rows, "mind_x", "brain").is_empty());
        assert!(cancel_targets(&[], "", "brain").is_empty());
    }
}
