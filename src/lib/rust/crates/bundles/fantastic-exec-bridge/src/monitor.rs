//! MONITORS — a filter on a running job's OUTPUT that tells an
//! agent when something it watches for appears: how a long run is stopped EARLY.
//! Python `exec_bridge/_monitor.py`, rule for rule.
//!
//! A job's output already flows through this bridge line by line, so a monitor POLLS
//! NOTHING: each line is matched as it arrives (`match`, a regex in python's `re`
//! dialect, searched; absent = every line), and a match is SENT to `notify` as a turn.
//! `every` throttles delivery — the first match goes at once, later ones are gathered
//! and delivered at most once per `every` seconds — and a timer exists only while
//! something waits. An idle monitor wakes never. `tail` caps the lines in one
//! delivery to the newest N.
//!
//! Each delivery is a call made by this bridge AS ITSELF (its `can_call` must name
//! `notify`, and `notify`'s `callable_by` this bridge), fire-and-forget as its own
//! task. A monitor IS a kernel task (its id is the `monitor_id`): `ps` lists it, kernel
//! `cancel` (or `unmonitor`) ends it, and it ends with its job — after delivering what
//! still waited. RAM only, like the job it watches.
//!
//! A DELIVERY THE TARGET REFUSES (the call gate: `unauthorized`; no such agent:
//! `unknown_agent`) ENDS the monitor — never a silent drop. The refusal is LOUD
//! three ways: a python `logger.warning` line naming monitor, job, target and
//! refusal; a `monitor_dead` event on this bridge's inbox (its watchers see it); and
//! the dead monitor stays in `monitors` with `status: "dead"` and the refusal as
//! `error` until `unmonitor` clears it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use fancy_regex::Regex;
use fantastic_bundle::{py_repr_value, py_str};
use fantastic_kernel::refusal::{bad, missing, refusal, with, MISSING_ARG, NOT_FOUND};
use fantastic_kernel::{AgentId, Item, Kernel, TaskId};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

/// Lines in one delivery when the caller names no `tail`.
pub const DEFAULT_TAIL: i64 = 100;

enum Msg {
    Line(String),
    End,
    /// The target refused a delivery: stop now, deliver nothing more.
    Dead,
}

/// What a refused delivery answers — the target will never take one.
const REFUSED: [&str; 2] = ["unauthorized", "unknown_agent"];

struct Mon {
    owner: AgentId,
    job_id: TaskId,
    notify: String,
    pattern: Option<String>,
    matcher: Option<Regex>,
    /// As the caller wrote it (an int stays an int), echoed by `monitor`/`monitors`.
    every: Value,
    tail: i64,
    label: Option<String>,
    tx: mpsc::UnboundedSender<Msg>,
    carry: String,
    delivered: u64,
    pending: usize,
    /// Set when `notify` refused a delivery: the monitor is dead.
    error: Option<String>,
}

/// What the monitor's task needs, fixed at its start.
struct Cfg {
    owner: AgentId,
    job_id: TaskId,
    notify: String,
    every: Option<f64>,
    tail: usize,
    label: Option<String>,
    caller: Option<String>,
    call: Option<i64>,
}

/// Keyed by (owner, monitor id): a monitor id is a task id, unique only within one
/// kernel, and the owner keeps two kernels in one process apart (as the job table's
/// key does).
type Key = (String, i64);

fn monitors() -> MutexGuard<'static, BTreeMap<Key, Mon>> {
    static M: OnceLock<Mutex<BTreeMap<Key, Mon>>> = OnceLock::new();
    M.get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn matches(m: &Mon, line: &str) -> bool {
    m.matcher
        .as_ref()
        .is_none_or(|r| r.is_match(line).unwrap_or(false))
}

/// One output line of the job: queued for every monitor whose `match` it meets.
pub(crate) fn feed_line(owner: &AgentId, job_id: TaskId, line: &str) {
    for m in monitors().values() {
        if &m.owner == owner && m.job_id == job_id && m.error.is_none() && matches(m, line) {
            let _ = m.tx.send(Msg::Line(line.to_string()));
        }
    }
}

/// A PTY chunk: split into lines (a partial line waits for its end).
pub(crate) fn feed_text(owner: &AgentId, job_id: TaskId, text: &str) {
    for m in monitors().values_mut() {
        if &m.owner != owner || m.job_id != job_id || m.error.is_some() {
            continue;
        }
        let buf = std::mem::take(&mut m.carry) + text;
        let mut parts: Vec<&str> = buf.split('\n').collect();
        m.carry = parts.pop().unwrap_or_default().to_string();
        for line in parts {
            let line = line.trim_end_matches('\r');
            if matches(m, line) {
                let _ = m.tx.send(Msg::Line(line.to_string()));
            }
        }
    }
}

/// The job is gone: each of its monitors delivers what waits, then ends.
pub(crate) fn job_ended(owner: &AgentId, job_id: TaskId) {
    for m in monitors().values_mut() {
        if &m.owner != owner || m.job_id != job_id || m.error.is_some() {
            continue;
        }
        let carry = std::mem::take(&mut m.carry);
        let line = carry.trim_end_matches('\r');
        if !carry.is_empty() && matches(m, line) {
            let _ = m.tx.send(Msg::Line(line.to_string()));
        }
        let _ = m.tx.send(Msg::End);
    }
}

/// Ends the job's monitors on EVERY way out of its pump — a return or an abort.
pub(crate) struct EndOnDrop(pub AgentId, pub TaskId);

impl Drop for EndOnDrop {
    fn drop(&mut self) {
        job_ended(&self.0, self.1);
    }
}

struct RemoveOnDrop(Key);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let mut g = monitors();
        // a DEAD monitor stays listed (its `error` says why) until `unmonitor`
        if g.get(&self.0).is_some_and(|m| m.error.is_none()) {
            g.remove(&self.0);
        }
    }
}

fn header(cfg: &Cfg, id: i64, ended: bool) -> String {
    let who = if cfg.caller.as_deref() == Some(cfg.notify.as_str()) {
        "your".to_string()
    } else {
        format!("{}'s", cfg.caller.as_deref().unwrap_or("the operator"))
    };
    let call = cfg
        .call
        .map_or_else(|| "None".to_string(), |c| c.to_string());
    let mut parts = vec![
        format!("monitor {id}"),
        format!("job {}", cfg.job_id.get()),
        format!("from {who} call {call}"),
    ];
    if let Some(l) = &cfg.label {
        parts.push(format!("\"{l}\""));
    }
    if ended {
        parts.push("job ended".to_string());
    }
    format!("[{}]", parts.join(" · "))
}

fn set_pending(key: &Key, n: usize) {
    if let Some(m) = monitors().get_mut(key) {
        m.pending = n;
    }
}

/// The monitor's task: gather what matched, deliver it on the throttle, end with the
/// job or a cancel. No timer while nothing waits.
async fn run(
    cfg: Cfg,
    id_rx: oneshot::Receiver<TaskId>,
    mut rx: mpsc::UnboundedReceiver<Msg>,
    kernel: Arc<Kernel>,
) {
    let Ok(id) = id_rx.await else { return };
    let id = id.get();
    let key: Key = (cfg.owner.0.clone(), id);
    let _gone = RemoveOnDrop(key.clone());
    let mut last: Option<Instant> = None;
    let mut pending: Vec<String> = Vec::new();
    let mut ended = false;
    let window_open = |last: Option<Instant>| {
        cfg.every
            .is_none_or(|e| last.is_none_or(|l| l.elapsed().as_secs_f64() >= e))
    };
    loop {
        if pending.is_empty() || !window_open(last) {
            let got = if pending.is_empty() {
                rx.recv().await.map(Some)
            } else {
                let left =
                    cfg.every.unwrap_or(0.0) - last.map_or(0.0, |l| l.elapsed().as_secs_f64());
                match tokio::time::timeout(Duration::from_secs_f64(left.max(0.0)), rx.recv()).await
                {
                    Ok(m) => m.map(Some),
                    Err(_) => Some(None),
                }
            };
            match got {
                None => ended = true,
                Some(None) => {}
                Some(Some(first)) => {
                    let mut dead = false;
                    let mut take = |m: Msg| match m {
                        Msg::Line(l) => pending.push(l),
                        Msg::End => ended = true,
                        Msg::Dead => dead = true,
                    };
                    take(first);
                    while let Ok(m) = rx.try_recv() {
                        take(m);
                    }
                    if dead {
                        return;
                    }
                }
            }
            set_pending(&key, pending.len());
        }
        if !pending.is_empty() && (ended || window_open(last)) {
            deliver(&cfg, &key, std::mem::take(&mut pending), ended, &kernel);
            last = Some(Instant::now());
            set_pending(&key, 0);
        }
        if ended {
            return;
        }
    }
}

fn deliver(cfg: &Cfg, key: &Key, lines: Vec<String>, ended: bool, kernel: &Arc<Kernel>) {
    let id = key.1;
    let dropped = lines.len().saturating_sub(cfg.tail);
    let mut body = lines[dropped..].join("\n");
    if dropped > 0 {
        body.push_str(&format!("\n(+{dropped} earlier lines dropped)"));
    }
    match monitors().get_mut(key) {
        // dead: the target refused an earlier delivery
        Some(m) if m.error.is_some() => return,
        Some(m) => m.delivered += 1,
        None => {}
    }
    let text = format!("{}\n{body}", header(cfg, id, ended));
    let k = Arc::clone(kernel);
    let target = AgentId::from(cfg.notify.as_str());
    let key = key.clone();
    let (owner, job_id, notify) = (cfg.owner.clone(), cfg.job_id.get(), cfg.notify.clone());
    kernel.spawn_task(&cfg.owner, "monitor_notify", async move {
        let out = k
            .send(vec![Item::new(
                target,
                json!({"type": "send", "text": text}),
            )])
            .await;
        let got = out
            .into_iter()
            .next()
            .map(|o| o.result)
            .unwrap_or(Value::Null);
        let reason = got.get("reason").and_then(Value::as_str).unwrap_or("");
        if !REFUSED.contains(&reason) {
            return;
        }
        let why = {
            let mut g = monitors();
            let Some(m) = g.get_mut(&key).filter(|m| m.error.is_none()) else {
                return;
            };
            let why = got
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or(reason)
                .to_string();
            m.error = Some(why.clone());
            m.delivered = m.delivered.saturating_sub(1);
            let _ = m.tx.send(Msg::Dead);
            why
        };
        refused(&owner, id, job_id, &notify, reason, &why, &k).await;
    });
}

/// The monitor just died of a refused delivery: say so — python `_refused`, a
/// stderr warning and a `monitor_dead` event on the bridge's inbox. The gate
/// decision stands; only the silence is removed.
async fn refused(
    owner: &AgentId,
    id: i64,
    job_id: i64,
    notify: &str,
    reason: &str,
    why: &str,
    kernel: &Arc<Kernel>,
) {
    log_line(format!(
        "exec_bridge {}: monitor {id} (job {job_id}) dead — {notify} refused the delivery: {why}",
        owner.0
    ));
    kernel
        .emit(
            owner,
            json!({
                "type": "monitor_dead",
                "monitor_id": id,
                "job_id": job_id,
                "notify": notify,
                "reason": reason,
                "error": why,
            }),
        )
        .await;
}

/// Lines a test reads back instead of stderr — `None` sends them to stderr.
pub(crate) static LOG_SINK: Mutex<Option<Vec<String>>> = Mutex::new(None);

/// One python `logging.lastResort` line (no handler is configured anywhere, so a
/// `logger.warning` prints the BARE message to stderr).
fn log_line(msg: String) {
    let mut g = LOG_SINK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match g.as_mut() {
        Some(buf) => buf.push(msg),
        None => eprintln!("{msg}"),
    }
}

/// `monitor {job_id, notify, match?, every?, tail?, label?}` — python `_monitor_verb`.
pub(crate) async fn monitor_reply(owner: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    let jid = match crate::require_job("monitor", payload) {
        Ok(j) => j,
        Err(e) => return e,
    };
    {
        let g = crate::jobs::jobs();
        match jid.find(&g, owner) {
            None => return crate::no_such_job(&jid),
            Some(j) if !crate::jobs::alive(j) => {
                return bad("monitor", "job_id", "a RUNNING job — this one has exited")
            }
            Some(_) => {}
        }
    }
    let job_id = jid.id.expect("found above");
    let notify = match payload.get("notify") {
        None | Some(Value::Null) => return missing("monitor", "notify"),
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        Some(_) => return bad("monitor", "notify", "an agent id"),
    };
    if kernel.agents.get(&AgentId::from(notify.as_str())).is_none() {
        let mut r = refusal(
            NOT_FOUND,
            format!(
                "monitor: no agent {} to notify",
                py_repr_value(&json!(notify))
            ),
        );
        with(&mut r, "arg", json!("notify"));
        return r;
    }
    let (pattern, matcher) = match payload.get("match") {
        None | Some(Value::Null) => (None, None),
        Some(Value::String(s)) if !s.is_empty() => match Regex::new(s) {
            Ok(r) => (Some(s.clone()), Some(r)),
            Err(e) => return bad("monitor", "match", &format!("a valid regex ({e})")),
        },
        Some(_) => return bad("monitor", "match", "a regex string"),
    };
    let every = match payload.get("every") {
        None | Some(Value::Null) => None,
        Some(v @ Value::Number(_)) if v.as_f64().is_some_and(|x| x > 0.0) => v.as_f64(),
        Some(_) => return bad("monitor", "every", "a positive number of seconds"),
    };
    if matcher.is_none() && every.is_none() {
        let mut r = refusal(
            MISSING_ARG,
            "monitor: every required — without `match` every line counts, so name the \
             window (`every`, seconds) its deliveries are gathered in",
        );
        with(&mut r, "arg", json!("every"));
        return r;
    }
    let tail = match payload.get("tail") {
        None => DEFAULT_TAIL,
        Some(Value::Number(n)) if n.as_i64().is_some_and(|t| t >= 1) && !n.is_f64() => {
            n.as_i64().unwrap_or(DEFAULT_TAIL)
        }
        Some(_) => return bad("monitor", "tail", "an int >= 1 (lines per delivery)"),
    };
    let label = match payload.get("label") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => (!s.is_empty()).then(|| s.clone()),
        Some(_) => return bad("monitor", "label", "a string"),
    };
    let (tx, rx) = mpsc::unbounded_channel();
    let (id_tx, id_rx) = oneshot::channel();
    let cfg = Cfg {
        owner: owner.clone(),
        job_id,
        notify: notify.clone(),
        every,
        tail: usize::try_from(tail).unwrap_or(usize::MAX),
        label: label.clone(),
        caller: fantastic_kernel::send::current_origin().map(|a| a.0),
        call: fantastic_kernel::current_task().map(|t| t.get()),
    };
    let mid = kernel.spawn_task(owner, "monitor", run(cfg, id_rx, rx, Arc::clone(kernel)));
    monitors().insert(
        (owner.0.clone(), mid.get()),
        Mon {
            owner: owner.clone(),
            job_id,
            notify: notify.clone(),
            pattern: pattern.clone(),
            matcher,
            every: payload.get("every").cloned().unwrap_or(Value::Null),
            tail,
            label: label.clone(),
            tx,
            carry: String::new(),
            delivered: 0,
            pending: 0,
            error: None,
        },
    );
    let _ = id_tx.send(mid);
    json!({
        "monitor_id": mid.get(),
        "job_id": job_id.get(),
        "notify": notify,
        "match": pattern,
        "every": payload.get("every").cloned().unwrap_or(Value::Null),
        "tail": tail,
        "label": label,
    })
}

/// `monitors` — this agent's monitors, a dead one with its `error`.
pub(crate) fn monitors_reply(owner: &AgentId) -> Value {
    let list: Vec<Value> = monitors()
        .iter()
        .filter(|(_, m)| &m.owner == owner)
        .map(|((_, id), m)| {
            json!({
                "monitor_id": id,
                "job_id": m.job_id.get(),
                "notify": m.notify,
                "match": m.pattern,
                "every": m.every.clone(),
                "tail": m.tail,
                "label": m.label,
                "delivered": m.delivered,
                "pending": m.pending,
                "status": if m.error.is_some() { "dead" } else { "running" },
                "error": m.error,
            })
        })
        .collect();
    json!({"monitors": list})
}

/// `unmonitor {monitor_id}` — end one of this agent's monitors.
pub(crate) async fn unmonitor_reply(
    owner: &AgentId,
    payload: &Value,
    kernel: &Arc<Kernel>,
) -> Value {
    let raw = match payload.get("monitor_id") {
        None | Some(Value::Null) => return missing("unmonitor", "monitor_id"),
        Some(v) => v.clone(),
    };
    let Some(mid) = raw.as_i64().filter(|_| raw.is_i64()) else {
        return bad(
            "unmonitor",
            "monitor_id",
            "an int — the id `monitor` returned",
        );
    };
    let key = (owner.0.clone(), mid);
    let dead = monitors().get(&key).map(|m| m.error.is_some());
    if dead == Some(true) {
        // its task is over, only the record is left
        monitors().remove(&key);
        return json!({"unmonitored": true, "monitor_id": mid});
    }
    if dead.is_none() {
        let mut r = refusal(
            NOT_FOUND,
            format!("exec_bridge: no monitor {}", py_str(&raw)),
        );
        with(&mut r, "monitor_id", raw);
        return r;
    }
    kernel.cancel_task(&TaskId(mid)).await;
    json!({"unmonitored": true, "monitor_id": mid})
}
