//! The unified chat transcript + `@`-routing for the TUI's single `Chat` mode.
//!
//! One transcript collapses the old AI and Kernel-manager modes: every line is
//! a [`Msg`] from one agent to another. AI turns stream into a live message
//! (`token` events append; `done` seals it); kernel commands push a prompt and
//! a reply. `@target` retargets a line; with no `@` the line goes to the sticky
//! target. Per-source colored rails ([`color_for`]) keep agents visually
//! distinct. The model is `serde`-ready for later persistence (in-mem for now).

use std::collections::{HashMap, HashSet, VecDeque};

use fantastic_host::gateway::Runtime;
use fantastic_host::parse_kv;
use fantastic_kernel::AgentId;
use ratatui::style::Color;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// Soft cap on retained transcript messages (oldest evicted past this).
const MSG_CAP: usize = 2000;

#[derive(Clone, Serialize, Deserialize)]
pub struct Msg {
    pub seq: u64,
    pub from: String,
    pub to: String,
    pub body: Body,
    pub state: State,
}

#[derive(Clone, Serialize, Deserialize)]
pub enum Body {
    Text(String),
    Tool {
        verb: String,
        target: String,
        summary: String,
    },
    Note(String),
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum State {
    Streaming,
    Done,
    Interrupted,
    Error,
}

#[derive(Default)]
pub struct Transcript {
    msgs: VecDeque<Msg>,
    next_seq: u64,
    /// source-agent-id → seq of its in-flight `Streaming` message.
    live: HashMap<String, u64>,
}

impl Transcript {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read-only view of the retained messages, oldest first.
    pub fn msgs(&self) -> &VecDeque<Msg> {
        &self.msgs
    }

    /// True while any source has an in-flight streaming message.
    pub fn has_live(&self) -> bool {
        !self.live.is_empty()
    }

    /// Push a message; assigns the next seq, caps the buffer. Returns the seq.
    pub fn push(&mut self, from: &str, to: &str, body: Body, state: State) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.msgs.push_back(Msg {
            seq,
            from: from.to_string(),
            to: to.to_string(),
            body,
            state,
        });
        while self.msgs.len() > MSG_CAP {
            // Evicting a still-live message would orphan its `live` entry; the
            // cap is large enough that this only ever drops long-settled lines.
            if let Some(m) = self.msgs.pop_front() {
                if m.state == State::Streaming {
                    self.live.remove(&m.from);
                }
            }
        }
        seq
    }

    fn get_mut(&mut self, seq: u64) -> Option<&mut Msg> {
        self.msgs.iter_mut().find(|m| m.seq == seq)
    }

    /// Append text to a `Body::Text` message (no-op if it is another body).
    pub fn append_text(&mut self, seq: u64, s: &str) {
        if let Some(m) = self.get_mut(seq) {
            if let Body::Text(t) = &mut m.body {
                t.push_str(s);
            }
        }
    }

    pub fn set_state(&mut self, seq: u64, state: State) {
        if let Some(m) = self.get_mut(seq) {
            m.state = state;
        }
    }

    /// Begin a streaming message from `from` to `to`: an empty `Streaming` text
    /// line recorded as that source's live message. Returns the seq.
    pub fn start_stream(&mut self, from: &str, to: &str) -> u64 {
        let seq = self.push(from, to, Body::Text(String::new()), State::Streaming);
        self.live.insert(from.to_string(), seq);
        seq
    }

    /// Mark every live message interrupted and clear the live set. Returns the
    /// seqs that were interrupted (for any caller-side bookkeeping).
    pub fn interrupt_live(&mut self) -> Vec<u64> {
        let seqs: Vec<u64> = self.live.values().copied().collect();
        for &seq in &seqs {
            self.set_state(seq, State::Interrupted);
        }
        self.live.clear();
        seqs
    }

    /// Close a live streaming message for `source` at turn's end: if it never
    /// received any streamed text (a backend that doesn't route tokens to us, e.g.
    /// ollama), fill it with `fallback` (the final response); then seal it Done. A
    /// no-op when there's no live stream (a streamed `done` already sealed it) — so
    /// streaming backends don't double-render.
    pub fn close_stream(&mut self, source: &str, fallback: &str) {
        if let Some(&seq) = self.live.get(source) {
            let empty = self
                .msgs
                .iter()
                .find(|m| m.seq == seq)
                .map(|m| matches!(&m.body, Body::Text(t) if t.is_empty()))
                .unwrap_or(false);
            if empty {
                self.append_text(seq, fallback);
            }
            self.set_state(seq, State::Done);
            self.live.remove(source);
        }
    }

    /// Route a backend event (`token`/`say`/`status`/`done`) into the
    /// transcript — the **live-token renderer**. `token` appends to the source's
    /// live message, `say`/`status` push notes/tool lines, `done` seals it. The
    /// event's `source` (falling back to `from`) names the agent; events with no
    /// live message for that source are tolerated (a `token` lazily opens one).
    /// ai-core emits a turn's events IN ORDER (`queued → token… → done`), so the
    /// `done` seal always follows that turn's last token — no split.
    pub fn on_event(&mut self, ev: &Value) {
        let ty = ev.get("type").and_then(Value::as_str).unwrap_or("");
        let source = ev
            .get("source")
            .and_then(Value::as_str)
            .or_else(|| ev.get("from").and_then(Value::as_str))
            .unwrap_or("brain")
            .to_string();
        let to = ev
            .get("client_id")
            .and_then(Value::as_str)
            .map(|c| if c == "fantastic" { "you" } else { c })
            .unwrap_or("you")
            .to_string();
        match ty {
            "token" => {
                let text = ev.get("text").and_then(Value::as_str).unwrap_or("");
                let seq = match self.live.get(&source) {
                    Some(&s) => s,
                    None => self.start_stream(&source, &to),
                };
                self.append_text(seq, text);
            }
            "say" => {
                // Tool-call summary line, e.g. "[tool core → {...}]".
                let text = ev
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                self.push(&source, &to, Body::Note(text), State::Done);
            }
            "status" => {
                // Surface a tool dispatch as a dim Tool line; other phases are
                // transient and intentionally not retained.
                if let Some(tool) = ev.get("detail").and_then(|d| d.get("tool")) {
                    let verb = tool
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("tool")
                        .to_string();
                    let target = tool
                        .get("args")
                        .and_then(|a| a.get("target_id"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    self.push(
                        &source,
                        &to,
                        Body::Tool {
                            verb,
                            target,
                            summary: String::new(),
                        },
                        State::Done,
                    );
                }
            }
            "done" => {
                if let Some(seq) = self.live.remove(&source) {
                    self.set_state(seq, State::Done);
                }
            }
            _ => {}
        }
    }
}

/// One chat **per character**: each addressee (`@ai`, `@sh`, `@ws`, or any agent
/// you address) owns its own [`Transcript`], and you tab between them. Addressing
/// someone *enters* their room (focus); Shift-Tab turns to face the next one. A
/// tab that receives a message while you're elsewhere is marked **unread**.
pub struct Tabs {
    order: Vec<String>,
    chats: HashMap<String, Transcript>,
    active: usize,
    unread: HashSet<String>,
}

impl Tabs {
    /// Seed the base "characters" (kept in this order; the first is active).
    pub fn new(base: &[&str]) -> Self {
        let order: Vec<String> = base.iter().map(|s| s.to_string()).collect();
        let chats = order
            .iter()
            .map(|id| (id.clone(), Transcript::new()))
            .collect();
        Tabs {
            order,
            chats,
            active: 0,
            unread: HashSet::new(),
        }
    }

    /// Tab ids in display/cycle order.
    pub fn ids(&self) -> &[String] {
        &self.order
    }

    /// The id of the character you're currently facing.
    pub fn active_id(&self) -> &str {
        &self.order[self.active]
    }

    pub fn active_index(&self) -> usize {
        self.active
    }

    /// True if `id` got a message while it wasn't the active tab.
    pub fn is_unread(&self, id: &str) -> bool {
        self.unread.contains(id)
    }

    /// The active character's transcript.
    pub fn active(&self) -> &Transcript {
        &self.chats[&self.order[self.active]]
    }

    pub fn active_mut(&mut self) -> &mut Transcript {
        let id = self.order[self.active].clone();
        self.chats.get_mut(&id).expect("active tab exists")
    }

    /// Borrow a tab's transcript (read-only); `None` if no such tab yet.
    pub fn chat(&self, id: &str) -> Option<&Transcript> {
        self.chats.get(id)
    }

    /// Create `id`'s tab if it doesn't exist yet (appended after the base tabs).
    fn ensure(&mut self, id: &str) {
        if !self.chats.contains_key(id) {
            self.order.push(id.to_string());
            self.chats.insert(id.to_string(), Transcript::new());
        }
    }

    /// **Enter** `id`'s room: create its tab if new, make it active, clear its
    /// unread mark. This is what addressing a character does.
    pub fn focus(&mut self, id: &str) {
        self.ensure(id);
        self.active = self.order.iter().position(|t| t == id).unwrap();
        self.unread.remove(id);
    }

    /// Deliver a message into `id`'s transcript (creating its tab if new). If `id`
    /// isn't the tab you're facing, it's marked **unread**. Returns the transcript
    /// so the caller can `push`/`on_event` into it.
    pub fn deliver(&mut self, id: &str) -> &mut Transcript {
        self.ensure(id);
        if self.order[self.active] != id {
            self.unread.insert(id.to_string());
        }
        self.chats.get_mut(id).expect("ensured")
    }

    /// Turn to the next/previous character (wraps). Clears the arrived-at tab's
    /// unread mark. Shift-Tab drives this.
    pub fn cycle(&mut self, forward: bool) {
        let n = self.order.len();
        if n == 0 {
            return;
        }
        self.active = if forward {
            (self.active + 1) % n
        } else {
            (self.active + n - 1) % n
        };
        let id = self.order[self.active].clone();
        self.unread.remove(&id);
    }
}

/// The smart input line: an editable **`@<sender>`** field + the message body.
/// You edit/delete the sender (not just the message), Tab-complete it against the
/// known characters, Shift-Tab roll through them; a send is a **nogo** if the
/// sender isn't known. Pure + fully unit-tested (no terminal needed).
pub struct Composer {
    /// The current addressee (no leading `@`).
    pub sender: String,
    /// The message body being typed.
    pub message: String,
    /// True while the cursor is in the `@sender` field (vs the message body).
    pub editing_sender: bool,
    /// Set when a send was rejected because the sender is unknown (for a flash).
    pub nogo: bool,
}

impl Composer {
    pub fn new(sender: &str) -> Self {
        Composer {
            sender: sender.to_string(),
            message: String::new(),
            editing_sender: false,
            nogo: false,
        }
    }

    /// Type a printable char into the focused field. `@` at the start of an empty
    /// message jumps to (re)editing the sender; a space commits the sender back to
    /// the message body.
    pub fn type_char(&mut self, c: char) {
        self.nogo = false;
        if !self.editing_sender && self.message.is_empty() && c == '@' {
            // Retarget from scratch: jump into the sender field and clear it.
            self.editing_sender = true;
            self.sender.clear();
            return;
        }
        if self.editing_sender {
            if c == ' ' {
                self.editing_sender = false;
            } else {
                self.sender.push(c);
            }
        } else {
            self.message.push(c);
        }
    }

    /// Backspace: delete from the message; once it's empty, step into and trim the
    /// `@sender` field — so you can edit the addressee, not just the text.
    pub fn backspace(&mut self) {
        self.nogo = false;
        if self.editing_sender {
            self.sender.pop();
        } else if self.message.pop().is_none() {
            self.editing_sender = true;
            self.sender.pop();
        }
    }

    /// Tab-complete the `@sender` against `known` (only while editing it).
    /// Completes to the first known id that extends the current fragment; if the
    /// fragment already names a known character, commit to the message. Returns
    /// true if anything changed.
    pub fn complete(&mut self, known: &[String]) -> bool {
        if !self.editing_sender {
            return false;
        }
        if let Some(hit) = known
            .iter()
            .find(|k| k.len() > self.sender.len() && k.starts_with(self.sender.as_str()))
        {
            self.sender = hit.clone();
            return true;
        }
        if known.contains(&self.sender) {
            self.editing_sender = false;
            return true;
        }
        false
    }

    /// Force the sender (e.g. after a tab switch) and leave the message untouched.
    pub fn set_sender(&mut self, sender: &str) {
        self.sender = sender.to_string();
        self.editing_sender = false;
        self.nogo = false;
    }

    /// Is the current sender a known character? A send is a **nogo** otherwise.
    pub fn is_valid(&self, known: &[String]) -> bool {
        known.contains(&self.sender)
    }

    /// Take the message (clearing it). The sender stays (sticky addressee).
    pub fn take_message(&mut self) -> String {
        std::mem::take(&mut self.message)
    }
}

/// A stable color for an agent's rail. `"you"` is forced White; everything else
/// is an fnv1a hash of the id into a fixed palette.
pub fn color_for(id: &str) -> Color {
    if id == "you" {
        return Color::White;
    }
    const PALETTE: [Color; 6] = [
        Color::Cyan,
        Color::Magenta,
        Color::Green,
        Color::Yellow,
        Color::Blue,
        Color::LightRed,
    ];
    let mut hash: u32 = 0x811c_9dc5;
    for b in id.bytes() {
        hash ^= b as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    PALETTE[(hash as usize) % PALETTE.len()]
}

/// The destination + intent resolved from one input line.
pub enum Route {
    Ai(String),
    Kernel(AgentId, Value),
    Reflect(AgentId),
    /// Run a command in the live PTY and breathe its screen into the chat.
    Shell(String),
    /// Drive the out-of-process workspace kernel over the gateway.
    Workspace(WsCmd),
    Empty,
}

/// A command against the sovereign workspace kernel (the `@ws` target). The
/// gateway lifecycle (`Up`/`Down`) plus a verb sent to the workspace ROOT.
pub enum WsCmd {
    /// Attach-or-spawn a workspace kernel in cwd with this runtime.
    Up(Runtime),
    /// Gracefully shut the workspace kernel down.
    Down,
    /// Send a verb payload to a workspace agent (root `kernel` for ROOT verbs).
    Verb(AgentId, Value),
}

/// Parse `@ws up [rust|python|swift]` body into the runtime (default Rust). The
/// body still carries the leading `up` verb, so the runtime is its 2nd token.
fn ws_runtime(body: &str) -> Runtime {
    match body.split_whitespace().nth(1).unwrap_or("") {
        "python" => Runtime::Python,
        "swift" => Runtime::Swift,
        _ => Runtime::Rust,
    }
}

/// Build a workspace verb payload `{"type":verb, ...k=v}` for the ROOT.
fn ws_verb(body: &str) -> Route {
    let mut toks = body.split_whitespace();
    let verb = toks.next().unwrap_or("reflect");
    let mut p = Map::new();
    p.insert("type".into(), json!(verb));
    for kv in toks {
        if let Some((k, v)) = kv.split_once('=') {
            p.insert(k.to_string(), parse_kv(v));
        }
    }
    Route::Workspace(WsCmd::Verb(AgentId::from("kernel"), Value::Object(p)))
}

/// Resolve a chat input line against the sticky target. Returns the new sticky
/// target (the resolved destination) + the route. `@target …` retargets;
/// otherwise the sticky target receives the whole line. `ai`/`brain` go to the
/// AI; any other id is a kernel agent (empty body → reflect, else a sugar
/// `<verb> [k=v…]` command).
pub fn route(line: &str, sticky: &str) -> (String, Route) {
    let line = line.trim();
    if line.is_empty() {
        return (sticky.to_string(), Route::Empty);
    }
    let (target, body) = if let Some(rest) = line.strip_prefix('@') {
        let mut it = rest.splitn(2, char::is_whitespace);
        let t = it.next().unwrap_or("").to_string();
        let b = it.next().unwrap_or("").trim().to_string();
        (t, b)
    } else {
        (sticky.to_string(), line.to_string())
    };

    if target.is_empty() {
        return (sticky.to_string(), Route::Empty);
    }

    if target == "ai" || target == "brain" {
        let route = if body.is_empty() {
            Route::Empty
        } else {
            Route::Ai(body)
        };
        return (target, route);
    }

    if target == "sh" || target == "shell" {
        // Normalize the sticky target to `sh` so bare follow-up lines keep
        // running in the live terminal.
        let route = if body.is_empty() {
            Route::Empty
        } else {
            Route::Shell(body)
        };
        return ("sh".to_string(), route);
    }

    if target == "ws" {
        // `@ws up [rt]` / `@ws down` are lifecycle; everything else is a verb to
        // the workspace ROOT; bare `@ws` shows its tree.
        let verb = body.split_whitespace().next().unwrap_or("");
        let route = match verb {
            "up" => Route::Workspace(WsCmd::Up(ws_runtime(&body))),
            "down" => Route::Workspace(WsCmd::Down),
            "" => Route::Workspace(WsCmd::Verb(
                AgentId::from("kernel"),
                json!({"type":"reflect","tree":"ids"}),
            )),
            _ => ws_verb(&body),
        };
        return ("ws".to_string(), route);
    }

    let id = AgentId::from(target.as_str());
    let route = if body.is_empty() {
        Route::Reflect(id)
    } else {
        let mut toks = body.split_whitespace();
        let verb = toks.next().unwrap_or("reflect");
        let mut p = Map::new();
        p.insert("type".into(), json!(verb));
        for kv in toks {
            if let Some((k, v)) = kv.split_once('=') {
                p.insert(k.to_string(), parse_kv(v));
            }
        }
        Route::Kernel(id, Value::Object(p))
    };
    (target, route)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_concatenates_into_one_done_msg() {
        let mut t = Transcript::new();
        let seq = t.start_stream("brain", "you");
        t.on_event(&json!({"type":"token","text":"hel","source":"brain"}));
        t.on_event(&json!({"type":"token","text":"lo","source":"brain"}));
        assert!(t.has_live());
        t.on_event(&json!({"type":"done","source":"brain"}));
        assert!(!t.has_live());
        // Exactly the one streaming message, now Done with the joined text.
        let live: Vec<&Msg> = t.msgs().iter().filter(|m| m.seq == seq).collect();
        assert_eq!(live.len(), 1);
        let m = live[0];
        assert!(m.state == State::Done);
        match &m.body {
            Body::Text(s) => assert_eq!(s, "hello"),
            _ => panic!("expected text body"),
        }
    }

    #[test]
    fn token_without_prior_stream_opens_one() {
        let mut t = Transcript::new();
        t.on_event(&json!({"type":"token","text":"hi","source":"brain"}));
        t.on_event(&json!({"type":"done","source":"brain"}));
        let m = t.msgs().back().unwrap();
        match &m.body {
            Body::Text(s) => assert_eq!(s, "hi"),
            _ => panic!("expected text body"),
        }
        assert!(m.state == State::Done);
    }

    #[test]
    fn interrupt_marks_live_message() {
        let mut t = Transcript::new();
        let seq = t.start_stream("brain", "you");
        t.append_text(seq, "partial");
        let hit = t.interrupt_live();
        assert_eq!(hit, vec![seq]);
        assert!(!t.has_live());
        assert!(t.msgs().back().unwrap().state == State::Interrupted);
    }

    #[test]
    fn color_for_is_stable_and_you_is_white() {
        assert_eq!(color_for("you"), Color::White);
        assert_eq!(color_for("brain"), color_for("brain"));
        // Different ids land in the fixed palette.
        for id in ["brain", "core", "web", "kernel"] {
            assert_ne!(color_for(id), Color::White);
        }
    }

    #[test]
    fn on_event_streams_tokens_records_say_and_seals_done() {
        let mut t = Transcript::new();
        let seq = t.start_stream("brain", "you");
        t.on_event(&json!({"type":"token","text":"He"}));
        t.on_event(&json!({"type":"token","text":"llo"}));
        t.on_event(&json!({"type":"say","text":"[tool]"}));
        t.on_event(&json!({"type":"done"}));

        // The live message is sealed Done with the concatenated tokens.
        let live = t.msgs().iter().find(|m| m.seq == seq).expect("live msg");
        assert!(live.state == State::Done, "live msg should end Done");
        match &live.body {
            Body::Text(s) => assert_eq!(s, "Hello"),
            _ => panic!("expected a text body for the streamed msg"),
        }
        assert!(!t.has_live(), "done clears the live set");

        // The `say` tool line was recorded as its own Note message.
        let say = t
            .msgs()
            .iter()
            .find(|m| matches!(&m.body, Body::Note(n) if n == "[tool]"))
            .expect("a say/tool line should be recorded");
        assert!(say.state == State::Done);
        assert_eq!(say.from, "brain");
    }

    #[test]
    fn interrupt_live_flips_state_to_interrupted() {
        let mut t = Transcript::new();
        let seq = t.start_stream("brain", "you");
        t.on_event(&json!({"type":"token","text":"partial"}));
        let hit = t.interrupt_live();
        assert_eq!(hit, vec![seq]);
        let m = t.msgs().iter().find(|m| m.seq == seq).expect("msg");
        assert!(
            m.state == State::Interrupted,
            "live msg flips to Interrupted"
        );
        assert!(!t.has_live());
    }

    #[test]
    fn color_for_you_is_white_and_hash_is_deterministic() {
        assert_eq!(color_for("you"), Color::White);
        // Same id → same color on repeated calls (determinism).
        assert_eq!(color_for("brain"), color_for("brain"));
        assert_eq!(color_for("core"), color_for("core"));
        // Two unequal ids: non-`you` ids land in the colored palette (never the
        // forced White of `you`), and the hash spreads them across it.
        assert_ne!(color_for("brain"), Color::White);
        assert_ne!(color_for("web"), Color::White);
    }

    #[test]
    fn route_at_ai_goes_to_ai_and_sets_sticky() {
        let (sticky, r) = route("@ai hello", "core");
        assert_eq!(sticky, "ai");
        match r {
            Route::Ai(b) => assert_eq!(b, "hello"),
            _ => panic!("expected Ai"),
        }
    }

    #[test]
    fn route_bare_line_uses_sticky_ai() {
        let (sticky, r) = route("hello there", "ai");
        assert_eq!(sticky, "ai");
        match r {
            Route::Ai(b) => assert_eq!(b, "hello there"),
            _ => panic!("expected Ai"),
        }
    }

    #[test]
    fn route_kernel_verb_with_kv() {
        let (sticky, r) = route("@kernel list_agents", "ai");
        assert_eq!(sticky, "kernel");
        match r {
            Route::Kernel(id, payload) => {
                assert_eq!(id.as_str(), "kernel");
                assert_eq!(payload["type"], "list_agents");
            }
            _ => panic!("expected Kernel"),
        }
    }

    #[test]
    fn route_bare_at_id_is_reflect() {
        let (sticky, r) = route("@core", "ai");
        assert_eq!(sticky, "core");
        match r {
            Route::Reflect(id) => assert_eq!(id.as_str(), "core"),
            _ => panic!("expected Reflect"),
        }
    }

    #[test]
    fn route_empty_keeps_sticky() {
        let (sticky, r) = route("   ", "web");
        assert_eq!(sticky, "web");
        assert!(matches!(r, Route::Empty));
    }

    #[test]
    fn route_at_sh_runs_shell_and_sticks() {
        let (sticky, r) = route("@sh make", "ai");
        assert_eq!(sticky, "sh");
        match r {
            Route::Shell(cmd) => assert_eq!(cmd, "make"),
            _ => panic!("expected Shell"),
        }
    }

    #[test]
    fn route_at_shell_alias_runs_shell() {
        let (sticky, r) = route("@shell htop", "ai");
        assert_eq!(sticky, "sh");
        match r {
            Route::Shell(cmd) => assert_eq!(cmd, "htop"),
            _ => panic!("expected Shell"),
        }
    }

    #[test]
    fn route_bare_line_when_sticky_sh_runs_shell() {
        let (sticky, r) = route("ls -la", "sh");
        assert_eq!(sticky, "sh");
        match r {
            Route::Shell(cmd) => assert_eq!(cmd, "ls -la"),
            _ => panic!("expected Shell"),
        }
    }

    #[test]
    fn route_at_sh_empty_is_empty_but_sticks() {
        let (sticky, r) = route("@sh", "ai");
        assert_eq!(sticky, "sh");
        assert!(matches!(r, Route::Empty));
    }

    #[test]
    fn route_at_ws_up_default_rust() {
        let (sticky, r) = route("@ws up", "ai");
        assert_eq!(sticky, "ws");
        match r {
            Route::Workspace(WsCmd::Up(rt)) => assert_eq!(rt, Runtime::Rust),
            _ => panic!("expected Workspace(Up)"),
        }
    }

    #[test]
    fn route_at_ws_up_python() {
        let (_s, r) = route("@ws up python", "ai");
        match r {
            Route::Workspace(WsCmd::Up(rt)) => assert_eq!(rt, Runtime::Python),
            _ => panic!("expected Workspace(Up python)"),
        }
    }

    #[test]
    fn route_at_ws_down() {
        let (sticky, r) = route("@ws down", "ai");
        assert_eq!(sticky, "ws");
        assert!(matches!(r, Route::Workspace(WsCmd::Down)));
    }

    #[test]
    fn route_at_ws_verb_targets_kernel_root() {
        let (sticky, r) = route("@ws list_agents", "ai");
        assert_eq!(sticky, "ws");
        match r {
            Route::Workspace(WsCmd::Verb(id, payload)) => {
                assert_eq!(id.as_str(), "kernel");
                assert_eq!(payload["type"], "list_agents");
            }
            _ => panic!("expected Workspace(Verb)"),
        }
    }

    #[test]
    fn route_at_ws_verb_with_kv() {
        let (_s, r) = route("@ws create_agent handler_module=web.tools port=8080", "ai");
        match r {
            Route::Workspace(WsCmd::Verb(id, payload)) => {
                assert_eq!(id.as_str(), "kernel");
                assert_eq!(payload["type"], "create_agent");
                assert_eq!(payload["handler_module"], "web.tools");
                assert_eq!(payload["port"], 8080);
            }
            _ => panic!("expected Workspace(Verb) with kv"),
        }
    }

    #[test]
    fn route_bare_at_ws_reflects_tree() {
        let (sticky, r) = route("@ws", "ai");
        assert_eq!(sticky, "ws");
        match r {
            Route::Workspace(WsCmd::Verb(id, payload)) => {
                assert_eq!(id.as_str(), "kernel");
                assert_eq!(payload["type"], "reflect");
                assert_eq!(payload["tree"], "ids");
            }
            _ => panic!("expected Workspace(Verb reflect tree)"),
        }
    }

    #[test]
    fn route_bare_line_when_sticky_ws_is_verb() {
        let (sticky, r) = route("reflect", "ws");
        assert_eq!(sticky, "ws");
        match r {
            Route::Workspace(WsCmd::Verb(id, payload)) => {
                assert_eq!(id.as_str(), "kernel");
                assert_eq!(payload["type"], "reflect");
            }
            _ => panic!("expected Workspace(Verb) from sticky ws"),
        }
    }

    #[test]
    fn route_send_with_kv_coerces() {
        let (_s, r) = route("@web reflect depth=2", "ai");
        match r {
            Route::Kernel(_, payload) => {
                assert_eq!(payload["type"], "reflect");
                assert_eq!(payload["depth"], 2);
            }
            _ => panic!("expected Kernel"),
        }
    }

    // ── Tabs (per-character chats) ──────────────────────────────────────

    #[test]
    fn tabs_seed_base_in_order_first_active() {
        let t = Tabs::new(&["ai", "sh", "ws"]);
        assert_eq!(t.ids(), ["ai", "sh", "ws"]);
        assert_eq!(t.active_id(), "ai");
        assert_eq!(t.active_index(), 0);
    }

    #[test]
    fn tabs_focus_existing_and_create_new() {
        let mut t = Tabs::new(&["ai", "sh", "ws"]);
        t.focus("sh");
        assert_eq!(t.active_id(), "sh");
        // a brand-new character appends a tab and enters it.
        t.focus("web");
        assert_eq!(t.active_id(), "web");
        assert_eq!(t.ids(), ["ai", "sh", "ws", "web"]);
    }

    #[test]
    fn tabs_cycle_wraps_both_ways() {
        let mut t = Tabs::new(&["ai", "sh", "ws"]);
        t.cycle(true);
        assert_eq!(t.active_id(), "sh");
        t.cycle(true);
        t.cycle(true);
        assert_eq!(t.active_id(), "ai"); // wrapped
        t.cycle(false);
        assert_eq!(t.active_id(), "ws"); // wrapped backward
    }

    #[test]
    fn tabs_deliver_marks_unread_only_when_not_active() {
        let mut t = Tabs::new(&["ai", "sh", "ws"]);
        // facing ai; a message into sh marks sh unread.
        t.deliver("sh")
            .push("sh", "you", Body::Text("hi".into()), State::Done);
        assert!(t.is_unread("sh"));
        assert!(!t.is_unread("ai"));
        // entering sh clears its unread.
        t.focus("sh");
        assert!(!t.is_unread("sh"));
        // a message into the active tab is never unread.
        t.deliver("sh")
            .push("sh", "you", Body::Text("yo".into()), State::Done);
        assert!(!t.is_unread("sh"));
    }

    #[test]
    fn tabs_cycle_clears_arrived_unread() {
        let mut t = Tabs::new(&["ai", "sh", "ws"]);
        t.deliver("sh")
            .push("sh", "you", Body::Text("hi".into()), State::Done);
        assert!(t.is_unread("sh"));
        t.cycle(true); // turn to sh
        assert_eq!(t.active_id(), "sh");
        assert!(!t.is_unread("sh"));
    }

    // ── Composer (smart @sender + message) ──────────────────────────────

    fn known() -> Vec<String> {
        ["ai", "sh", "ws", "web"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn composer_types_into_message_by_default() {
        let mut c = Composer::new("ai");
        for ch in "hey".chars() {
            c.type_char(ch);
        }
        assert_eq!(c.sender, "ai");
        assert_eq!(c.message, "hey");
        assert!(!c.editing_sender);
    }

    #[test]
    fn composer_backspace_steps_from_message_into_sender() {
        let mut c = Composer::new("ai");
        c.type_char('h');
        c.backspace(); // clears message
        assert_eq!(c.message, "");
        c.backspace(); // empty message → edit sender, trims it
        assert!(c.editing_sender);
        assert_eq!(c.sender, "a");
        c.backspace();
        assert_eq!(c.sender, "");
    }

    #[test]
    fn composer_at_in_empty_message_edits_sender() {
        let mut c = Composer::new("ai");
        c.type_char('@'); // jump to editing sender; clears it for a fresh target
        assert!(c.editing_sender);
        assert_eq!(c.sender, "");
        // typing now builds a fresh sender; space commits to the message body.
        for ch in "web".chars() {
            c.type_char(ch);
        }
        assert_eq!(c.sender, "web");
        c.type_char(' ');
        assert!(!c.editing_sender);
    }

    #[test]
    fn composer_tab_completes_sender() {
        let mut c = Composer::new("");
        c.editing_sender = true;
        c.sender = "w".into();
        assert!(c.complete(&known())); // "w" → "ws" (first match)
        assert_eq!(c.sender, "ws");
        // an exact known name commits to the message instead.
        c.sender = "web".into();
        c.editing_sender = true;
        assert!(c.complete(&known()));
        assert!(!c.editing_sender);
    }

    #[test]
    fn composer_tab_complete_noop_when_no_match() {
        let mut c = Composer::new("");
        c.editing_sender = true;
        c.sender = "zzz".into();
        assert!(!c.complete(&known()));
        assert_eq!(c.sender, "zzz"); // unchanged
    }

    #[test]
    fn composer_validity_gates_send() {
        let mut c = Composer::new("ai");
        assert!(c.is_valid(&known()));
        c.set_sender("ghost");
        assert!(!c.is_valid(&known())); // nogo
    }

    #[test]
    fn composer_take_message_clears_but_keeps_sender() {
        let mut c = Composer::new("kernel");
        for ch in "list_agents".chars() {
            c.type_char(ch);
        }
        assert_eq!(c.take_message(), "list_agents");
        assert_eq!(c.message, "");
        assert_eq!(c.sender, "kernel"); // sticky addressee
    }
}
