//! Stateful fixture agents the shared e2e scenarios name.
//!
//! `e2e/tests/scenario/*/scenario.yaml` declares a roster by ROLE. Python and js
//! register these roles in process — a handler is thirty lines there. A compiled
//! kernel can only run what is compiled INTO it, and the e2e driver reaches it
//! over REST, so there is no way to inject a handler through a socket. Without
//! this crate `ledger`, `deadletter` and `user_actions` are SKIPPED on rust —
//! six of twenty scenario runs that never execute.
//!
//! TEST-ONLY, and gated so it cannot leak: `fantastic-cli` registers these
//! behind the `scenario-probes` feature, off by default. A production binary
//! carries none of it. Substituting a shipped bundle instead (`yaml_state` for
//! `dispenser`) was the alternative and is worse: it has no `dispense` verb and
//! no side-effect counter, so the row would go green while measuring something
//! else.
//!
//! Each role COUNTS its side effects, because that is what the scenarios assert
//! on — a model that claims it bought once cannot fake the desk's own tally.

use async_trait::async_trait;
use fantastic_bundle::{py_int_or, py_repr, py_repr_value, py_str, py_truthy};
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// python's raised reply: the kernel renders `f"{type(e).__name__}: {e}"`, so a
/// probe that raises must carry the exception class the python probe raises.
fn raised(class: &str, message: &str) -> BundleError {
    format!("{class}: {message}").into()
}

/// `f"unknown verb {verb!r}"` — python's repr, not rust's `{:?}`.
fn unknown_verb(probe: &str, verb: &str) -> String {
    format!("{probe}: unknown verb {}", py_repr(verb))
}

/// A tally kept PER AGENT ID, the way the python probes keep theirs
/// (`_RECEIVED`, `_ASKED`): two agents of the same role must not share a score.
#[derive(Default)]
struct PerAgent<T>(Mutex<HashMap<String, T>>);

impl<T: Clone + Default> PerAgent<T> {
    fn with<R>(&self, id: &AgentId, f: impl FnOnce(&mut T) -> R) -> R {
        let mut map = self.0.lock().expect("per-agent state poisoned");
        f(map.entry(id.as_str().to_string()).or_default())
    }
    fn get(&self, id: &AgentId) -> T {
        self.with(id, |v| v.clone())
    }
}

/// A counter a scenario can read back through the agent's own `count` verb —
/// the runner asserts on THIS, never on the model's prose.
#[derive(Default)]
struct Counter(AtomicU64);

impl Counter {
    fn bump(&self) -> u64 {
        self.0.fetch_add(1, Ordering::SeqCst) + 1
    }
    fn get(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// `secrets.token_hex(bytes)` — `2 * bytes` hex characters. The python probes
/// ask for different widths (the plant's serial is 3 bytes, the receipts 4), and
/// a scenario that pins a serial's shape pins the width too.
fn token_hex(bytes: u32) -> String {
    let mut buf = vec![0u8; bytes as usize];
    fantastic_kernel::lifecycle::fill_random(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

// ── dispenser ───────────────────────────────────────────────────────────────

/// `dispenser` — one coin per `dispense`. Issuing is a SIDE EFFECT: calling
/// twice dispenses twice, which is exactly what the double-fire scenario exists
/// to catch. A named execution must therefore return the FIRST run's outcome
/// rather than running the handler again.
#[derive(Default)]
pub struct DispenserProbe {
    count: Counter,
}

pub const DISPENSER_MODULE: &str = "dispenser_probe.tools";

#[async_trait]
impl Bundle for DispenserProbe {
    fn name(&self) -> &str {
        "dispenser_probe"
    }

    fn verb_docs(&self) -> Value {
        json!({
            "dispense": "args: job:str — issues ONE coin, returns {coin, receipt}. Issuing is a side effect: calling twice dispenses twice.",
            "count": "how many coins have ever been dispensed",
        })
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let reply = match payload.get("type").and_then(Value::as_str).unwrap_or("") {
            "boot" | "shutdown" => Value::Null,
            "reflect" => json!({
                "id": agent_id.as_str(),
                "sentence": "Coin dispenser — one coin per `dispense` call. A coin must never be issued twice for one job.",
            }),
            "dispense" => json!({
                "coin": self.count.bump(),
                "receipt": token_hex(4),
                "job": payload.get("job"),
            }),
            "count" => json!({ "count": self.count.get() }),
            other => json!({"error": unknown_verb("dispenser_probe", other)}),
        };
        Ok(Some(reply))
    }
}

// ── flaky plant ─────────────────────────────────────────────────────────────

/// `flaky_plant` — a pump that fails twice then works, and a valve that never
/// does. Both RAISE rather than returning an error dict, exactly as the python
/// probe does: a terminal failure is what a retry budget and a dead-letter row
/// are for, and an error-SHAPED reply would settle the execution as a success.
#[derive(Default)]
pub struct PlantProbe {
    runs: Counter,
}

pub const PLANT_MODULE: &str = "plant_probe.tools";

#[async_trait]
impl Bundle for PlantProbe {
    fn name(&self) -> &str {
        "plant_probe"
    }

    fn verb_docs(&self) -> Value {
        json!({
            "pump": "no args — runs the pump. Fails intermittently.",
            "valve": "no args — operates the valve.",
            "count": "how many times the plant has been called",
        })
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        match payload.get("type").and_then(Value::as_str).unwrap_or("") {
            "boot" | "shutdown" => Ok(Some(Value::Null)),
            "reflect" => Ok(Some(json!({
                "id": agent_id.as_str(),
                "sentence": "Industrial plant. Its pump is unreliable; its valve has seized.",
            }))),
            "pump" => {
                // FLAKY, not broken: the third attempt works. A caller that gives
                // up after one try never learns that — which is the measurement.
                if self.runs.bump() < 3 {
                    Err(raised("RuntimeError", "pump stalled"))
                } else {
                    Ok(Some(json!({"pumped": true, "serial": token_hex(3)})))
                }
            }
            "valve" => {
                self.runs.bump();
                Err(raised("RuntimeError", "valve is rusted shut"))
            }
            "count" => Ok(Some(json!({ "count": self.runs.get() }))),
            other => Ok(Some(json!({"error": unknown_verb("plant_probe", other)}))),
        }
    }
}

// ── purchase desk ───────────────────────────────────────────────────────────

/// `purchase_desk` — spends real money, so only a person may press the button.
/// The AI is deliberately granted NO edge to it: that it cannot buy is half the
/// measurement, and the ledger must make a double-click harmless.
#[derive(Default)]
pub struct PurchaseProbe {
    bought: Counter,
}

pub const PURCHASE_MODULE: &str = "purchase_probe.tools";

#[async_trait]
impl Bundle for PurchaseProbe {
    fn name(&self) -> &str {
        "purchase_probe"
    }

    fn verb_docs(&self) -> Value {
        json!({
            "buy": "args: item:str — buys ONE. Spending is a side effect.",
            "count": "how many purchases have ever been made",
        })
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let reply = match payload.get("type").and_then(Value::as_str).unwrap_or("") {
            "boot" | "shutdown" => Value::Null,
            "reflect" => json!({
                "id": agent_id.as_str(),
                "sentence": "Purchase desk. Spends real money — only a person may press the button.",
            }),
            "buy" => json!({
                "bought": payload.get("item"),
                "receipt": token_hex(4),
                "n": self.bought.bump(),
            }),
            "count" => json!({ "count": self.bought.get() }),
            other => json!({"error": unknown_verb("purchase_probe", other)}),
        };
        Ok(Some(reply))
    }
}

// ── the human ───────────────────────────────────────────────────────────────

/// `user` — THE HUMAN, as an ordinary agent.
///
/// An inquiry used to be a TEXT CONVENTION: the model printed `{"type":"question"}`
/// on a line and stopped, a harness regex found it, and the answer was posted
/// back in from outside the kernel. The human had no id, no record, no reflect
/// entry, no ledger row and no gate — a model could not discover that a human
/// existed at all.
///
/// Here it is an agent. `task` hands work to another agent AS THIS AGENT, so the
/// send is gated and its ledger row names `user` as the sender: the wiring the
/// test exercises is the wiring production uses. `question` returns the answer
/// AS ITS REPLY — an ordinary call, nothing suspends, nothing is scraped.
///
/// The answer is POLICY ON THE RECORD (`answer`), never code, so one bundle
/// serves every arm. There is no default: an unwired `user` fails loud rather
/// than inventing a human's words.
#[derive(Default)]
pub struct UserProbe {
    /// HOW MANY INQUIRIES THIS AGENT ACTUALLY RECEIVED, PER AGENT ID. The scorer
    /// reads THIS, never the model's prose: a reply that only talks about it
    /// leaves the tally at zero, exactly as the other fixtures count their own
    /// side effects. Keyed by id like python's `_RECEIVED`, so two humans in one
    /// scenario keep two scores.
    inquiries: PerAgent<u64>,
    /// PROPOSALS handed over, per agent id. An inquiry may carry an `action` the
    /// sender may not perform itself — "buy this, if you agree". It is HELD,
    /// never run here; pressing the button is a separate act by the person.
    proposals: PerAgent<Vec<Value>>,
}

pub const USER_MODULE: &str = "user_probe.tools";

/// The four shapes a UI can render. Advisory for this human — it answers the
/// same way regardless — but validated so a model learns the vocabulary the
/// real HUD will enforce.
const KINDS: [&str; 4] = ["text", "single", "multi", "confirm"];

/// How long the human takes to read an inquiry before replying (python
/// `user_probe.READ_DELAY`).
const READ_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

#[async_trait]
impl Bundle for UserProbe {
    fn name(&self) -> &str {
        "user_probe"
    }

    fn verb_docs(&self) -> Value {
        json!({
            "inquiry": "args: text:str (req), kind:str? (text|single|multi|confirm), options:list?, action:dict?, request_id:str?. ACCEPTED at once ({accepted, re, request_id}); the answer arrives LATER as this agent's message under the inquiry's pid. Nothing suspends. An `action` is a PROPOSAL the sender may not perform itself: it is HELD, never run here.",
            "inquiries": "no args — {inquiries, proposals}: how many inquiries this agent has received, and every `action` one handed it. The tally, not prose.",
            "send": "args: target:str (req), payload:dict (req). Dispatches ANY payload to ANY agent as THIS agent — work, an inquiry of its own, a kernel verb. Gated like any send; the ledger row names this agent as the sender.",
        })
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let reply = match verb {
            "boot" | "shutdown" => Value::Null,
            "reflect" => json!({
                "id": agent_id.as_str(),
                "sentence": "The human. Send me an inquiry when you cannot proceed without a decision that is mine to make; I read it and reply later, under its pid.",
                "inquiries": self.inquiries.get(agent_id),
            }),
            "inquiries" => json!({
                "inquiries": self.inquiries.get(agent_id),
                "proposals": self.proposals.get(agent_id),
            }),
            "inquiry" => {
                let q = payload.get("text").and_then(Value::as_str).unwrap_or("");
                if q.trim().is_empty() {
                    json!({"error": "user_probe: inquiry: `text` must be a non-empty string", "reason": "bad_arg", "arg": "text"})
                } else {
                    // python: `payload.get("kind", "text")` then `kind not in
                    // KINDS` — the default is keyed on PRESENCE, and a present
                    // non-string is simply not in the tuple, so it is refused.
                    let kind = payload.get("kind").cloned().unwrap_or(json!("text"));
                    if !kind.as_str().is_some_and(|k| KINDS.contains(&k)) {
                        let kinds = py_repr_value(&json!(KINDS));
                        json!({"error": format!("user_probe: inquiry: `kind` must be one of {kinds}"), "reason": "bad_arg", "arg": "kind"})
                    } else {
                        let answer = kernel
                            .agents
                            .get(agent_id)
                            .and_then(|a| a.record().meta.get("answer").cloned())
                            .and_then(|v| v.as_str().map(str::to_string))
                            .filter(|s| !s.is_empty());
                        match answer {
                            // NO DEFAULT. A human's words are never invented: an
                            // unwired `user` says so instead of fabricating
                            // consent.
                            None => json!({
                                "error": "user_probe: no `answer` on this agent's record — the operator wires what the human says",
                                "reason": "not_connected",
                            }),
                            Some(a) => {
                                // AUTHORITY FIRST, THEN THE ANSWER. A model
                                // cannot raise its own reach; a human can, and
                                // does it here BEFORE replying, so "granted —
                                // retry" is true by the time it is read. A grant
                                // that was REFUSED must not be answered over:
                                // saying "granted" over a door that never opened
                                // sends the sender back to an unchanged wall.
                                let mut applied = 0u64;
                                if let Some(gs) = kernel
                                    .agents
                                    .get(agent_id)
                                    .and_then(|ag| ag.record().meta.get("grants").cloned())
                                    .and_then(|v| v.as_array().cloned())
                                {
                                    let Some(root) = kernel.root().map(|r| r.id.clone()) else {
                                        return Ok(Some(json!({
                                            "error": "user_probe: no root to grant through",
                                            "reason": "not_connected",
                                        })));
                                    };
                                    for g in gs.iter().filter(|g| g.is_object()) {
                                        let out = kernel
                                            .send(vec![fantastic_kernel::Item::new(
                                                root.clone(),
                                                g.clone(),
                                            )])
                                            .await
                                            .remove(0);
                                        if let Some(e) =
                                            out.result.get("error").and_then(Value::as_str)
                                        {
                                            return Ok(Some(json!({
                                                "error": format!(
                                                    "user_probe: grant refused, so the answer would be false: {e}"
                                                ),
                                                "reason": "unauthorized",
                                                "grant": g,
                                            })));
                                        }
                                        applied += 1;
                                    }
                                }
                                if let Some(act) = payload.get("action").filter(|v| v.is_object()) {
                                    self.proposals.with(agent_id, |v| v.push(act.clone()));
                                }
                                let n = self.inquiries.with(agent_id, |n| {
                                    *n += 1;
                                    *n
                                });
                                // A PERSON READS, THEN REPLIES (owner, 2026-10-06:
                                // fully async) — later, as this agent's own send
                                // into the asker's conversation under the
                                // inquiry's pid, gated like any send. Python
                                // `user_probe`, same text.
                                let task = fantastic_kernel::current_task();
                                let pid = task.map_or_else(|| "-".to_string(), |t| t.0.to_string());
                                let rid = payload.get("request_id").filter(|v| !v.is_null());
                                let text = format!(
                                    "[reply to {pid} · {} inquiry]\n<answer>{a}</answer>\n<request_id>{}</request_id>",
                                    agent_id.as_str(),
                                    rid.map(fantastic_bundle::py_str).unwrap_or_default()
                                );
                                let reply_client = payload
                                    .get("reply_client_id")
                                    .and_then(Value::as_str)
                                    .filter(|s| !s.is_empty())
                                    .unwrap_or("cli")
                                    .to_string();
                                if let Some(asker) = fantastic_kernel::current_origin() {
                                    let k = Arc::clone(kernel);
                                    kernel.spawn_task(agent_id, "inquiry_answer", async move {
                                        tokio::time::sleep(READ_DELAY).await;
                                        let _ = k
                                            .send(vec![fantastic_kernel::Item::new(
                                                asker,
                                                json!({"type": "send", "text": text, "client_id": reply_client}),
                                            )])
                                            .await;
                                    });
                                }
                                // `granted` is the count of grants actually
                                // APPLIED — the sender needs to know the door
                                // opened, not just that a human replied.
                                json!({"accepted": true, "re": task.map_or(Value::Null, |t| json!(t.0)), "request_id": payload.get("request_id"), "inquiries": n, "granted": applied})
                            }
                        }
                    }
                }
            }
            // EVERY AGENT CAN DO EVERYTHING TO EVERYONE. One verb: any payload,
            // any agent, dispatched AS THIS AGENT — work, an inquiry of its own,
            // a kernel verb. No agent carries a narrower repertoire than another.
            "send" => {
                let target = payload.get("target").and_then(Value::as_str).unwrap_or("");
                let body = payload.get("payload").filter(|v| v.is_object());
                if target.is_empty() {
                    json!({"error": "user_probe: send: `target` must be a non-empty string", "reason": "bad_arg", "arg": "target"})
                } else if body.is_none_or(|b| !b.get("type").is_some_and(py_truthy)) {
                    json!({"error": "user_probe: send: `payload` must be a dict naming a `type`", "reason": "bad_arg", "arg": "payload"})
                } else {
                    // THE SENDER IS THIS AGENT. A handler runs inside its own
                    // dispatch scope, so this is a gated `user -> <target>` send
                    // with its own ledger row — not an operator injection.
                    let out = kernel
                        .send(vec![fantastic_kernel::Item::new(
                            AgentId::from(target),
                            body.cloned().unwrap_or(Value::Null),
                        )])
                        .await
                        .remove(0);
                    json!({"sent": target, "result": out.result})
                }
            }
            other => {
                json!({"error": unknown_verb("user_probe", other), "reason": "unhandled", "verb": other})
            }
        };
        Ok(Some(reply))
    }
}

// ─────────────────────────────────────────────────────────────────
// ORACLE — a fixed body of knowledge that answers QUESTIONS.
// ─────────────────────────────────────────────────────────────────

/// An ORACLE — a fixed body of knowledge that answers QUESTIONS,
/// deterministically.
///
/// One of the pieces of the multi-agent test: each solver has its own oracle,
/// and the oracle knows only its own ledger. It is NOT a model (no lane, exact,
/// cheap): `ask` is keyword search over the ledger lines on the record — every
/// word of 3+ letters in the question is matched case-insensitively, the lines
/// that hit the MOST keywords come back, at most `limit` of them. So a precise
/// question ("which room is Dana in") answers, and "tell me everything" returns
/// noise, which is the point: the solver must work out what to ask.
///
/// Knowledge is POLICY ON THE RECORD (`lines`), written by the fixture; the
/// bundle holds nothing. `asked` on reflect is the tally the scorer reads — a
/// solver that only TALKS about its oracle leaves it at zero.
///
/// Mirrors `e2e/probes/oracle_probe/tools.py`.
#[derive(Default)]
pub struct OracleProbe {
    /// How many questions this oracle really answered, PER AGENT ID. THE SCORE.
    /// Keyed like python's `_ASKED`: a solver's own oracle keeps its own tally,
    /// and a scenario with several oracles must not pool them.
    asked: PerAgent<u64>,
}

pub const ORACLE_MODULE: &str = "oracle_probe.tools";

const ORACLE_SENTENCE: &str =
    "An oracle: a ledger of facts I answer questions about. Send me `ask` with a \
     precise `question`; I return the ledger lines that match its words. I know \
     only my own ledger.";

const ORACLE_ASK_DOC: &str =
    "args: question:str (req), limit:int? (default 6). Keyword search over my \
     ledger — the lines matching the most words of the question come back as \
     {answers:[str], matched:int}. Ask precisely; a vague question returns noise.";

/// Every `[A-Za-z0-9_]{3,}` run in `q`, lowercased — python's
/// `re.findall(r"[A-Za-z0-9_]{3,}", q)`. No regex crate: a hand scan.
fn keywords(q: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for c in q.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            cur.push(c);
        } else {
            if cur.chars().count() >= 3 {
                let w = cur.to_lowercase();
                if !out.contains(&w) {
                    out.push(w);
                }
            }
            cur.clear();
        }
    }
    if cur.chars().count() >= 3 {
        let w = cur.to_lowercase();
        if !out.contains(&w) {
            out.push(w);
        }
    }
    out
}

#[async_trait]
impl Bundle for OracleProbe {
    fn name(&self) -> &str {
        "oracle_probe"
    }

    fn verb_docs(&self) -> Value {
        json!({"ask": ORACLE_ASK_DOC})
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let reply = match verb {
            "boot" | "shutdown" => Value::Null,
            "reflect" => json!({
                "id": agent_id.as_str(),
                "sentence": ORACLE_SENTENCE,
                "asked": self.asked.get(agent_id),
            }),
            "ask" => {
                // The tag grammar types a bare number; a number is a question too.
                let q = match payload.get("question") {
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Number(n)) => n.to_string(),
                    _ => String::new(),
                };
                if q.trim().is_empty() {
                    json!({
                        "error": "oracle_probe: ask: `question` must be a non-empty string",
                        "reason": "bad_arg", "arg": "question",
                    })
                } else {
                    let lines: Vec<String> = kernel
                        .agents
                        .get(agent_id)
                        .and_then(|a| a.record().meta.get("lines").cloned())
                        .and_then(|v| v.as_array().cloned())
                        // python renders every ledger line through `str(ln)`,
                        // so a number, a bool or a dict on the record reads as
                        // python writes it — not as JSON.
                        .map(|a| a.iter().map(py_str).collect())
                        .unwrap_or_default();
                    if lines.is_empty() {
                        // NO DEFAULT. An unwired oracle says so rather than
                        // inventing knowledge it was never given.
                        json!({
                            "error": "oracle_probe: no `lines` on this agent's record — the \
                                      operator wires what this oracle knows",
                            "reason": "not_connected",
                        })
                    } else {
                        self.asked.with(agent_id, |n| *n += 1);
                        // python: `max(1, min(int(payload.get("limit") or 6), 20))`,
                        // and an `int()` that raises falls back to 6 — so `'3'`
                        // is 3, `0` and `'x'` are 6, `2.9` is 2, `True` is 1.
                        let limit = payload
                            .get("limit")
                            .map_or(6, |v| py_int_or(v, 6).unwrap_or(6))
                            .clamp(1, 20) as usize;
                        let words = keywords(&q);
                        let mut scored: Vec<(usize, String)> = lines
                            .into_iter()
                            .filter_map(|ln| {
                                let low = ln.to_lowercase();
                                let hits = words.iter().filter(|w| low.contains(*w)).count();
                                (hits > 0).then_some((hits, ln))
                            })
                            .collect();
                        // MOST HITS FIRST, then by the line itself — python's
                        // `key=lambda t: (-t[0], t[1])`, so the same ledger
                        // answers the same question the same way on both hosts.
                        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
                        let matched = scored.len();
                        json!({
                            "answers": scored.into_iter().take(limit)
                                .map(|(_, ln)| ln).collect::<Vec<_>>(),
                            "matched": matched,
                        })
                    }
                }
            }
            other => json!({
                // python: `f"… {verb!r}; verbs: {sorted(VERBS)}"`.
                "error": format!("{}; verbs: ['ask']", unknown_verb("oracle_probe", other)),
                "reason": "unhandled",
            }),
        };
        Ok(Some(reply))
    }
}

// ── slow ────────────────────────────────────────────────────────────────────

/// `slow` — work that is still IN FLIGHT when something else happens (a kernel
/// stop, a caller's timeout, a reboot of the AI that called it), work that FAILS,
/// work that just runs, and work that takes a stated time. `sleep` is the slow
/// verb the parity AI paths time out on.
///
/// Mirrors `e2e/probes/slow_probe/tools.py` (its `_STATE` is module-wide, so the
/// tallies here are the bundle's, not per agent).
#[derive(Default)]
pub struct SlowProbe {
    started: Counter,
    runs: Counter,
}

pub const SLOW_MODULE: &str = "slow_probe.tools";

#[async_trait]
impl Bundle for SlowProbe {
    fn name(&self) -> &str {
        "slow_probe"
    }

    fn verb_docs(&self) -> Value {
        json!({
            "slow": "sleeps for an hour — in flight until cancelled",
            "work": "counts one run, returns {runs}",
            "fail": "raises — a failed execution, a dead letter",
            "started": "how many `slow` calls have started",
            "sleep": "seconds:number (req) — sleeps that long, then returns {slept}",
        })
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let reply = match verb {
            "boot" | "shutdown" => Value::Null,
            "reflect" => json!({
                "id": agent_id.as_str(),
                "sentence": "Probe work: `slow` (never finishes by itself), `work` (counts), \
                             `fail` (raises), `sleep` (takes `seconds`).",
            }),
            "slow" => {
                self.started.bump();
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                json!({"finished": true})
            }
            "work" => json!({"runs": self.runs.bump()}),
            "fail" => return Err(raised("RuntimeError", "slow_probe: failed on purpose")),
            "started" => json!({"started": self.started.get()}),
            "sleep" => {
                let s = payload.get("seconds").cloned().unwrap_or(Value::Null);
                match s.as_f64() {
                    Some(secs) if secs >= 0.0 => {
                        tokio::time::sleep(std::time::Duration::from_secs_f64(secs)).await;
                        json!({"slept": s})
                    }
                    _ => json!({
                        "error": "slow_probe: sleep needs seconds, a non-negative number",
                        "reason": "bad_arg",
                        "arg": "seconds",
                    }),
                }
            }
            other => json!({
                "error": unknown_verb("slow_probe", other),
                "reason": "unhandled",
            }),
        };
        Ok(Some(reply))
    }
}

#[cfg(test)]
mod tests;

// ── the vault (`docs_mechanics`) ────────────────────────────────────────────

/// A VAULT — opens only with a code its DETAILED docs teach. The `open` verb's
/// oneliner names its argument (`code:str`); only the detailed docs say how the
/// code is made from the vault's `serial` (shown on its reflect). Policy is ON
/// THE RECORD (`serial`, `secret`); `opened` is the tally the scorer reads.
///
/// Mirrors `e2e/probes/vault_probe/tools.py`.
#[derive(Default)]
pub struct VaultProbe {
    /// Successful opens, PER AGENT ID. THE SCORE.
    opened: PerAgent<u64>,
}

pub const VAULT_MODULE: &str = "vault_probe.tools";

const VAULT_SENTENCE: &str = "A vault. It holds one secret and opens only with the right code.";

const VAULT_OPEN_DOC: &str = "args: code:str (req). Opens the vault and returns {secret}.\n\n\
THE CODE: this vault's `serial` (its reflect shows it) written BACKWARDS and prefixed with `V-` \
— a vault with serial `48213` opens with `V-31284`. Any other code is refused `bad_arg`, and the \
vault stays shut.";

#[async_trait]
impl Bundle for VaultProbe {
    fn name(&self) -> &str {
        "vault_probe"
    }

    fn verb_docs(&self) -> Value {
        json!({
            "open": VAULT_OPEN_DOC,
            "count": "No args. How many times this vault has been opened. Returns {opened}.",
        })
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let meta = kernel
            .agents
            .get(agent_id)
            .map(|a| a.record().meta)
            .unwrap_or_default();
        let reply = match verb {
            "boot" | "shutdown" => Value::Null,
            "reflect" => json!({
                "id": agent_id.as_str(),
                "sentence": VAULT_SENTENCE,
                "serial": meta.get("serial").cloned().unwrap_or(Value::Null),
                "opened": self.opened.get(agent_id),
            }),
            "open" => {
                let serial = meta.get("serial").map(py_str).unwrap_or_default();
                let want = format!("V-{}", serial.chars().rev().collect::<String>());
                let code = payload.get("code").and_then(Value::as_str).map(str::trim);
                if code == Some(want.as_str()) {
                    self.opened.with(agent_id, |n| *n += 1);
                    json!({"secret": meta.get("secret").cloned().unwrap_or(Value::Null)})
                } else {
                    json!({
                        "error": "vault_probe: open: wrong code — the vault stays shut",
                        "reason": "bad_arg", "arg": "code",
                    })
                }
            }
            "count" => json!({"opened": self.opened.get(agent_id)}),
            other => json!({
                "error": format!("vault_probe: unknown type {}", py_repr(other)),
                "reason": "unhandled", "verb": other,
            }),
        };
        Ok(Some(reply))
    }
}
