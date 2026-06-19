//! The unified chat transcript + `@`-routing for the TUI's single `Chat` mode.
//!
//! One transcript collapses the old AI and Kernel-manager modes: every line is
//! a [`Msg`] from one agent to another. AI turns stream into a live message
//! (`token` events append; `done` seals it); kernel commands push a prompt and
//! a reply. `@target` retargets a line; with no `@` the line goes to the sticky
//! target. Per-source colored rails ([`color_for`]) keep agents visually
//! distinct. The model is `serde`-ready for later persistence (in-mem for now).

use std::collections::{HashMap, VecDeque};

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

    /// Route a backend event (`token`/`say`/`status`/`done`) into the
    /// transcript. The event's `source` (falling back to `from`) names the
    /// agent; events with no live message for that source are tolerated
    /// (a `token` lazily opens one).
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
    Empty,
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
}
