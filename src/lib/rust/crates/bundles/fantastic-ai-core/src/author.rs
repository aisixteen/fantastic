//! author — the AI agent's SELF-REFLECT / SELF-CORRECT loop, as a verb.
//!
//! Merged INTO ai-core (2026-08-07, user decision) from the standalone
//! `fantastic-author` bundle: the verify-and-repair driver is not a separate
//! mind that borrows a brain — it is the AI agent driving ITSELF. Python
//! `ai_core/author.py` is canonical; on disagreement this module is wrong.

use fantastic_kernel::{AgentId, Item, Kernel};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

fn meta_str(agent_id: &AgentId, kernel: &Kernel, key: &str) -> Option<String> {
    let agent = kernel.agents.get(agent_id).map(|e| Arc::clone(&e))?;
    let meta = agent.meta.read().expect("meta poisoned");
    meta.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Turn budget for one `author` run — the live seed converges well inside it.
const STEPS: usize = 26;

/// Hard ceiling on a caller-supplied `steps` — the loop stays BOUNDED whoever
/// asks (a runaway mind burns a real brain).
const MAX_STEPS: usize = 64;

/// Default probe text: the turn the authored tool must ANSWER to be believed.
const DEFAULT_PROBE: &str = "hello world";

/// How long to wait for the authored tool to answer a probe before calling it
/// dead for this turn (polled, so a fast tool costs a poll, not the whole wait).
const PROBE_WAIT: Duration = Duration::from_millis(1500);
const PROBE_POLL: Duration = Duration::from_millis(50);

/// Grace granted ONCE the tool is seen calling its `must_call` dependency: a tool
/// that thinks with a real language model answers in model-time, not in kernel-time.
/// A tool that never calls anything never buys this extension — so an echo still
/// fails inside [`PROBE_WAIT`].
const PROBE_THINK_WAIT: Duration = Duration::from_secs(120);

/// The LEAN system prompt — VERBATIM from the live seed. It is tuned, and it
/// already carries the balanced-JSON contract that makes the envelope parseable
/// without downstream repair. Do not "improve" it without re-running the live
/// test: every clause in it is paying for itself on a weak model.
pub const LEAN_SYSTEM: &str = r#"You are an agent inside a LIVE agent kernel. Everything here is an agent addressed by a string id. There is ONE primitive: send(target_id, payload) — a JSON payload whose "type" field names the verb; the target replies with JSON.

You act by replying with EXACTLY ONE JSON object and NOTHING else, in one of these two forms:
  {"act": {"target": "<id>", "payload": { ...a verb payload... }}}
  {"final": "<short message>"}
After an {"act"} I send it for you and reply with the result as your next message.

YOUR REPLY MUST BE ONE COMPLETE, WELL-FORMED JSON OBJECT. It is parsed by a machine, not read by a human: nothing is repaired for you. Every "{" you open you MUST close with "}", and every "[" with "]" — an object that ends one brace short is DISCARDED and your act never happens. No prose, no markdown, no code fences, no trailing commas. When the payload carries source code, the code is ONE JSON string: escape every " as \" and every newline as \n, and close the string before you close the enclosing objects. Before you finish, re-read your reply from "{" to the final "}" and confirm it is balanced.

A create_agent act MUST name `parent_id` — the OWNER of the new agent. Name YOUR OWN id to grow your own subtree (that needs no grant); it is a real argument with no default, and omitting it refuses the act.

To LEARN anything, reflect it: {"act":{"target":"<id>","payload":{"type":"reflect"}}}. Reflecting "kernel" returns the system overview, your OWN capabilities, the agent tree, and a MAP of keyed docs (`readmes`) you can fetch one at a time. DISCOVER what you need by reflecting BEFORE you act — do not assume verb or API shapes.

You are ALONE and AUTONOMOUS — no human will reply between your turns. Never stop to ask, and never announce that you are "ready to proceed" or "understand now": that is NOT done. Reflecting and planning are not completing. Every reply must be a concrete {"act"} that moves the task forward. Only reply {"final":...} once the thing you were asked to build actually EXISTS and you have exercised it and seen it work. If you catch yourself about to declare readiness, instead emit the next {"act"} (a create_agent, or a send). Do NOT reflect the same target or doc twice — reading is not progress. The MOMENT a doc shows you how to build the tool (e.g. the self-extension doc's copy-pasteable example), your NEXT act MUST be a create_agent, not another reflect."#;

/// First complete JSON object from `s` (models fence/preface); tolerant of the
/// stray `\'` escape the genesis extractor also forgives.
///
/// NO REPAIR. A driver that patches the model's malformed JSON is measuring its
/// own patch, not the model: if the envelope does not parse, the act did not
/// happen. Validity is forced UPSTREAM (the backend's JSON-constrained decoding)
/// — never reconstructed downstream.
pub fn first_json(s: &str) -> Option<Value> {
    let start = s.find('{')?;
    let tail = &s[start..];
    let parse = |t: &str| -> Option<Value> {
        serde_json::Deserializer::from_str(t)
            .into_iter::<Value>()
            .next()?
            .ok()
    };
    parse(tail).or_else(|| parse(&tail.replace("\\'", "'")))
}

// ── the driver ──────────────────────────────────────────────────────

/// One `author` run. Everything the loop needs, so the body stays flat.
struct Run<'a> {
    kernel: &'a Arc<Kernel>,
    /// The author agent — progress events are emitted on THIS id.
    me: AgentId,
    /// The brain we relay turns to.
    brain: AgentId,
    /// The synthetic listener id we `watch` the authored tool with.
    ear: AgentId,
    /// Inbox of `ear` — every payload flowing through the authored tool.
    rx: tokio::sync::mpsc::Receiver<Value>,
    /// The dependency the authored tool must be OBSERVED calling, if demanded.
    must_call: Option<AgentId>,
    /// The synthetic listener id we `watch` [`Run::must_call`] with.
    call_ear: AgentId,
    /// Inbox of `call_ear` — every payload flowing through the dependency.
    call_rx: tokio::sync::mpsc::Receiver<Value>,
    /// Did the tool ANSWER since the probe window opened? (a `say` on its own id)
    saw_say: bool,
    /// Every `say` text heard in this probe window — the canary is looked for HERE,
    /// and the worst of them is what the pushback quotes back at the model.
    says: Vec<String>,
    /// Did the tool finish its turn (`done`)? Then the verdict is final NOW — no
    /// point burning the think-grace waiting for a `say` that already came (or for
    /// an answer the tool is never going to improve).
    saw_done: bool,
    /// Did the dependency get exercised since the probe window opened?
    saw_call: bool,
    /// THE VERBS THE TOOL AIMED AT THE AI AGENT in this probe window (`payload.type` of
    /// every call mirrored to [`Run::call_ear`], de-duplicated, in order). A rejected
    /// call leaves no event and throws nothing — this list is the ONLY trace of it.
    called_verbs: Vec<String>,
    /// The verbs the dependency SAYS it has, read once from its own `reflect`. Empty
    /// when there is no dependency, or when it does not describe itself — and then no
    /// accusation is made, because we would be inventing it.
    dep_verbs: Vec<String>,
    /// The event types the dependency SAYS it emits (its `reflect`'s `emits`). Its own
    /// events flow through the same ear as the tool's calls; these are not calls.
    dep_emits: Vec<String>,
    /// THE CRASH. The runtime's own exception text from `job_done {ok:false, error}`
    /// on the authored agent — the single most useful thing we can hand the model,
    /// and the thing this driver used to throw away.
    crash: Option<String>,
    /// Is a probe window OPEN? Traffic on the dependency counts ONLY inside one —
    /// outside it, the ear is hearing the AUTHOR's own turns to the brain, which
    /// prove nothing about the tool.
    probing: bool,
    probe: String,
    /// THE CANARY: the tracer the tool's answer must carry (case-insensitive).
    /// `None` → the old, weaker, gameable check ("did it answer at all?").
    expect: Option<String>,
}

impl Run<'_> {
    /// Emit a progress event on the author's own id (UI watches this).
    async fn status(&self, phase: &str, step: usize) {
        self.kernel
            .emit(
                &self.me,
                json!({"type": "status", "phase": phase, "step": step}),
            )
            .await;
    }

    async fn say(&self, text: impl Into<String>) {
        self.kernel
            .emit(&self.me, json!({"type": "say", "text": text.into()}))
            .await;
    }

    /// Drain BOTH ears into the two facts. Did the authored tool ANSWER? The
    /// generic proof is a `say` on its own id — it responded to a turn. (`watch`
    /// mirrors everything sent to the tool, including our own probe, so we look
    /// for the tool's OUTPUT, not our input.) And was the DEPENDENCY exercised?
    /// Anything at all flowing through it counts — the tool's `send` into it is
    /// mirrored to the ear, and a backend that runs also emits `token`/`status`/
    /// `done`. Only a tool that never touched it stays silent.
    fn drain(&mut self) -> bool {
        while let Ok(ev) = self.rx.try_recv() {
            match ev.get("type").and_then(Value::as_str) {
                Some("say") => {
                    self.saw_say = true;
                    self.says.push(
                        ev.get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    );
                }
                // Only the TOOL's own `done` — the ear hears nothing else.
                Some("done") => self.saw_done = true,
                // THE CRASH. The script threw and the runtime named the exception.
                // A dead script will never speak, so its turn is over NOW — and the
                // model finally gets to read the error it has been repairing blind.
                Some("job_done") if ev.get("ok").and_then(Value::as_bool) == Some(false) => {
                    // exec_bridge reports the interpreter's own words in `stderr`
                    // (a traceback); `error` was the in-proc engines' field.
                    self.crash = Some(
                        ev.get("error")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .or_else(|| {
                                ev.get("stderr")
                                    .and_then(Value::as_str)
                                    .filter(|s| !s.is_empty())
                            })
                            .unwrap_or("(the runtime reported no error text)")
                            .to_string(),
                    );
                    self.saw_done = true;
                }
                _ => {}
            }
        }
        while let Ok(ev) = self.call_rx.try_recv() {
            // Outside a probe window this is the author's OWN relay to the brain —
            // heard, and DISCARDED. Only the tool's probe can earn `saw_call`.
            if !self.probing {
                continue;
            }
            self.saw_call = true;
            // …and WHICH verb it aimed at the AI agent. The AI agent's rejection is a return
            // value the ear can never see; the call itself is all we get, so keep it.
            if let Some(v) = ev.get("type").and_then(Value::as_str) {
                if !self.called_verbs.iter().any(|k| k == v) {
                    self.called_verbs.push(v.to_string());
                }
            }
        }
        self.ok()
    }

    /// The AI agent describes ITSELF. Read its `verbs` (and the `emits` that flow through
    /// the same ear but are not calls) ONCE, from its own reflect — this is what lets the
    /// mind tell a REJECTED call from an accepted one without a single hardcoded verb
    /// name. Runs before the first probe window, so the ear discards this traffic.
    async fn learn_dep(&mut self) {
        let Some(dep) = self.must_call.clone() else {
            return;
        };
        let r = self
            .kernel
            .send(vec![Item::new(dep.clone(), json!({"type": "reflect"}))])
            .await
            .remove(0)
            .result;
        self.dep_verbs = keys_of(&r, "verbs");
        self.dep_emits = keys_of(&r, "emits");
    }

    /// Split what the tool aimed at the AI agent into (ACCEPTED, REJECTED) — against the
    /// AI agent's own reflect. The AI agent's own events are subtracted first: they arrive on
    /// the same ear and they are not calls.
    fn split_calls(&self) -> (Vec<&str>, Vec<&str>) {
        self.called_verbs
            .iter()
            .map(String::as_str)
            .filter(|v| !self.dep_emits.iter().any(|e| e == v))
            .partition(|v| self.dep_verbs.iter().any(|k| k == v))
    }

    /// It IS calling the AI agent with a verb the AI agent really has — that is real thinking,
    /// and thinking takes model-time. A call the AI agent does not accept is rejected in
    /// microseconds and buys no grace: waiting on it only buys silence.
    fn thinking(&self) -> bool {
        if self.dep_verbs.is_empty() {
            return true; // it does not describe itself → judge nothing, wait as before
        }
        !self.split_calls().0.is_empty()
    }

    /// THE COMPARISON — what the tool actually SENT the AI agent, against what the AI agent
    /// says it ACCEPTS. This is the fact that used to be invisible: a rejected call is a
    /// return value, so nothing crashed, nothing was emitted, and the model was told only
    /// that its agent was deaf. Returns `None` when there is nothing to compare (no
    /// dependency, no observed call, or an AI agent that does not describe its verbs — we
    /// never invent an accusation).
    fn dep_note(&self) -> Option<String> {
        let dep = self.must_call.as_ref()?.as_str();
        if self.called_verbs.is_empty() || self.dep_verbs.is_empty() {
            return None;
        }
        let (accepted, rejected) = self.split_calls();
        if !rejected.is_empty() {
            Some(rejected_note(
                dep,
                &shown(&rejected),
                &self.dep_verbs.join(", "),
            ))
        } else if !accepted.is_empty() {
            Some(accepted_note(dep, &shown(&accepted)))
        } else {
            None
        }
    }

    /// THE CANARY. Did the tool's answer CARRY the tracer the probe's correct
    /// handling produces? The probe's own words are SUBTRACTED from the answer
    /// first: an agent that quotes the question back ("Received: Reply with exactly
    /// the word BANANA…") is echoing, and an echo must never be able to smuggle the
    /// tracer in on the question's coat-tails. What is left is the tool's OWN
    /// contribution — and only a completion that really came from the brain puts the
    /// tracer there.
    fn canary(&self) -> bool {
        let Some(expect) = &self.expect else {
            return true; // no canary demanded → the old, weaker check
        };
        let expect = expect.to_lowercase();
        let probe = self.probe.to_lowercase();
        self.says.iter().any(|s| {
            let own = s.to_lowercase().replace(&probe, " ");
            own.contains(&expect)
        })
    }

    /// The verdict: it answered, AND (if demanded) it really called its dependency,
    /// AND (if demanded) its answer carries the canary — the one that cannot be
    /// faked by a mechanism.
    fn ok(&self) -> bool {
        self.saw_say && (self.must_call.is_none() || self.saw_call) && self.canary()
    }

    /// It spoke — but it never called what it was told to call. That is an ECHO.
    fn echoed(&self) -> bool {
        self.saw_say && self.must_call.is_some() && !self.saw_call
    }

    /// It spoke, and (where demanded) it did touch the AI agent — but the answer is NOT
    /// the AI agent's: a rejected call whose error it stringified, a completion it threw
    /// away, an invented reply. The canary is missing, so it never THOUGHT.
    fn no_canary(&self) -> bool {
        self.saw_say && self.expect.is_some() && !self.canary() && !self.echoed()
    }

    /// The worst thing it said, trimmed — quoted back so the model sees its own lie.
    fn said(&self) -> String {
        let s = self.says.last().map(String::as_str).unwrap_or("");
        let s: String = s.trim().chars().take(240).collect();
        s.replace('\n', " ")
    }

    /// THE TRACER STAYS SECRET. Every turn on its way to the brain passes through
    /// here, and the probe and its tracer are struck out of it — the ONE choke point,
    /// so no push-back can leak them by accident. The driver probes the tool itself;
    /// the model has no use for the probe, and a tracer it can read is a tracer it can
    /// HARDCODE (both live models tried exactly that). This scrubs the model's own
    /// echoes back at it too: an agent that quotes the probe into its `say` cannot
    /// smuggle the tracer into the transcript via the push-back that quotes it.
    fn scrub(&self, turn: &str) -> String {
        let out = redact(
            turn,
            &self.probe,
            "<the probe turn — I do not show it to you>",
        );
        match &self.expect {
            Some(tracer) => redact(&out, tracer, "<redacted>"),
            None => out,
        }
    }

    /// Open a fresh probe window: discard every stale payload (the author's OWN
    /// turns to the brain flow through the dependency ear too — they are not the
    /// TOOL calling it) and forget the last verdict.
    fn reset_window(&mut self) {
        while self.rx.try_recv().is_ok() {}
        while self.call_rx.try_recv().is_ok() {}
        self.saw_say = false;
        self.saw_done = false;
        self.says.clear();
        self.saw_call = false;
        // called_verbs is RUN-scoped, NOT window-scoped: a verb the tool aimed at the
        // AI agent in ANY window is a fact about the tool that must survive to the NEXT
        // push-back. Clearing it per-window is why a tool that sent {type:'think'} 16
        // times was never once told the verb was rejected — the window that built the
        // push-back had already forgotten. (The per-window verdict — saw_say/saw_call/
        // crash — is reset; the verb HISTORY that diagnoses a failure is not.)
        self.crash = None;
        self.probing = true;
    }

    /// A script that dies AT BIRTH (a SyntaxError, a top-level throw) emits its
    /// `job_done {ok:false}` in the microseconds between `create_agent` returning and
    /// our ear reaching it — the runtime says WHY exactly once, to nobody. So when the
    /// tool is dead and still holds code, START it again inside the open probe window:
    /// the same code throws the same exception, and this time the ear is listening.
    /// This is not a retry and not a fallback — it is how the crash is OBSERVED.
    async fn revive_for_observation(&self, tool: &AgentId) {
        let st = self
            .kernel
            .send(vec![Item::new(tool.clone(), json!({"type": "status"}))])
            .await
            .remove(0)
            .result;
        let alive = st.get("running").and_then(Value::as_bool) == Some(true);
        let has_code = st.get("has_code").and_then(Value::as_bool) == Some(true);
        if !alive && has_code {
            let _ = self
                .kernel
                .send(vec![Item::new(tool.clone(), json!({"type": "start"}))])
                .await
                .remove(0)
                .result;
        }
    }

    /// Send the tool the probe and WAIT (polling) for it to answer. This is the
    /// mind's ground truth: the tool is believed only when it speaks — and, when
    /// `must_call` is set, only when the probe also drove the dependency.
    async fn probe_tool(&mut self, tool: &AgentId) -> bool {
        self.reset_window();
        self.revive_for_observation(tool).await;
        let _ = self
            .kernel
            .send(vec![Item::new(
                tool.clone(),
                json!({"type": "send", "text": self.probe, "client_id": "author"}),
            )])
            .await
            .remove(0)
            .result;
        let mut deadline = tokio::time::Instant::now() + PROBE_WAIT;
        let mut extended = false;
        loop {
            if self.drain() {
                break;
            }
            // Its turn is OVER (`done`) and it is still not ok — waiting longer only
            // buys silence. Judge it now, on what it actually said.
            if self.saw_done {
                break;
            }
            // It IS calling its dependency — with a verb the dependency really has, so
            // that is real thinking, and thinking takes model-time. Grant the grace once.
            // A call the AI agent rejects earns nothing: it already came back, as an error.
            if self.saw_call && !extended && self.thinking() {
                extended = true;
                deadline = tokio::time::Instant::now() + PROBE_THINK_WAIT;
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(PROBE_POLL).await;
        }
        let ok = self.drain();
        self.probing = false; // the window closes; a late `say` still counts, a
                              // stray brain relay never does.
        if !ok && self.crash.is_none() {
            // The ear can MISS a crash that fired before the watch existed (a
            // create-time boot dies in milliseconds — a syntax error always
            // does). The job table cannot: ask it before calling a dead program
            // merely deaf, and carry the interpreter's own words.
            let st = self
                .kernel
                .send(vec![Item::new(tool.clone(), json!({"type": "status"}))])
                .await
                .remove(0)
                .result;
            let failed = st.get("jobs").and_then(Value::as_array).and_then(|jobs| {
                jobs.iter()
                    .rev()
                    .find(|j| j.get("status").and_then(Value::as_str) == Some("failed"))
                    .cloned()
            });
            if let Some(job) = failed {
                let jid = job.get("job_id").and_then(Value::as_str).unwrap_or("");
                let full = self
                    .kernel
                    .send(vec![Item::new(
                        tool.clone(),
                        json!({"type": "status", "job_id": jid}),
                    )])
                    .await
                    .remove(0)
                    .result;
                let text = full["jobs"][0]["stderr"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .or_else(|| {
                        job.get("last_line")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    });
                if let Some(t) = text {
                    self.crash = Some(t);
                }
            }
        }
        ok
    }

    /// The AI agent the answer must come from: the asserted dependency, else the brain
    /// this mind itself thinks with (there is only one language model in the room).
    fn dependency_agent(&self) -> String {
        self.must_call
            .as_ref()
            .unwrap_or(&self.brain)
            .as_str()
            .to_string()
    }

    /// The authored tool's real state, trimmed — what the model must diagnose.
    async fn tool_state(&self, tool: &AgentId) -> String {
        let st = self
            .kernel
            .send(vec![Item::new(tool.clone(), json!({"type": "reflect"}))])
            .await
            .remove(0)
            .result
            .to_string();
        st.chars().take(1200).collect()
    }
}

/// The dependency's id, for a message (`must_call` is always Some where used).
fn dep_name(must_call: &Option<AgentId>) -> String {
    must_call
        .as_ref()
        .map_or_else(|| "-".into(), |d| d.as_str().to_string())
}

/// The ONE closing instruction. Every push-back ends with it — appended ONCE, by
/// [`compose`], so a layered diagnosis never says it twice.
const NEXT_ACT: &str = "Emit your next act now.";

/// A diagnosis, its layered detail (the rejected/accepted call, when we saw one), and
/// the single closing instruction. The order is deliberate: the MECHANISM first (what I
/// observed of your tool), the CAUSE second (what your tool did to the AI agent) — the
/// cause is the last thing it reads before it acts.
fn compose(base: &str, note: Option<String>) -> String {
    match note {
        Some(n) => format!("{base}\n\n{n} {NEXT_ACT}"),
        None => format!("{base} {NEXT_ACT}"),
    }
}

/// The keys of a reflect's `{k: {...}}` map, in the order the agent declared them.
fn keys_of(reflect: &Value, k: &str) -> Vec<String> {
    reflect
        .get(k)
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

/// The calls, as the model wrote them: `{type:'think'}`.
fn shown(verbs: &[&str]) -> String {
    verbs
        .iter()
        .map(|v| format!("{{type:'{v}'}}"))
        .collect::<Vec<_>>()
        .join(" and ")
}

/// THE REJECTED CALL — the fact the mind used to be blind to. The tool DID reach for the
/// AI agent, and the AI agent threw the call away: an invented verb comes back as an `error`
/// RETURN VALUE, so nothing crashed and nothing was emitted, and every push-back the
/// model got described a symptom (it is deaf) instead of the cause (you called it wrong).
/// The verb list is the AI AGENT'S OWN — read from its reflect, never hardcoded here: the
/// point is that the system reports itself, and the model must learn to ask it.
fn rejected_note(dep: &str, used: &str, verbs: &str) -> String {
    format!(
        "THE CAUSE: I watched your agent send {used} to '{dep}'. '{dep}' does NOT accept that — the \
         verbs it actually has are: {verbs}. Your call was REJECTED, and your agent received an error \
         object instead of an answer. REFLECT an agent BEFORE you call it \
         ({{\"act\":{{\"target\":\"{dep}\",\"payload\":{{\"type\":\"reflect\"}}}}}}) and read its \
         `verbs` — then fix your agent's code to call '{dep}' with a verb it really has (update_agent \
         its `code` then stop+boot, or delete_agent then create_agent)."
    )
}

/// The opposite, and it must be said just as plainly: the verb IS one the AI agent has, so
/// the call LANDED. Do not accuse it of a rejection it did not suffer — the defect is
/// downstream, in what it did with the answer (the canary diagnosis).
fn accepted_note(dep: &str, used: &str) -> String {
    format!(
        "WHAT I OBSERVED: your agent sent {used} to '{dep}', and '{dep}' DOES accept that verb — the \
         call landed. So the call is not the defect: the answer '{dep}' returned never came out of \
         your agent as its `say`. Read what that call RETURNS and emit the completion text it carries."
    )
}

/// THE ECHO REFUSAL. The tool answered — and answering is exactly what an echo
/// does. Name the failure precisely (it is not "broken", it is not THINKING) and
/// point at the doc that says what an AI agent is.
fn echo_pushback(must_call: &Option<AgentId>) -> String {
    let dep = dep_name(must_call);
    format!(
        "Your agent answered, but it NEVER called '{dep}' — it is ECHOING, not THINKING. A mind must \
         send the user's turn to '{dep}' and use the completion it returns. Read the kernel's `minds` \
         doc: {{\"act\":{{\"target\":\"kernel\",\"payload\":{{\"type\":\"reflect\",\"readme\":\"minds\"}}}}}}. \
         Then FIX its code (update_agent then stop+boot, or delete_agent + create_agent) and try again."
    )
}

/// THE CANARY REFUSAL. It answered, and it even TOUCHED the AI agent — and the answer
/// still is not the AI agent's (the classic: an invented verb, the kernel's error
/// `JSON.stringify`d and served as the "answer"). Name that precisely, and point it
/// at REFLECTION — never hand it the verb, and NEVER name the canary: the tracer must
/// stay something only a real completion can produce.
fn canary_pushback(said: &str, dep: &str) -> String {
    format!(
        "Your agent answered '{said}' — but that did NOT come from the brain. It must hand the user's \
         turn to '{dep}' and USE the completion the brain returns. If the brain rejected your call, you \
         used the wrong verb or shape: REFLECT it \
         ({{\"act\":{{\"target\":\"{dep}\",\"payload\":{{\"type\":\"reflect\"}}}}}}) to see the verbs it \
         actually accepts, and read the kernel's `minds` doc \
         ({{\"act\":{{\"target\":\"kernel\",\"payload\":{{\"type\":\"reflect\",\"readme\":\"minds\"}}}}}}). \
         Then FIX the agent's code (update_agent then stop+boot, or delete_agent + create_agent) and try \
         again."
    )
}

/// Strike every occurrence of `needle` (ASCII-case-insensitive) out of `text`.
/// ASCII-lowering is byte-length preserving, so the indices found in the lowered
/// haystack are valid char boundaries in the original — an em-dash in a relayed
/// reflect must not be sliced in half.
fn redact(text: &str, needle: &str, mask: &str) -> String {
    if needle.is_empty() {
        return text.to_string();
    }
    let hay = text.to_ascii_lowercase();
    let ned = needle.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while let Some(hit) = hay[i..].find(&ned) {
        let at = i + hit;
        out.push_str(&text[i..at]);
        out.push_str(mask);
        i = at + ned.len();
    }
    out.push_str(&text[i..]);
    out
}

/// THE CRASH REFUSAL — the diagnosis this driver used to SWALLOW. The runtime told
/// us exactly why the script died; a model made to repair blind can only guess, and
/// what a blind agent eventually guesses is that faking the answer will do. So hand
/// it its own exception, verbatim, and let it debug.
fn crash_pushback(tid: &str, err: &str) -> String {
    format!(
        "AUTONOMOUS VERIFY: your agent '{tid}' CRASHED. The runtime reported: {err}. That is the \
         REAL exception from the JavaScript engine that runs your code — read it, find the line it \
         names in the `code` you wrote, and FIX it (update_agent its `code` then stop+boot, or \
         delete_agent then create_agent again with corrected code)."
    )
}

/// THE HANG REFUSAL — a different disease, and it must read as one: nothing threw,
/// the script is alive, and it simply never answers its inbox.
fn deaf_pushback(tid: &str, state: &str) -> String {
    format!(
        "AUTONOMOUS VERIFY: I sent your agent '{tid}' a probe turn of my own choosing \
         ({{\"type\":\"send\",\"text\":<my text>,\"client_id\":\"me\"}}) and it started but emitted \
         NO say/done — it is not answering its inbox (nothing crashed; it is deaf, not broken). Its \
         state: {state}. Its code MUST loop on `await kernel.on_message()`, and on a turn where \
         type=='send' read `text` and `client_id` and then emit \
         {{type:'say', text:<its answer>, client_id:<client_id>}} then \
         {{type:'done', client_id:<client_id>}}. Fix it: update_agent its `code` then stop+boot, or \
         delete_agent then create_agent again with corrected code."
    )
}

/// Trim a relayed result so one fat reflect cannot eat the model's context.
fn trimmed(result: &Value) -> String {
    let mut shown = result.to_string();
    if shown.len() > 8000 {
        shown.truncate(8000);
        shown.push_str("…(truncated)");
    }
    shown
}

/// The `author` verb — the agent's SELF-CORRECT loop. args: `goal:str` (req),
/// `id:str?`, `probe:str?`, `expect:str?` (the canary), `must_call:str?`,
/// `steps:int?` (≤64). The agent drives ITS OWN model to author a durable tool
/// on exec_bridge and refuses its own `final` until the tool is OBSERVED
/// answering a probe the model never sees. Returns `{id, code}` or `{error}`.
pub async fn author(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    // ── inputs (nothing guessed: no goal and no brain are LOUD failures) ──
    let Some(goal) = payload
        .get("goal")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|g| !g.is_empty())
    else {
        return json!({"error": "author: no `goal` — say what the tool must do"});
    };
    // SELF-CORRECT: the agent IS the brain (py-canonical). A `brain` override
    // is gone with the standalone bundle — the loop drives the model of the
    // agent the verb was sent to.
    let brain = agent_id.as_str().to_string();
    let probe = payload
        .get("probe")
        .and_then(Value::as_str)
        .filter(|p| !p.trim().is_empty())
        .unwrap_or(DEFAULT_PROBE)
        .to_string();
    // THE CANARY. Optional — but when it is set, it is what decides.
    let expect = payload
        .get("expect")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(str::to_string);
    let suggested_id = payload.get("id").and_then(Value::as_str).unwrap_or("");
    let steps = payload
        .get("steps")
        .and_then(Value::as_u64)
        .map_or(STEPS, |n| (n as usize).clamp(1, MAX_STEPS));
    // The dependency assertion: "answering" is a weak proof — an echo answers.
    let must_call = payload
        .get("must_call")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(AgentId::from);

    // ── the ears: synthetic listeners on the authored tool, and (if demanded) on
    // the dependency it must be observed calling ──
    let ear = AgentId::from(format!("{}__author_ear", agent_id.as_str()).as_str());
    let (tx, rx) = tokio::sync::mpsc::channel::<Value>(1024);
    kernel.inboxes.insert(ear.clone(), tx);
    let call_ear = AgentId::from(format!("{}__author_call_ear", agent_id.as_str()).as_str());
    let (ctx, call_rx) = tokio::sync::mpsc::channel::<Value>(1024);
    kernel.inboxes.insert(call_ear.clone(), ctx);
    if let Some(dep) = must_call.clone() {
        kernel.watch(&dep, call_ear.clone()).await;
    }

    let mut run = Run {
        kernel,
        me: agent_id.clone(),
        brain: AgentId::from(brain.as_str()),
        ear,
        rx,
        must_call: must_call.clone(),
        call_ear,
        call_rx,
        saw_say: false,
        says: Vec::new(),
        saw_done: false,
        saw_call: false,
        called_verbs: Vec::new(),
        dep_verbs: Vec::new(),
        dep_emits: Vec::new(),
        crash: None,
        probing: false,
        probe,
        expect,
    };
    // Ask the AI agent what it accepts — ONCE, and from the AI agent itself. Nothing in this
    // driver knows the name of a single verb; the dependency reports itself, and that
    // report is what turns "your agent is deaf" into "your call was REJECTED".
    run.learn_dep().await;

    let name_clause = if suggested_id.is_empty() {
        String::new()
    } else {
        format!(" Name it '{suggested_id}'.")
    };
    // The goal turn NEVER carries the probe. The driver probes the tool ITSELF, and
    // the model that can read the probe is the model that hardcodes the answer to it.
    let mut turn = format!(
        "TASK: Author a DURABLE tool — a new agent whose behaviour you write — that satisfies this \
         goal: {goal}.{name_clause} It must ANSWER a turn: when it is sent \
         {{\"type\":\"send\",\"text\":<text>,\"client_id\":<id>}} it emits a 'say' (tagged with that \
         client_id) and then a 'done'. Discover HOW to author it (which executor, its script API, \
         its receive/emit pattern) by REFLECTING first. Create it, then TEST it yourself by sending \
         it {{\"type\":\"send\",\"text\":<any turn you like>,\"client_id\":\"me\"}}. I will ALSO \
         probe it myself, every step, with a turn of MY choosing that you will never see, and I \
         will tell you what it did — so it must work for ANY turn, not for one you prepared for. \
         Only reply final once it really answers."
    );

    let mut tool: Option<AgentId> = None;
    let mut verified = false;
    let mut reflect_streak = 0usize;
    let mut last_state = String::from("no tool was ever created");

    for step in 0..steps {
        // THE ONE DOOR to the model — and everything going through it is scrubbed of
        // the probe and the tracer. There is no other place a turn is built.
        let text = run.scrub(&turn);
        let reply = run
            .kernel
            .send(vec![Item::new(
                run.brain.clone(),
                json!({"type": "send", "text": text, "system_prompt": LEAN_SYSTEM,
                       "client_id": run.me.as_str()}),
            )])
            .await
            .remove(0)
            .result;
        if let Some(err) = reply.get("error").and_then(Value::as_str) {
            run.say(format!("brain error: {err}")).await;
            run.kernel.emit(&run.me, json!({"type": "done"})).await;
            return json!({"error": format!("author: brain '{}' failed: {err}", run.brain.as_str())});
        }
        let resp = reply.get("response").and_then(Value::as_str).unwrap_or("");

        // A late answer may have landed since the last turn.
        if run.drain() {
            verified = true;
        }

        // NO REPAIR — an unparseable envelope is a nudge, not a patch.
        let Some(action) = first_json(resp) else {
            turn = "Reply with ONE JSON object: {\"act\":{...}} or {\"final\":...}.".into();
            continue;
        };

        // ── VERIFY-REPAIR: refuse an unverified completion ──
        if action.get("final").is_some() {
            if let Some(t) = tool.clone() {
                if !verified && run.probe_tool(&t).await {
                    verified = true;
                }
            }
            if verified {
                break;
            }
            run.status("repairing", step).await;
            // THE CALL IT MADE — layered into every diagnosis below that has a call to
            // explain. A rejected call is silent, so no other line of the push-back can
            // ever name it.
            let note = run.dep_note();
            // THE CRASH refusal — first, because a script that threw is broken before
            // it is anything else, and its exception is the most actionable truth we
            // have. (Same order as the autonomous verify below.)
            if let (Some(err), Some(t)) = (run.crash.clone(), tool.clone()) {
                run.say("the model claimed success — but its tool CRASHED; feeding it the error")
                    .await;
                last_state = format!("it CRASHED: {err}");
                turn = format!(
                    "You replied final, but your tool is DEAD — you have NOT succeeded. {}",
                    compose(&crash_pushback(t.as_str(), &err), note)
                );
                continue;
            }
            // The ECHO refusal: it spoke, but it never called what it must call.
            if run.echoed() {
                run.say("the model claimed success — but its tool ECHOES, it never called the brain; pushing back")
                    .await;
                last_state = format!("it answers, but it never calls '{}'", dep_name(&must_call));
                turn = compose(&echo_pushback(&must_call), None);
                continue;
            }
            // The CANARY refusal: it spoke, it even called the AI agent — and the answer
            // is not the AI agent's. It never thought.
            if run.no_canary() {
                run.say("the model claimed success — but its tool's answer does NOT come from the brain; pushing back")
                    .await;
                last_state = format!(
                    "it answers '{}', but that answer does not come from the brain",
                    run.said()
                );
                turn = compose(&canary_pushback(&run.said(), &run.dependency_agent()), note);
                continue;
            }
            // Nothing threw and nothing spoke: it is alive and DEAF (or it does not
            // exist at all). Say WHICH — the two repairs are not the same repair. And
            // when it was DEAF because the AI agent rejected its call, `note` says so: that
            // is the difference between a symptom and a cause.
            run.say("the model claimed success — but its tool has not answered; pushing back")
                .await;
            let diagnosis = match tool.clone() {
                Some(t) => {
                    let state = run.tool_state(&t).await;
                    last_state = state.clone();
                    compose(&deaf_pushback(t.as_str(), &state), note)
                }
                None => {
                    last_state = "no tool was ever created".into();
                    compose(
                        "You have not created any tool yet — create_agent one NOW, on a *_runtime \
                         executor, whose `code` satisfies the goal.",
                        None,
                    )
                }
            };
            turn = format!(
                "You replied final, but I have NOT observed your tool answer the probe turn I sent it \
                 — you have NOT succeeded. Do not give up. {diagnosis}"
            );
            continue;
        }

        let Some(act) = action.get("act") else {
            turn = "Reply with ONE JSON object {\"act\":{\"target\":..,\"payload\":..}}.".into();
            continue;
        };
        let target = act
            .get("target")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let mut act_payload = act.get("payload").cloned().unwrap_or(json!({}));
        // OWNED: the payload is rewritten below (placement), so a borrow of it
        // must not still be live at that point.
        let verb = act_payload
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let verb = verb.as_str();
        run.status(
            if verb == "create_agent" {
                "authoring"
            } else {
                "reflecting"
            },
            step,
        )
        .await;

        // The tools an author builds are ITS OWN subtree — but placement is now
        // stated by `parent_id`, not by who is addressed. Kernel verbs are
        // answered by the ROOT alone, so a structural mutation goes to 'kernel'
        // and names THIS agent as the parent: the tool lands as a child of the
        // author, later `delete_agent`/`update_agent` edit it, and the
        // placement-keyed exemption means it costs zero grants (forking your
        // own subtree is baseline). Under deny-all an agent could not reach a
        // tool parented elsewhere anyway.
        let privileged = fantastic_kernel::PRIVILEGED_VERBS.contains(&verb);
        if verb == "create_agent" {
            if let Some(obj) = act_payload.as_object_mut() {
                obj.insert("parent_id".to_string(), json!(agent_id.0));
            }
        }
        let dispatch_target = if privileged {
            AgentId::from("kernel")
        } else {
            AgentId::from(target.as_str())
        };
        let result = run
            .kernel
            .send(vec![Item::new(
                dispatch_target.clone(),
                act_payload.clone(),
            )])
            .await
            .remove(0)
            .result;

        // A fresh script agent? That is the candidate tool — put the ear on it.
        let hm = act_payload
            .get("handler_module")
            .and_then(Value::as_str)
            .unwrap_or("");
        // MATCH THE EXECUTOR, not a naming convention. `ends_with("_runtime.tools")`
        // silently stopped recognizing tools when the executor became
        // `exec_bridge.tools` — the exact harness-asserts-the-wrong-surface scar
        // py's emergence README records.
        if verb == "create_agent" && hm == "exec_bridge.tools" && result.get("error").is_none() {
            if let Some(id) = result.get("id").and_then(Value::as_str) {
                let t = AgentId::from(id);
                run.kernel.watch(&t, run.ear.clone()).await;
                // A subprocess tool needs a beat to boot and register its
                // on_message before the first probe — the in-proc engines had no
                // such window, and probing into it reads a good tool as deaf.
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                tool = Some(t);
                verified = false; // a NEW tool is unproven, whatever the old one did
                run.say(format!("authored '{id}' — verifying it actually answers"))
                    .await;
            }
        }
        // LOOP-BREAKER: a weak model reads forever and never commits.
        if verb == "create_agent" {
            reflect_streak = 0;
        } else if tool.is_none() && verb == "reflect" {
            reflect_streak += 1;
        }

        if run.drain() {
            verified = true;
        }
        let shown = trimmed(&result);

        // ── AUTONOMOUS VERIFY: probe the tool OURSELVES and report ground truth ──
        let mut note = String::new();
        if let Some(t) = tool.clone() {
            if !verified {
                run.status("verifying", step).await;
                let probed = run.probe_tool(&t).await;
                // THE CALL IT MADE, read from the window that just closed (see the
                // `final` branch): the one fact that used to be unobservable, layered
                // into whichever diagnosis fires.
                let call = run.dep_note();
                if probed {
                    verified = true;
                } else if let Some(err) = run.crash.clone() {
                    // IT THREW — and now it is told what it threw. This is the whole
                    // lever: a model that can read its own stack trace can debug; a
                    // model that cannot can only guess, and then cheat.
                    run.status("repairing", step).await;
                    last_state = format!("it CRASHED: {err}");
                    note = format!("\n\n{}", compose(&crash_pushback(t.as_str(), &err), call));
                } else if run.echoed() {
                    // It ANSWERED — and that is exactly the trap: the cheapest
                    // artifact that satisfies "it speaks" is an echo.
                    run.status("repairing", step).await;
                    last_state =
                        format!("it answers, but it never calls '{}'", dep_name(&must_call));
                    note = format!(
                        "\n\nAUTONOMOUS VERIFY: {}",
                        compose(&echo_pushback(&must_call), None)
                    );
                } else if run.no_canary() {
                    // It answered, it touched the AI agent — and the answer is not the
                    // AI agent's. The canary is the only check that catches this.
                    run.status("repairing", step).await;
                    last_state = format!(
                        "it answers '{}', but that answer does not come from the brain",
                        run.said()
                    );
                    note = format!(
                        "\n\nAUTONOMOUS VERIFY: {}",
                        compose(&canary_pushback(&run.said(), &run.dependency_agent()), call)
                    );
                } else {
                    // Alive, silent, nothing thrown: DEAF. A distinct message, so the
                    // model can tell a hang from a crash — and, when it went deaf because
                    // the AI agent REJECTED its call, `call` names the cause it could not see.
                    run.status("repairing", step).await;
                    let state = run.tool_state(&t).await;
                    last_state = state.clone();
                    note = format!("\n\n{}", compose(&deaf_pushback(t.as_str(), &state), call));
                }
            }
        }
        if verified {
            break;
        }
        turn = format!("Result of your send to '{target}':\n{shown}{note}");

        // Nothing built after repeated reads: STOP feeding docs, force the author.
        if reflect_streak >= 2 && tool.is_none() {
            run.status("repairing", step).await;
            turn = format!(
                "You have now reflected {reflect_streak} times and built NOTHING — reading is not \
                 progress and you are burning your turns. STOP reflecting. Your NEXT reply MUST be \
                 exactly ONE create_agent act (target 'kernel') that authors a durable tool on a \
                 exec_bridge executor, whose `code` satisfies the goal: {goal}. On a turn where \
                 type=='send' it must emit {{type:'say', text:<its answer>, client_id:<the incoming \
                 client_id>}} then {{type:'done', client_id}}. Do NOT reflect again — author it now."
            );
        }
    }

    // ── verdict: NEVER claim success unverified ──
    let out = match (verified, tool.clone()) {
        (true, Some(t)) => {
            let code = meta_str(&t, run.kernel, "code").map_or(Value::Null, Value::from);
            run.say(format!("verified: '{}' answers the probe", t.as_str()))
                .await;
            json!({"id": t.as_str(), "code": code})
        }
        _ => {
            let tid = tool
                .as_ref()
                .map(|t| t.as_str().to_string())
                .unwrap_or_else(|| "none".into());
            run.say("gave up — no working tool").await;
            json!({"error": format!(
                "author: gave up after {steps} steps — tool: {tid}; last state: {last_state}"
            )})
        }
    };
    run.kernel.emit(&run.me, json!({"type": "done"})).await;
    if let Some(t) = tool {
        run.kernel.unwatch(&t, &run.ear).await;
    }
    if let Some(dep) = must_call {
        run.kernel.unwatch(&dep, &run.call_ear).await;
    }
    run.kernel.inboxes.remove(&run.ear);
    run.kernel.inboxes.remove(&run.call_ear);
    out
}
