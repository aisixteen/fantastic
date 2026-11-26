//! The Context-Protocol seam: shape the assembled messages to fit the
//! agent's token budget via its configured `context_strategy`, prepend the
//! ONE canonical `[context-notice]`, and push a `context` event to the
//! caller. The `too_small` failsafe is a failfast (model NOT called) that
//! also pushes a `context:too_small` event — NOT a fallback. The durable
//! store is untouched (the notice lives only in the model view). Mirrors the
//! Python `ai_core/core.py` `_project_context` / `_context_notice`.

use crate::constants::{BUDGET_FLOOR, NOTICE_ENVELOPE_RESERVE};
use crate::context::{budget as agent_budget, recent_n as cfg_recent_n, resolve_context_window};
use crate::events::to_caller;
use crate::helpers::{agent_meta, now_secs};
use crate::memory::MemoryStoreError;
use crate::model_format::pass::{Pass, Pending};
use crate::state::BackendState;
use crate::strategies::{
    compact, fold::fold, is_known_strategy, strip::strip, truncate, ProjectionCtx,
};
use crate::view::res_of;
use fantastic_bundle::py_repr_value;
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};
use std::sync::Arc;

pub use crate::constants::{RECALL_EVERY, TRIGGER};

/// `round(x, 3)` — how python reports the calibration.
#[must_use]
pub fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

/// Why a projection produced no view.
#[derive(Debug)]
pub enum ProjectFail {
    /// The `too_small` failsafe or an unknown-strategy config error — the dict IS
    /// the reply.
    Refusal(Value),
    /// The memory store refused a read or a write the projection needed.
    Store(MemoryStoreError),
    /// A fold's summary would not fit its budget after every re-summarise — the
    /// turn fails loud (python `FoldSummaryError`).
    FoldSummary(crate::strategies::fold::FoldSummaryError),
    /// A fold's anchor cannot be known — the turn fails loud (python
    /// `member_anchor`'s `ValueError`).
    FoldAnchor(crate::folds::FoldAnchorError),
    /// A summary's task answer has not come yet (#795): the projection's step sends
    /// its request and runs the projection again ([`project_context`]).
    Pending,
}

impl From<Pending> for ProjectFail {
    fn from(_: Pending) -> Self {
        Self::Pending
    }
}

impl From<MemoryStoreError> for ProjectFail {
    fn from(e: MemoryStoreError) -> Self {
        Self::Store(e)
    }
}

impl From<crate::strategies::fold::FoldFail> for ProjectFail {
    fn from(e: crate::strategies::fold::FoldFail) -> Self {
        match e {
            crate::strategies::fold::FoldFail::Store(e) => Self::Store(e),
            crate::strategies::fold::FoldFail::Summary(e) => Self::FoldSummary(e),
            crate::strategies::fold::FoldFail::Anchor(e) => Self::FoldAnchor(e),
            crate::strategies::fold::FoldFail::Pending => Self::Pending,
        }
    }
}

/// The ONE canonical inbound context-notice — composed at the SEAM from a
/// strategy's projection artifact. A `role:user` turn (the role every backend
/// reliably attends to). Carries the protocol affordances: `recall` to page
/// dropped turns back, and persist-to-memory. Model view ONLY — never the store.
pub fn context_notice(
    strategy: &str,
    summary: Option<&str>,
    omitted_marker: bool,
    dropped_n: usize,
    stripped: usize,
    stripped_bytes: i64,
) -> Value {
    // its words are the type's (`res/<type>/notice_tail.txt`, `res/shared/notice_*.txt`),
    // written by the map: the default type's are python's bytes
    crate::model_format::map::with_pieces(
        json!({"role": "user"}),
        &[crate::model_format::map::Piece::Notice {
            strategy: strategy.to_string(),
            dropped: dropped_n,
            summary: summary.map(str::to_string),
            omitted: omitted_marker,
            stripped,
            stripped_bytes,
        }],
    )
}

/// What the projection's own reckoning gives ([`fit`]): the messages as they are (it did
/// not fire), the `too_small` refusal with the event that says so, or the shaped body
/// whose view row is written next.
enum Fitted {
    Unfired(Vec<Value>),
    TooSmall { event: Value, refusal: Value },
    Shaped(Shaped),
}

/// A projection that fired, before its view row is written.
struct Shaped {
    system_block: Vec<Value>,
    notice: Value,
    out_body: Vec<Value>,
    proj: crate::strategies::Projection,
    strat_name: String,
    budget: i64,
    calib: f64,
    body_len: usize,
}

/// Who a projection is for, owned, so its steps carry it.
#[derive(Clone)]
struct For {
    self_id: AgentId,
    state: Arc<BackendState>,
    kernel: Arc<Kernel>,
    client_id: String,
    name: &'static str,
    model_window: i64,
}

/// Shape `messages` to fit the budget, prepend the notice, emit the `context`
/// event — as STEPS (#795): the head read (a strip's seed is the stored view), the
/// strategy's reckoning with its summaries asked as task requests
/// ([`crate::model_format::pass::drive`]), then the view row written; `next` gets the
/// pass back and `Ok(projected_model_messages)`, or the `too_small` failsafe / an
/// unknown-strategy config error / a store or fold failure. Sets the public
/// `state.projection` summary + the private `state.compaction_mark`.
///
/// `store_len` is the DURABLE store's length behind `messages` (from
/// [`crate::assembly::assemble_messages`]) — the index the reaction cursor is
/// recorded at. It is NOT derivable from `messages`, which is a deduped/trimmed
/// view.
///
/// `pass`: the chat's pass path; a strategy that summarizes makes a task pass on it.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn project_context<R, F, Fut>(
    pass: Pass,
    self_id: &AgentId,
    state: &Arc<BackendState>,
    kernel: &Arc<Kernel>,
    client_id: &str,
    name: &'static str,
    model_window: i64,
    messages: Vec<Value>,
    store_len: usize,
    rec: R,
    next: F,
) where
    R: Send + 'static,
    F: FnOnce(R, Pass, Result<Vec<Value>, ProjectFail>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let who = For {
        self_id: self_id.clone(),
        state: Arc::clone(state),
        kernel: Arc::clone(kernel),
        client_id: client_id.to_string(),
        name,
        model_window,
    };
    // A STRIP IS SEEDED FROM THE STORED VIEW: the head is read first (a step)
    let strat = agent_meta(self_id, kernel)
        .get("context_strategy")
        .and_then(Value::as_str)
        .map(str::to_string);
    if matches!(strat.as_deref(), Some("strip" | "fold")) {
        crate::steps::head(self_id, kernel, (pass, who, messages, rec, next), move |(pass, who, messages, rec, next), r| async move {
            if let Err(e) = r {
                return next(rec, pass, Err(ProjectFail::Store(e))).await;
            }
            reckon(pass, who, messages, store_len, rec, next).await;
        })
        .await;
    } else {
        reckon(pass, who, messages, store_len, rec, next).await;
    }
}

/// The strategy's reckoning run to its end (its summaries asked as task requests),
/// then the projection's end ([`ended`]).
async fn reckon<R, F, Fut>(pass: Pass, who: For, messages: Vec<Value>, store_len: usize, rec: R, next: F)
where
    R: Send + 'static,
    F: FnOnce(R, Pass, Result<Vec<Value>, ProjectFail>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let (k, me) = (Arc::clone(&who.kernel), who.self_id.clone());
    let w = who.clone();
    let helper = move |pass: &Pass, _: &mut (R, F)| match fit(pass, &w, &messages) {
        Err(ProjectFail::Pending) => Err(Pending),
        other => Ok(other),
    };
    crate::model_format::pass::drive(k, me, pass, (rec, next), helper, move |(rec, next), pass, fitted| async move {
        ended(pass, who, fitted, store_len, rec, next).await;
    })
    .await;
}

/// The projection's end: a refusal is said, a shaped body's view row is written (a
/// step) and its state and event follow.
async fn ended<R, F, Fut>(
    pass: Pass,
    who: For,
    fitted: Result<Fitted, ProjectFail>,
    store_len: usize,
    rec: R,
    next: F,
) where
    R: Send + 'static,
    F: FnOnce(R, Pass, Result<Vec<Value>, ProjectFail>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let sh = match fitted {
        Err(e) => return next(rec, pass, Err(e)).await,
        Ok(Fitted::Unfired(m)) => return next(rec, pass, Ok(m)).await,
        Ok(Fitted::TooSmall { event, refusal }) => {
            to_caller(&who.kernel, &who.self_id, &who.client_id, event).await;
            return next(rec, pass, Err(ProjectFail::Refusal(refusal))).await;
        }
        Ok(Fitted::Shaped(sh)) => sh,
    };
    // THE VIEW ARRAY, written by every projection: what the model sees, at what
    // resolution — so the next turn (and the next process) rebuilds the same bytes.
    // The fold rows this projection made go in the same append, before it (#766 RS5).
    // `projection.view_ids` below is read after it, so it names the minted ids.
    let ids: Vec<Value> = sh
        .out_body
        .iter()
        .filter_map(|m| {
            m.get("id")
                .and_then(Value::as_i64)
                .map(|id| json!({"id": id, "res": res_of(m)}))
        })
        .collect();
    let why = format!("projection: {}", sh.strat_name);
    let (me, k) = (who.self_id.clone(), Arc::clone(&who.kernel));
    let folds = sh.proj.folds.clone();
    crate::steps::save_view(&me, &k, &folds, &ids, sh.budget, &why, (pass, who, sh, rec, next), move |(pass, who, sh, rec, next), minted| async move {
        match minted {
            Ok(minted) => {
                let out = shown(&who, sh, &minted, store_len).await;
                next(rec, pass, Ok(out)).await;
            }
            Err(e) => next(rec, pass, Err(ProjectFail::Store(e))).await,
        }
    })
    .await;
}

/// THE STRATEGY'S RECKONING, plain code over the pass's book (#795): the budget, the
/// failsafe, the strategy, the final fit. [`ProjectFail::Pending`]: a summary's task
/// answer has not come yet.
fn fit(pass: &Pass, who: &For, messages: &[Value]) -> Result<Fitted, ProjectFail> {
    let (self_id, state, kernel, client_id, name, model_window) = (
        &who.self_id,
        &who.state,
        &who.kernel,
        who.client_id.as_str(),
        who.name,
        who.model_window,
    );
    let messages = messages.to_vec();
    let meta = agent_meta(self_id, kernel);
    // EVERY SIZE IS OF THE BYTES THIS PASS'S TYPE IS SENT (`Pass::tokens`)
    let size = |m: &Value| pass.tokens(m);
    let total = |ms: &[Value]| pass.tokens_all(ms);
    // THE BUDGET IN ESTIMATE UNITS: the record's budget, divided by how much the
    // endpoint's own count exceeded our estimate last turn (`calib`).
    let calib = state.calib.lock().expect("calib poisoned").unwrap_or(1.0);
    let b = if calib > 0.0 {
        ((agent_budget(&meta, model_window) as f64 / calib) as i64).max(BUDGET_FLOOR)
    } else {
        agent_budget(&meta, model_window)
    };
    if total(&messages) <= (b as f64 * TRIGGER) as i64 {
        *state.projection.lock().expect("projection poisoned") = Some(json!({
            "fired": false,
            "trigger": TRIGGER,
            "calibration": round3(calib),
        }));
        return Ok(Fitted::Unfired(messages));
    }
    let system_block: Vec<Value> = messages.iter().take(1).cloned().collect();
    let body: Vec<Value> = messages.iter().skip(1).cloned().collect();
    if body.is_empty() {
        *state.projection.lock().expect("projection poisoned") = Some(json!({"fired": false}));
        return Ok(Fitted::Unfired(messages));
    }
    let sys_tokens = total(&system_block);
    let body_budget = b - sys_tokens;
    let last_cost = size(&body[body.len() - 1]);
    // The live user turn AND the notice envelope are both non-negotiable — if
    // there's no room for BOTH, fail loud (so the trim below never drops the live turn).
    if body_budget < last_cost + NOTICE_ENVELOPE_RESERVE {
        let window = resolve_context_window(&meta, model_window);
        let hint = format!(
            "the system prompt ({sys_tokens} tok) leaves no room in the {window}-token \
             window for even one turn; reduce agents/menu or raise context_window"
        );
        *state.projection.lock().expect("projection poisoned") =
            Some(json!({"fired": false, "too_small": true}));
        *state.compaction_mark.lock().expect("mark poisoned") = None;
        let event = json!({
                "type": "context", "source": self_id.as_str(), "ts": now_secs(),
                "phase": "too_small",
                "detail": {
                    "context_window": window, "system_tokens": sys_tokens,
                    // WHAT IT COULD NOT FIT — python's detail. A probe reading
                    // `too_small` off the wire needs the size and the head of
                    // the turn that did not fit, not only the window.
                    "last_message_tokens": last_cost,
                    "last_message_head": body[body.len() - 1].get("content")
                        .map(|c| match c.as_str() {
                            Some(s) => s.chars().take(200).collect::<String>(),
                            None => c.to_string().chars().take(200).collect::<String>(),
                        }),
                    "hint": hint,
                },
            });
        return Ok(Fitted::TooSmall {
            event,
            refusal: json!({"error": format!("{name}: context_insufficient — {hint}")}),
        });
    }
    // RAW, as python's `rec.get("context_strategy", DEFAULT_STRATEGY)`: only an
    // ABSENT key means `compact`. `as_str().unwrap_or("compact")` was a fallback
    // that ran compact for `null`, `5` and `[]` — python refuses all three.
    let strat_value = meta
        .get("context_strategy")
        .cloned()
        .unwrap_or_else(|| json!(crate::strategies::DEFAULT_STRATEGY));
    let Some(strat_name) = strat_value.as_str().filter(|n| is_known_strategy(n)) else {
        // python `f"... {strat_name!r} (valid: {strategy_names()})"` — the repr of
        // the VALUE, and `str(list)` of the registry's names.
        return Err(ProjectFail::Refusal(json!({
            "error": format!(
                "{name}: unknown context_strategy {} (valid: {})",
                py_repr_value(&strat_value),
                py_repr_value(&json!(crate::strategies::STRATEGY_NAMES)),
            ),
        })));
    };
    let strat_name = strat_name.to_string();
    // `none`: never shrink. An over-budget turn is refused here, before any
    // model call or row write, and nothing is dropped.
    if strat_name == "none" {
        return Err(ProjectFail::Refusal(json!({
            "error": format!(
                "{name}: context full: {}k of {}k tokens — clear, fold by hand, or raise context_window",
                total(&messages) / 1000,
                b / 1000
            ),
        })));
    }
    let recent = cfg_recent_n(&meta);
    let pctx = ProjectionCtx {
        budget: body_budget,
        recent_n: recent,
        pass,
        self_id,
        kernel,
        client_id,
    };
    let mut proj = match strat_name.as_str() {
        "truncate" => truncate(&body, body_budget, &size),
        "strip" => strip(&body, &pctx),
        "fold" => fold(&body, &pctx)?,
        _ => compact(&body, recent, body_budget, pass)?,
    };
    let dropped_pre = body.len().saturating_sub(proj.body.len());
    let notice = context_notice(
        &strat_name,
        proj.summary.as_deref(),
        proj.omitted_marker,
        dropped_pre,
        proj.stripped,
        proj.stripped_bytes,
    );
    // Single budget authority: the strategy reserved the envelope, but guard the
    // final fit anyway — trim oldest body turns (tool-pairing-safe). Never drop the
    // last (live) turn: the failsafe above guarantees room for [notice + live turn].
    // TRIM FROM THE MIDDLE, NOT THE FRONT. `out_body[0]` is the goal turn every
    // strategy deliberately pins (`compact`/`truncate` both put it first) and the
    // last entry is the live turn. Taking from the front silently undoes the pin —
    // with a snug window the goal sentence would be absent from the projection
    // entirely, in both strategies.
    let mut out_body = std::mem::take(&mut proj.body);
    let over = |body: &[Value]| -> bool {
        let mut probe = system_block.clone();
        probe.push(notice.clone());
        probe.extend(body.iter().cloned());
        total(&probe) > b
    };
    while out_body.len() > 2 && over(&out_body) {
        let mut next = vec![out_body[0].clone()];
        next.extend(out_body[2..].iter().cloned());
        out_body = next;
    }
    // Last resort, and only when nothing else is left to give: [goal, live] still
    // over budget. The live turn is what the model must answer, so the goal is
    // what goes — loudly, in the notice's own count, not silently.
    while out_body.len() > 1 && over(&out_body) {
        out_body = out_body[1..].to_vec();
    }
    Ok(Fitted::Shaped(Shaped {
        system_block,
        notice,
        out_body,
        proj,
        strat_name,
        budget: b,
        calib,
        body_len: body.len(),
    }))
}

/// A shaped projection whose view row is written: the minted fold ids shown, the
/// state kept, the caller told; the messages of the request.
async fn shown(
    who: &For,
    sh: Shaped,
    minted: &std::collections::BTreeMap<i64, i64>,
    store_len: usize,
) -> Vec<Value> {
    let (self_id, state, kernel, client_id) = (&who.self_id, &who.state, &who.kernel, who.client_id.as_str());
    let Shaped {
        system_block,
        notice,
        mut out_body,
        proj,
        strat_name,
        budget: _,
        calib,
        body_len,
    } = sh;
    // the folds' ids were minted in that append (#766, fold id order): this request
    // shows the ids the store holds
    for m in &mut out_body {
        crate::folds::remap_ids(m, minted);
    }
    let dropped_n = body_len.saturating_sub(out_body.len());
    let summarized = proj.summary.is_some();
    let reported = *state.reported.lock().expect("reported poisoned");
    *state.projection.lock().expect("projection poisoned") = Some(json!({
        "fired": true,
        "trigger": TRIGGER,
        "calibration": round3(calib),
        "reported_prompt_tokens": reported,
        "strategy": strat_name,
        "kept_messages": out_body.len(),
        "dropped_messages": dropped_n,
        "summarized": summarized,
        // ALWAYS 0 here: this runtime has no pinned-facts block, and python's key
        // is in the projection a model reads off `context_status`. An absent key
        // is a different reply from a zero one.
        "pinned_facts": 0,
        "stripped": proj.stripped,
        "stripped_bytes": proj.stripped_bytes,
        "folded": proj.folds.len(),
        "unanchored": proj.unanchored,
        // What the model actually sees this turn — so "after reload it sees only
        // the fold" is measured from the view, never self-reported.
        "view_ids": out_body.iter().filter_map(|m| m.get("id").and_then(Value::as_i64))
            .take(200).collect::<Vec<_>>(),
        "view_ids_truncated":
            out_body.iter().filter(|m| m.get("id").is_some()).count() > 200,
    }));
    // `store_len`, NOT `body.len() - 1`. `body` is the DEDUPED/trimmed model view
    // and `derive_reaction` slices the FULL durable store by this index; whenever
    // the trimmers dropped whole turns the index came up short by that many, so the
    // reader scanned an assistant turn written BEFORE the notice and reported a
    // reaction to a compaction the model had not yet seen.
    *state.compaction_mark.lock().expect("mark poisoned") =
        Some((store_len, client_id.to_string()));
    // a compaction reshapes the head: the next request's parts are a checkpoint
    *state.prev_request.lock().expect("prev_request poisoned") = None;
    if !proj.folds.is_empty() {
        // "When it feels it folded, it runs context_recall" — at the start of
        // the next turn, over the new head. DEBOUNCED: every RECALL_EVERY-th
        // batch (python: 11 folds, 5 recalls in 60 turns before the debounce).
        let mut since = state.folds_since.lock().expect("folds_since poisoned");
        *since += proj.folds.len();
        if *since >= RECALL_EVERY {
            *since = 0;
            *state.recall_due.lock().expect("recall_due poisoned") = true;
        }
    }
    to_caller(
        kernel,
        self_id,
        client_id,
        json!({
            "type": "context", "source": self_id.as_str(), "ts": now_secs(),
            "phase": "compacted",
            "detail": {
                "strategy": strat_name, "dropped_messages": dropped_n,
                "kept_messages": out_body.len(), "summarized": summarized,
                // WHAT THE STRATEGY DID, on the event a probe scores from —
                // python's detail. Without these the fold arm reads `folded=0`
                // with fold levels in its store.
                "pinned_facts": 0,
                "stripped": proj.stripped,
                "stripped_bytes": proj.stripped_bytes,
                "folded": proj.folds.len(),
                "unanchored": proj.unanchored,
            },
        }),
    )
    .await;
    let mut out = system_block;
    out.push(notice);
    out.extend(out_body);
    out
}

/// Read-model over the durable transcript: AFTER the last compaction notice
/// (its cursor), did the model react? Scans the same client's thread for
/// `send` tool-calls — a `recall` to its OWN id, or a memory write
/// (`write`/`patch`/`append`). The reaction IS the 'ack'; this derives it, and the
/// next step gets it. `None` if no compaction has fired.
pub async fn derive_reaction<R, F, Fut>(
    self_id: &AgentId,
    state: &Arc<BackendState>,
    kernel: &Arc<Kernel>,
    rec: R,
    next: F,
) where
    R: Send + 'static,
    F: FnOnce(R, Result<Option<Value>, MemoryStoreError>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let fired = state
        .projection
        .lock()
        .expect("projection poisoned")
        .as_ref()
        .and_then(|p| p.get("fired").and_then(Value::as_bool))
        .unwrap_or(false);
    let mark = state.compaction_mark.lock().expect("mark poisoned").clone();
    let (idx, _client_id) = match (fired, mark) {
        (true, Some(m)) => m,
        _ => return next(rec, Ok(None)).await,
    };
    // the durable turns, read by a step (#795)
    let me = self_id.as_str().to_string();
    crate::steps::history(self_id, kernel, rec, move |rec, store| async move {
        let r = store.map(|store| Some(reaction_from(&me, store.get(idx..).unwrap_or_default())));
        next(rec, r).await;
    })
    .await;
}

/// The reaction read itself, over the durable turns AFTER the notice cursor: did
/// the model `recall` to its OWN id, or write to memory?
///
/// THE MEMORY VERBS are a file_bridge's MUTATING text verbs (`write`/`edit`/
/// `append`) — a model persists what it must keep as plain files.
///
/// Pure, so the SHAPES it recognizes can be pinned. It reads exactly what
/// dispatch reads: an UNTERMINATED envelope is no call, so it
/// is no reaction either.
pub fn reaction_from(self_id: &str, turns: &[Value]) -> Value {
    let mut recalled = false;
    let mut persisted = false;
    let mut recall_count: u64 = 0;
    for m in turns {
        if m.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        // Tool calls live as envelope TEXT inside the assistant content — extract
        // them with the shared parser, which is the one authority on the shapes.
        let content = m.get("content").and_then(Value::as_str).unwrap_or("");
        for call in crate::tool_parse::extract_tool_calls(content) {
            let args = &call.arguments;
            let target = args.get("target_id").and_then(Value::as_str);
            let ptype = args
                .get("payload")
                .and_then(|p| p.get("type"))
                .and_then(Value::as_str);
            if target == Some(self_id) && ptype == Some("recall") {
                recalled = true;
                recall_count += 1;
            } else if matches!(ptype, Some("write") | Some("edit") | Some("append")) {
                persisted = true;
            }
        }
    }
    let mut out = Map::new();
    out.insert("recalled".to_string(), json!(recalled));
    out.insert("persisted".to_string(), json!(persisted));
    out.insert("recall_count".to_string(), json!(recall_count));
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::reaction_from;
    use serde_json::json;

    fn turn(content: &str) -> serde_json::Value {
        json!({"role": "assistant", "content": content})
    }

    #[test]
    fn the_reaction_reader_sees_what_dispatch_saw() {
        // AN UNTERMINATED ENVELOPE IS NOT A CALL: dispatch
        // skips it, and the reaction reader must see the same thing dispatch saw.
        let turns = [
            turn(
                r#"<call name="send"><routing><target_id>me</target_id></routing><payload><type>recall</type></payload>"#,
            ),
            turn(
                r#"<call name="send"><routing><target_id>notes</target_id></routing><payload><type>write</type><path>facts.md</path><bytes>x</bytes></payload></call>"#,
            ),
        ];
        let r = reaction_from("me", &turns);
        assert_eq!(
            r["recalled"], false,
            "an unterminated recall is no call: {r}"
        );
        assert_eq!(r["recall_count"], 0);
        assert_eq!(r["persisted"], true, "the write must count: {r}");
    }

    #[test]
    fn a_turn_with_no_call_is_no_reaction() {
        let turns = [
            turn("I have finished."),
            json!({"role": "user", "content": "go"}),
        ];
        let r = reaction_from("me", &turns);
        assert_eq!(r["recalled"], false);
        assert_eq!(r["persisted"], false);
        assert_eq!(r["recall_count"], 0);
    }

    #[test]
    fn a_recall_addressed_elsewhere_is_not_this_agents_reaction() {
        let turns = [turn(
            r#"<call name="send"><routing><target_id>other</target_id></routing><payload><type>recall</type></payload></call>"#,
        )];
        let r = reaction_from("me", &turns);
        assert_eq!(r["recalled"], false, "{r}");
    }
}
