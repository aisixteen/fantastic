//! vLLM LLM bundle — talks to a vLLM server DIRECTLY.
//!
//! Mirrors python's `vllm_backend` (`provider.py` + `tools.py`), which is the
//! canonical reference: every default, refusal text and reflect key here is
//! python's. It speaks vLLM's own OpenAI-compatible surface —
//! `POST {endpoint}/chat/completions` — with no proxy and no protocol
//! translation in between. NO native tool-calling: the provider streams raw
//! text and ai-core parses the `<call name=…>` envelope out of it.
//!
//! What is vLLM-specific, and why each piece exists (as measured on a production
//! deployment of Qwen3.8-27B-FP8 on vLLM 0.27.1):
//!
//! - THE THOUGHT CHANNEL. With `--reasoning-parser qwen3` the thought streams
//!   as `delta.reasoning_content` (some versions: `delta.reasoning`) and only
//!   the answer as `delta.content`. Reading `content` alone drops whole turns.
//! - A DEAD TURN — all thinking, no answer — is the worst outcome in an agent
//!   loop. `thinking_token_budget` is vLLM's hard stop for it: once spent, vLLM
//!   injects the end-of-reasoning token and the model continues into the
//!   answer. It must be BELOW `max_tokens` or it never fires.
//! - ONE system message, at index 0 — Qwen's chat template raises "System
//!   message must be at the beginning" otherwise.
//! - A stream that ends with NO `finish_reason` means vLLM died mid-answer;
//!   that is an error, never a completion.
//! - The KV-cache SESSION TABLE ([`SessionTable`]): vLLM's prefix cache makes a
//!   warm conversation answer ~6.5× faster; past the server's KV capacity it
//!   preempts and a preempted conversation re-prefills from scratch. The table
//!   keeps the number of LIVE conversations under that ceiling and rotates the
//!   least-recently-used one out.
//! - THE KEY, when the door needs one (an HTTPS door in front of a GPU server
//!   takes `Authorization: Bearer <key>`). Read before EACH
//!   request from the record's `credentials` ref in a credential store
//!   (`fantastic_ai_core::credentials`), never the environment or the record; a
//!   record with no `credentials` sends no header at all, so a local vLLM
//!   started without `--api-key` keeps working. A 401 or 403 says to name
//!   `credentials`.

use async_trait::async_trait;
use fantastic_ai_core::credentials::{key_reader, KeySource};
use fantastic_ai_core::provider::{
    Provider, ProviderError, ProviderEvent, ProviderReport, ProviderStream, ReportCell, Stalled,
    Unconfigured,
};
use fantastic_ai_core::schema::{self, Field};
use fantastic_ai_core::think::{decode_openai_delta, ThinkWrap};
use fantastic_ai_core::{agent_loop::BackendConfig, caching, helpers, verbs};
use fantastic_bundle::{py_float, py_int, py_repr_value, py_str, py_truthy};
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel, TaskId};
use futures_util::StreamExt;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// The handler module id a record names to select this bundle.
pub const HANDLER_MODULE: &str = "vllm_backend.tools";

/// The bundle readme — python's, byte for byte.
pub const README: &str = include_str!("readme.md");

/// The name every refusal carries.
const NAME: &str = "vllm_backend";

pub mod server;

/// What `reflect` says this agent IS — python `tools.py`'s `sentence`.
pub const SENTENCE: &str = "vLLM-backed LLM agent, talking to the vLLM server directly \
(raw prompt-and-parse tool-calling; a KV-cache session table per endpoint).";

const CFG: BackendConfig = BackendConfig {
    name: NAME,
    // A server's window is the record's to name (`context_window`); the kernel
    // cannot read it.
    model_window: fantastic_ai_core::constants::DEFAULT_CONTEXT_WINDOW,
    provider_cache: false,
};

// ─── sampling ───────────────────────────────────────────────────────────────

/// Applied ONLY where the record sets nothing (a production deployment's
/// agentic defaults), in python's key order — the order
/// reflect's `sampling` and the request body list them in.
///
/// `reasoning_effort` is a CHAT-TEMPLATE input (a soft steer on how much the
/// model wants to think); `thinking_token_budget` is the HARD limit, which a
/// medium-effort agent turn (~900 thinking tokens) only reaches on a runaway.
fn sampling_defaults() -> [(&'static str, Value); 5] {
    [
        ("temperature", json!(0.2)),
        ("top_p", json!(0.8)),
        ("top_k", json!(20)),
        ("reasoning_effort", json!("medium")),
        ("thinking_token_budget", json!(16000)),
    ]
}

/// vLLM accepts exactly these; "high" answers 400, so it is refused at write.
pub const REASONING_EFFORTS: [&str; 3] = ["low", "medium", "xhigh"];

/// Below this, NO thinking budget is sent: a handful of tokens of thinking is
/// no thinking, and a model starved of it stops with an empty answer.
pub const BUDGET_FLOOR: i64 = 256;

/// Sent only when set on the record — never defaulted. Penalties especially:
/// `presence_penalty` 0.7 caused looping and `frequency_penalty` collapsed the
/// output into sub-word garbage at ~31k chars; both 0 stayed clean to ~87k.
pub const PASSTHROUGH: [&str; 7] = [
    "min_p",
    "presence_penalty",
    "frequency_penalty",
    "repetition_penalty",
    "no_repeat_ngram_size",
    "seed",
    "stop",
];

// ─── connection ─────────────────────────────────────────────────────────────

/// TWO timeouts on purpose. A stopped instance drops SYNs silently, so one long
/// timeout would hang a caller for the whole read deadline; connect fails fast.
/// Read is long because a max-length generation decodes for many minutes.
/// Both are that deployment's values; each is a record field.
pub const CONNECT_TIMEOUT: f64 = 5.0;
pub const READ_TIMEOUT: f64 = 900.0;

// ─── the KV-cache session table ─────────────────────────────────────────────

/// Seconds a conversation may sit idle before its slot frees itself.
pub const KV_TTL: f64 = 1800.0;
pub const KV_OVERFLOWS: [&str; 2] = ["evict", "reject"];

/// The LIVE conversations on ONE vLLM endpoint, least-recently used first.
///
/// Shared by every vllm_backend agent in this kernel that names the same
/// endpoint (one table per endpoint, the closest a kernel
/// gets to "one count per GPU"). It is BOOKKEEPING, not memory management:
/// vLLM owns its prefix cache and the table frees no GPU memory — a
/// conversation rotated out here that comes back still hits whatever blocks
/// vLLM kept. What the table does is keep the conversations this kernel drives
/// at once under the server's KV ceiling, so vLLM never has to preempt one of
/// ours. It sees only THIS kernel's traffic, so it is a local policy under the
/// real ceiling.
///
/// Each admission uses the ADMITTING agent's own `kv_slots`/`kv_overflow`/
/// `kv_ttl` against the shared live set.
#[derive(Debug, Default)]
pub struct SessionTable {
    /// `(conversation key, last use)`, least-recently used FIRST — a Vec
    /// because the live set is a handful of entries and order is the point.
    live: Vec<(String, f64)>,
    evicted: u64,
    rejected: u64,
}

impl SessionTable {
    /// Mark `key` live and most-recently used. `Ok(Some(key))` names the
    /// conversation ROTATED OUT to make room (`evict`); `Err` is python's
    /// `KvSlotsFull` (`reject`).
    ///
    /// # Errors
    /// `reject` and every slot is live.
    pub fn admit(
        &mut self,
        key: &str,
        slots: i64,
        overflow: &str,
        ttl: f64,
        now: f64,
    ) -> Result<Option<String>, ProviderError> {
        // idle past its TTL: the slot frees itself
        self.live.retain(|(_, last)| now - last <= ttl);
        if let Some(pos) = self.live.iter().position(|(k, _)| k == key) {
            self.live.remove(pos);
            self.live.push((key.to_string(), now));
            return Ok(None);
        }
        let mut rotated = None;
        if i64::try_from(self.live.len()).unwrap_or(i64::MAX) >= slots {
            if overflow == "reject" {
                self.rejected += 1;
                // the soonest a slot frees on its own: the idlest one's TTL
                let idlest = self.live.first().map_or(now, |(_, last)| *last);
                let retry_after = (ttl - (now - idlest)).max(0.0);
                return Err(ProviderError {
                    message: format!(
                        "{NAME}: all {slots} KV slots on this endpoint are live \
                         (kv_overflow=reject) — retry in ~{retry_after:.0}s, or set \
                         kv_overflow=evict to rotate the least-recently-used out"
                    ),
                    // python raises its own `KvSlotsFull(RuntimeError)`, and a
                    // raise renders as `<class name>: <message>`.
                    kind: "KvSlotsFull",
                    transport: false,
                });
            }
            if !self.live.is_empty() {
                rotated = Some(self.live.remove(0).0);
                self.evicted += 1;
            }
        }
        self.live.push((key.to_string(), now));
        Ok(rotated)
    }

    /// `{live, conversations (least-recently used first), evicted, rejected}`.
    #[must_use]
    pub fn view(&self) -> Map<String, Value> {
        let mut out = Map::new();
        out.insert("live".into(), json!(self.live.len()));
        out.insert(
            "conversations".into(),
            json!(self.live.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>()),
        );
        out.insert("evicted".into(), json!(self.evicted));
        out.insert("rejected".into(), json!(self.rejected));
        out
    }
}

/// endpoint (normalised) → its table. Process-global, like every per-kernel
/// map in these bundles (one kernel per process).
fn tables() -> &'static Mutex<HashMap<String, SessionTable>> {
    static TABLES: OnceLock<Mutex<HashMap<String, SessionTable>>> = OnceLock::new();
    TABLES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Run `f` on `endpoint`'s table, creating it on first use (python
/// `table_for`, a `setdefault`).
fn with_table<T>(endpoint: &str, f: impl FnOnce(&mut SessionTable) -> T) -> T {
    let mut all = tables().lock().expect("vllm tables poisoned");
    f(all
        .entry(endpoint.trim_end_matches('/').to_string())
        .or_default())
}

/// Seconds on a monotonic clock — python `time.monotonic`. Only differences
/// are ever used, so the origin is arbitrary.
fn monotonic() -> f64 {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    ORIGIN.get_or_init(Instant::now).elapsed().as_secs_f64()
}

/// python `str(m.get("content", ""))`.
fn content_text(m: &Value) -> String {
    m.get("content").map(py_str).unwrap_or_default()
}

fn role(m: &Value) -> Option<&str> {
    m.get("role").and_then(Value::as_str)
}

/// A conversation's identity: its system text + first user turn.
///
/// That is the PREFIX vLLM caches, and it is stable across turns because the
/// history is append-only. When compaction rewrites it, the key changes — and
/// so has the prefix, so the old cache no longer applies: counting it as a new
/// conversation is the truth, and the stale one ages out of the table.
#[must_use]
pub fn conversation_key(messages: &[Value]) -> String {
    let system = messages
        .iter()
        .filter(|m| role(m) == Some("system"))
        .map(content_text)
        .collect::<Vec<_>>()
        .join("\n\n");
    let first_user = messages
        .iter()
        .find(|m| role(m) == Some("user"))
        .map(content_text)
        .unwrap_or_default();
    let digest = Sha256::digest(format!("{system}\u{0}{first_user}").as_bytes());
    digest
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()[..16]
        .to_string()
}

/// Tokens a chat template adds around one message (role markers, separators) — a
/// generous bound (Qwen's ChatML adds ~5).
pub const MESSAGE_OVERHEAD: i64 = 16;

/// An UPPER bound on the prompt's tokens: every byte of each message's role and
/// content (a byte-level BPE token is at least one byte) plus
/// [`MESSAGE_OVERHEAD`] per message. Never below the server's count, so a turn it
/// lets keep its `max_tokens` fits the window. python `prompt_bound`.
#[must_use]
pub fn prompt_bound(messages: &[Value]) -> i64 {
    messages
        .iter()
        .map(|m| {
            let role = m
                .get("role")
                .map(fantastic_bundle::py_str)
                .unwrap_or_default();
            let content = match m.get("content") {
                Some(Value::String(s)) => s.len(),
                Some(v) => serde_json::to_string(v).map_or(0, |s| s.len()),
                None => "null".len(),
            };
            i64::try_from(role.len() + content).unwrap_or(i64::MAX) + MESSAGE_OVERHEAD
        })
        .sum()
}

/// Every system message merged into ONE at index 0 (Qwen's template refuses a
/// system message anywhere else); the other turns keep their order.
#[must_use]
pub fn one_system_message(messages: &[Value]) -> Vec<Value> {
    let systems: Vec<String> = messages
        .iter()
        .filter(|m| role(m) == Some("system"))
        .map(content_text)
        .collect();
    let rest = messages
        .iter()
        .filter(|m| role(m) != Some("system"))
        .cloned();
    if systems.is_empty() {
        return rest.collect();
    }
    std::iter::once(json!({"role": "system", "content": systems.join("\n\n")}))
        .chain(rest)
        .collect()
}

/// python's `f"{x:g}"` for the timeouts a refusal names (`5`, `900`, `2.5`).
fn py_g(x: f64) -> String {
    if x == 0.0 {
        return "0".to_string();
    }
    #[allow(clippy::cast_possible_truncation)]
    let exp = x.abs().log10().floor() as i32;
    if !(-4..6).contains(&exp) {
        let s = format!("{x:.5e}");
        let (m, e) = s.split_once('e').unwrap_or((&s, "0"));
        let m = m.trim_end_matches('0').trim_end_matches('.');
        let e: i32 = e.parse().unwrap_or(0);
        return format!("{m}e{}{:02}", if e < 0 { '-' } else { '+' }, e.abs());
    }
    let decimals = usize::try_from(5 - exp).unwrap_or(0);
    let s = format!("{x:.decimals$}");
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    }
}

// ─── the thinking budget a pass SENT, per agent ─────────────────────────────

/// What the last pass of each agent actually sent as `thinking_token_budget`
/// (python `vllm_backend.provider.last_budgets`). The provider is rebuilt per
/// call, so it lives beside the tables, keyed by agent id, and reflect reads it
/// back.
fn last_budgets() -> &'static Mutex<HashMap<String, Option<i64>>> {
    static LAST: OnceLock<Mutex<HashMap<String, Option<i64>>>> = OnceLock::new();
    LAST.get_or_init(|| Mutex::new(HashMap::new()))
}

/// One agent's token counts since its kernel started (RAM — a restart zeroes them),
/// from the `usage` of each streamed reply, side calls included (#192).
#[derive(Default)]
struct Usage {
    requests: i64,
    prompt: i64,
    output: i64,
}

/// Each agent's counts, keyed by id: the stream adds to it, `usage` and `reflect` read it.
fn usage_table() -> &'static Mutex<HashMap<String, Arc<Mutex<Usage>>>> {
    static TABLE: OnceLock<Mutex<HashMap<String, Arc<Mutex<Usage>>>>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The agent's running counts — created on first use.
fn usage_of(agent_id: &str) -> Arc<Mutex<Usage>> {
    let mut table = usage_table().lock().expect("usage table poisoned");
    Arc::clone(table.entry(agent_id.to_string()).or_default())
}

/// The agent's counts as the `usage` verb and `reflect` show them.
fn usage_now(agent_id: &str) -> Value {
    let counts = usage_of(agent_id);
    let u = counts.lock().expect("usage poisoned");
    json!({"requests": u.requests, "prompt_tokens": u.prompt, "output_tokens": u.output})
}

/// The `usage` verb's doc.
const USAGE_DOC: &str = "usage — args: reset? (bool). The agent's token usage since its kernel \
     started: requests, prompt_tokens, output_tokens. reset: true returns the counts, then \
     zeroes them.";

/// `usage` — args: reset? (bool). `reset: true` returns the counts, then zeroes them.
fn usage_reply(agent_id: &str, payload: &Value) -> Value {
    let reset = payload.get("reset").cloned().unwrap_or(json!(false));
    let Some(reset) = reset.as_bool() else {
        let mut r = fantastic_kernel::refusal::refusal(
            fantastic_kernel::refusal::BAD_ARG,
            "usage: reset must be true or false",
        );
        fantastic_kernel::refusal::with(&mut r, "arg", json!("reset"));
        return r;
    };
    let out = usage_now(agent_id);
    if reset {
        *usage_of(agent_id).lock().expect("usage poisoned") = Usage::default();
    }
    out
}

// ─── provider ───────────────────────────────────────────────────────────────

/// How a server names the thinking budget (#207): vllm sends
/// `thinking_token_budget`, strata `reasoning_budget_tokens`, llamacpp takes
/// no per-request budget (a server flag).
pub const DIALECTS: [&str; 3] = ["vllm", "strata", "llamacpp"];

struct VllmProvider {
    agent_id: String,
    endpoint: String,
    model: String,
    /// The key reader, awaited before EACH request; `None` = no `credentials`
    /// on the record, no header.
    key: Arc<dyn KeySource>,
    max_tokens: i64,
    /// The record's `context_window`. vLLM counts the prompt AND the output against
    /// one window, so a cap the prompt leaves no room for is refused (HTTP 400). A
    /// turn whose `max_tokens` could overrun it — [`prompt_bound`] plus `max_tokens`
    /// past the window — sends NO max_tokens: vLLM then gives it exactly what the
    /// prompt leaves (a cap of nearly the whole window would be refused once a
    /// prompt is in it).
    /// python `VllmProvider._window`.
    window: Option<i64>,
    kv_slots: i64,
    kv_overflow: String,
    /// how this server names the thinking budget (#207): vllm, strata or llamacpp
    dialect: String,
    kv_ttl: f64,
    /// The effective sampling: the record's values over the defaults, plus any
    /// PASSTHROUGH field the record set.
    sampling: Map<String, Value>,
    /// Whether `thinking_token_budget` came from the RECORD (the caller's
    /// statement — never touched) or from the default (ours to fit per call).
    explicit_budget: bool,
    connect_timeout: f64,
    /// THE READ CLOCK: the longest the server may go without a byte, headers
    /// included — the record's `stream_idle_seconds`, else `read_timeout` (#177).
    read_clock: f64,
    report: ReportCell,
}

impl VllmProvider {
    /// The `thinking_token_budget` to send on a call bounded by `call_max`
    /// (python `VllmProvider._budget_for`):
    ///
    /// - a MAIN turn with an EXPLICIT budget sends it as set — the write keeps
    ///   it below the record's `max_tokens`, so it can fire;
    /// - otherwise (the DEFAULT on a main turn, or any budget on a bounded
    ///   SIDE CALL such as a fold summary) it is `min(budget, call_max / 2)`,
    ///   so at least half of the call's output is left for the answer (a rule
    ///   of thumb, not tuned) — an explicit 8000 on a ~2000-token fold summary
    ///   becomes 1000 instead of refusing the fold;
    /// - below [`BUDGET_FLOOR`] NO budget is sent at all: a few tokens of
    ///   thinking is no thinking — the model stops with thinking only and no
    ///   answer (a dead turn).
    fn budget_for(&self, call_max: i64, side_call: bool) -> Result<Option<i64>, ProviderError> {
        let Some(v) = self
            .sampling
            .get("thinking_token_budget")
            .filter(|v| !v.is_null())
        else {
            return Ok(None);
        };
        let budget = py_int(v).map_err(|e| ProviderError::value_error(e.to_string()))?;
        if self.explicit_budget && !side_call {
            return Ok(Some(budget));
        }
        let adapted = budget.min(call_max.div_euclid(2));
        if adapted < BUDGET_FLOOR {
            tracing::info!(
                "{NAME}: no thinking_token_budget for a call bounded by max_tokens={call_max} \
                 (half of it, {adapted}, is under the {BUDGET_FLOOR} floor)"
            );
            return Ok(None);
        }
        if adapted != budget {
            tracing::info!(
                "{NAME}: thinking_token_budget {budget} adapted to {adapted} for a call \
                 bounded by max_tokens={call_max}"
            );
        }
        Ok(Some(adapted))
    }

    fn body(
        &self,
        messages: &[Value],
        call_max: i64,
        side_call: bool,
    ) -> Result<Value, ProviderError> {
        let mut body = Map::new();
        body.insert("model".into(), json!(self.model));
        let sent = one_system_message(messages);
        let room = self
            .window
            .is_none_or(|w| call_max.saturating_add(prompt_bound(&sent)) <= w);
        body.insert("messages".into(), json!(sent));
        if side_call || room {
            body.insert("max_tokens".into(), json!(call_max));
        } // else: the rest of the window — see `window`
        body.insert("stream".into(), json!(true));
        // the final chunk carries usage (and an EMPTY choices array)
        body.insert("stream_options".into(), json!({"include_usage": true}));
        for key in ["temperature", "top_p", "top_k", "reasoning_effort"]
            .into_iter()
            .chain(PASSTHROUGH)
        {
            if let Some(v) = self.sampling.get(key).filter(|v| !v.is_null()) {
                body.insert(key.into(), v.clone());
            }
        }
        let budget = self.budget_for(call_max, side_call)?;
        last_budgets()
            .lock()
            .expect("vllm budgets poisoned")
            .insert(self.agent_id.clone(), budget);
        if let Some(b) = budget {
            // the dialect's own name for the budget (#207); llamacpp takes none
            match self.dialect.as_str() {
                "strata" => body.insert("reasoning_budget_tokens".into(), json!(b)),
                "llamacpp" => None,
                _ => body.insert("thinking_token_budget".into(), json!(b)),
            };
        }
        Ok(Value::Object(body))
    }

    /// A failed request, said in words: what the socket error MEANS for a
    /// vLLM box (python maps httpx's exception classes the same way).
    fn connection_error(&self, e: &reqwest::Error) -> ProviderError {
        classify(
            e.is_connect(),
            e.is_timeout(),
            &self.endpoint,
            self.connect_timeout,
            self.read_clock,
            None,
        )
    }
}

/// The socket error and every cause under it, said in one line — what a log
/// needs to tell a reset from a bad chunk from a timeout.
fn error_chain(e: &reqwest::Error) -> String {
    let mut text = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// The four connection-failure sentences, chosen from what the error IS.
fn classify(
    connect: bool,
    timeout: bool,
    endpoint: &str,
    connect_timeout: f64,
    read_clock: f64,
    cause: Option<&str>,
) -> ProviderError {
    // every branch here is the LINK failing, not the model or the request (#198)
    if connect && timeout {
        ProviderError::transport(format!(
            "{NAME}: no answer from {endpoint} within connect_timeout={}s — the \
             instance is STOPPED (its SYNs are dropped), or the tunnel/port is wrong",
            py_g(connect_timeout)
        ))
    } else if connect {
        ProviderError::transport(format!(
            "{NAME}: {endpoint} refused the connection — the host is up but vLLM is \
             not listening yet (it loads for ~75 s after boot), or the tunnel in front \
             of it is down"
        ))
    } else if timeout {
        // the same stall every stream backend reports (#177)
        ProviderError::transport(
            Stalled {
                idle_secs: read_clock,
            }
            .error(NAME, endpoint)
            .message,
        )
    } else {
        let why = cause.map(|c| format!(" (cause: {c})")).unwrap_or_default();
        ProviderError::transport(format!(
            "{NAME}: {endpoint} dropped the stream mid-answer — vLLM died or \
             restarted{why}; this turn is NOT complete"
        ))
    }
}

#[async_trait]
impl Provider for VllmProvider {
    fn model(&self) -> String {
        self.model.clone()
    }

    fn report(&self) -> ProviderReport {
        self.report.lock().expect("report poisoned").clone()
    }

    fn clear_prompt_tokens(&self) {
        let mut r = self.report.lock().expect("report poisoned");
        r.prompt_tokens = None;
        r.completion_tokens = None;
    }

    fn clear_stop_reason(&self) {
        self.report.lock().expect("report poisoned").stop_reason = None;
    }

    async fn chat(
        &self,
        messages: &[Value],
        max_tokens: Option<i64>,
    ) -> Result<ProviderStream, ProviderError> {
        // `max_tokens` bounds a side call (a fold summary); absent, the
        // record's `max_tokens` bounds the turn.
        let call_max = max_tokens.unwrap_or(self.max_tokens);
        // a per-call bound marks a SIDE CALL (a fold summary)
        let body = self.body(messages, call_max, max_tokens.is_some())?;
        // Admission BEFORE the request: a `reject` must not reach the GPU. A side
        // call is one request, not a conversation kept warm: it takes no slot, so
        // folds cannot fill the table (#184).
        let rotated = if max_tokens.is_some() {
            None
        } else {
            let key = conversation_key(messages);
            with_table(&self.endpoint, |t| {
                t.admit(
                    &key,
                    self.kv_slots,
                    &self.kv_overflow,
                    self.kv_ttl,
                    monotonic(),
                )
            })?
        };
        if let Some(out) = rotated {
            tracing::info!("{NAME}: KV slot rotated — conversation {out} out");
        }

        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs_f64(self.connect_timeout))
            // after connect, one deadline per read — python's httpx read timeout
            .read_timeout(Duration::from_secs_f64(self.read_clock))
            .build()
            .map_err(|e| ProviderError::raised(format!("{NAME}: http client: {e}")))?;
        // THE KEY, read before EACH request (python `await self._key(...)`):
        // none on the record ⇒ no `Authorization` header at all.
        let key = self
            .key
            .key(&self.endpoint)
            .await?
            .filter(|k| !k.is_empty());
        let mut req = client
            .post(format!("{}/chat/completions", self.endpoint))
            .json(&body);
        if let Some(key) = &key {
            req = req.bearer_auth(key);
        }
        let resp = req.send().await.map_err(|e| self.connection_error(&e))?;
        let status = resp.status();
        if status.as_u16() >= 400 {
            let detail: String = resp
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(500)
                .collect();
            let why = if matches!(status.as_u16(), 401 | 403) {
                " — the door wants a key: name it in `credentials` ({store, ref})".to_string()
            } else {
                String::new()
            };
            // a transient 5xx is the LINK failing, not the request (#198)
            if matches!(status.as_u16(), 502..=504) {
                return Err(ProviderError::transport(format!(
                    "{NAME}: {} answered HTTP {}{why}: {detail}",
                    self.endpoint,
                    status.as_u16()
                )));
            }
            return Err(ProviderError::raised(format!(
                "{NAME}: {} answered HTTP {}{why}: {detail}",
                self.endpoint,
                status.as_u16()
            )));
        }
        Ok(consume_sse(
            resp,
            Arc::clone(&self.report),
            usage_of(&self.agent_id),
            self.endpoint.clone(),
            (self.connect_timeout, self.read_clock),
            // a MAIN turn (no per-call bound) must not end on a cut — see `end`
            max_tokens.is_none().then_some(call_max),
        ))
    }
}

/// The SSE body as a LIVE stream of tokens, per line (python's provider is an
/// async generator over `resp.aiter_lines()`, which reassembles a line a read
/// split mid-JSON — so does the buffer here).
fn consume_sse(
    resp: reqwest::Response,
    report: ReportCell,
    usage: Arc<Mutex<Usage>>,
    endpoint: String,
    timeouts: (f64, f64),
    main_turn_max: Option<i64>,
) -> ProviderStream {
    let inner = resp.bytes_stream();
    let init = (
        inner,
        SseState::new(report, usage, endpoint, timeouts, main_turn_max),
        VecDeque::new(),
        false,
    );
    Box::pin(futures_util::stream::unfold(
        init,
        |(mut inner, mut st, mut pending, mut finished)| async move {
            loop {
                if let Some(ev) = pending.pop_front() {
                    return Some((ev, (inner, st, pending, finished)));
                }
                if finished {
                    return None;
                }
                let Some(chunk_res) = inner.next().await else {
                    // EOF without `[DONE]`: the same ending checks apply, and a
                    // last data line with no newline is still decoded.
                    finished = true;
                    match st.decode_buffered(true, &mut pending) {
                        Ok(_) => st.end(&mut pending),
                        Err(e) => pending.push_back(Err(e)),
                    }
                    continue;
                };
                match chunk_res {
                    Ok(bytes) => st.buf.extend_from_slice(&bytes),
                    Err(e) => {
                        finished = true;
                        if e.is_timeout() {
                            // the stall, said as every stream backend says it (#177)
                            pending.push_back(Err(classify(
                                false,
                                true,
                                &st.endpoint,
                                st.timeouts.0,
                                st.timeouts.1,
                                None,
                            )));
                        } else if st.saw_finish {
                            // vLLM already said the answer was finished; the body
                            // broke after that (a tunnel closing the socket on a
                            // clean completion). The answer is complete: end it as
                            // a clean close, and log what broke.
                            tracing::warn!(
                                "{NAME}: the stream from {} broke after its finish_reason — \
                                 the answer is complete: {}",
                                st.endpoint,
                                error_chain(&e)
                            );
                            // the cut tail may be half a line: it is ignored, not an error
                            if let Err(tail) = st.decode_buffered(true, &mut pending) {
                                tracing::warn!("{NAME}: ignoring the cut tail: {}", tail.message);
                            }
                            st.end(&mut pending);
                        } else {
                            // a failure before the finish: said in words, no closing
                            // marker (python raises straight out of the generator)
                            pending.push_back(Err(classify(
                                false,
                                false,
                                &st.endpoint,
                                st.timeouts.0,
                                st.timeouts.1,
                                Some(&error_chain(&e)),
                            )));
                        }
                        continue;
                    }
                }
                match st.decode_buffered(false, &mut pending) {
                    Ok(false) => {}
                    Ok(true) => {
                        finished = true;
                        st.end(&mut pending);
                    }
                    Err(e) => {
                        finished = true;
                        pending.push_back(Err(e));
                    }
                }
            }
        },
    ))
}

/// Per-line SSE decode state — the buffer, the per-TURN thought span, and
/// whether vLLM ever said the answer was finished.
struct SseState {
    buf: Vec<u8>,
    wrap: ThinkWrap,
    report: ReportCell,
    /// The agent's counts; the reply's final `usage` chunk adds to them (#192).
    usage: Arc<Mutex<Usage>>,
    saw_finish: bool,
    endpoint: String,
    timeouts: (f64, f64),
    /// `Some(bound)` on a MAIN turn (the record's `max_tokens`), where a cut
    /// is a failure; `None` on a bounded side call, where it is the answer.
    main_turn_max: Option<i64>,
    /// OpenAI-style servers send the model's calls as `delta.tool_calls`
    /// pieces (#186): collected here, rendered as call text when the stream
    /// ends, so a native call never vanishes into a dead turn.
    tool_calls: Vec<(String, String)>,
}

impl SseState {
    fn new(
        report: ReportCell,
        usage: Arc<Mutex<Usage>>,
        endpoint: String,
        timeouts: (f64, f64),
        main_turn_max: Option<i64>,
    ) -> Self {
        Self {
            buf: Vec::new(),
            wrap: ThinkWrap::new(),
            report,
            usage,
            saw_finish: false,
            endpoint,
            timeouts,
            main_turn_max,
            tool_calls: Vec::new(),
        }
    }

    /// Decode every complete line in the buffer; with `tail`, also the rest of
    /// the buffer, which has no newline yet. `Ok(true)` = `[DONE]`: the stream
    /// is over.
    fn decode_buffered(
        &mut self,
        tail: bool,
        out: &mut VecDeque<Result<ProviderEvent, ProviderError>>,
    ) -> Result<bool, ProviderError> {
        while let Some(nl) = self.buf.iter().position(|b| *b == b'\n') {
            let line_bytes: Vec<u8> = self.buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&line_bytes[..line_bytes.len() - 1])
                .trim_end_matches('\r')
                .to_string();
            if self.decode_line(&line, out)? {
                return Ok(true);
            }
        }
        if tail && !self.buf.is_empty() {
            let rest = std::mem::take(&mut self.buf);
            let line = String::from_utf8_lossy(&rest)
                .trim_end_matches('\r')
                .to_string();
            return self.decode_line(&line, out);
        }
        Ok(false)
    }

    /// Decode ONE line. `Ok(true)` = `[DONE]`: the stream is over.
    fn decode_line(
        &mut self,
        line: &str,
        out: &mut VecDeque<Result<ProviderEvent, ProviderError>>,
    ) -> Result<bool, ProviderError> {
        // anything that is not a `data:` line (a blank separator, a `:`
        // keep-alive) carries nothing
        let Some(rest) = line.strip_prefix("data:") else {
            return Ok(false);
        };
        let data = rest.trim();
        if data == "[DONE]" {
            return Ok(true);
        }
        if data.is_empty() {
            return Ok(false);
        }
        // python `json.loads` raises on a malformed line — the turn fails.
        let obj: Value = serde_json::from_str(data).map_err(|e| ProviderError {
            message: e.to_string(),
            kind: "JSONDecodeError",
            transport: false,
        })?;
        // `usage.prompt_tokens` and a `length` finish → `max_tokens`
        fantastic_ai_core::think::note_openai_usage(&obj, &self.report);
        if let Some(u) = obj.get("usage").filter(|u| u.is_object()) {
            let n = |k: &str| u.get(k).and_then(Value::as_i64).unwrap_or(0);
            let mut counts = self.usage.lock().expect("usage poisoned");
            counts.requests += 1;
            counts.prompt += n("prompt_tokens");
            counts.output += n("completion_tokens");
        }
        let Some(choice) = obj
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
        else {
            return Ok(false); // the usage-only final chunk
        };
        if choice.get("finish_reason").is_some_and(py_truthy) {
            self.saw_finish = true;
        }
        let mut delta = choice
            .get("delta")
            .filter(|d| py_truthy(d))
            .cloned()
            .unwrap_or_else(|| json!({}));
        // `reasoning_content` or, in some vLLM versions, `reasoning` — the
        // shared decoder reads the first spelling, so the second is folded in.
        let has_rc = delta
            .get("reasoning_content")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty());
        if !has_rc {
            if let Some(r) = delta.get("reasoning").cloned() {
                delta["reasoning_content"] = r;
            }
        }
        // `delta.tool_calls`: `{index, function: {name?, arguments?}}` pieces,
        // `name` arriving once and `arguments` as a JSON string in chunks (#186).
        if let Some(pieces) = delta.get("tool_calls").and_then(Value::as_array) {
            for piece in pieces {
                let idx = piece
                    .get("index")
                    .and_then(Value::as_i64)
                    .unwrap_or(0)
                    .max(0) as usize;
                if self.tool_calls.len() <= idx {
                    self.tool_calls
                        .resize(idx + 1, (String::new(), String::new()));
                }
                if let Some(f) = piece.get("function") {
                    if let Some(name) = f.get("name").and_then(Value::as_str) {
                        self.tool_calls[idx].0.push_str(name);
                    }
                    if let Some(args) = f.get("arguments").and_then(Value::as_str) {
                        self.tool_calls[idx].1.push_str(args);
                    }
                }
            }
        }
        for piece in decode_openai_delta(&delta, &mut self.wrap) {
            out.push_back(Ok(ProviderEvent::Token(piece)));
        }
        Ok(false)
    }

    /// The stream is over: close an open thought span, then refuse a stream
    /// vLLM never finished — the text so far must never read as an answer.
    fn end(&mut self, out: &mut VecDeque<Result<ProviderEvent, ProviderError>>) {
        if let Some(m) = self.wrap.close_marker() {
            out.push_back(Ok(ProviderEvent::Token(m.to_string())));
        }
        // A native call the server parsed for us becomes fantastic call text,
        // so the turn is not read as thinking-only (#186).
        if !self.tool_calls.is_empty() {
            let mut rendered: Vec<String> = Vec::new();
            for (name, args) in &self.tool_calls {
                match serde_json::from_str::<Value>(args) {
                    Ok(Value::Object(map)) => {
                        let body: String = map
                            .iter()
                            .map(|(k, v)| {
                                let text = match v {
                                    Value::String(s) => s.clone(),
                                    other => other.to_string(),
                                };
                                format!("<{k}>{text}</{k}>")
                            })
                            .collect();
                        rendered.push(format!("<call name=\"{name}\">{body}</call>"));
                    }
                    _ => out.push_back(Err(ProviderError::raised(format!(
                        "{NAME}: the server sent a native tool call '{name}' whose \
                         arguments are not a JSON object: {args}"
                    )))),
                }
            }
            if !rendered.is_empty() {
                out.push_back(Ok(ProviderEvent::Token(rendered.join("\n"))));
            }
        }
        if !self.saw_finish {
            out.push_back(Err(ProviderError::transport(format!(
                "{NAME}: the stream from {} ended without a finish_reason — vLLM died \
                 mid-answer; this turn is NOT complete",
                self.endpoint
            ))));
            return;
        }
        // A CUT: a BOUNDED SIDE CALL (a fold summary, which passed its own
        // `max_tokens`) that stopped at its bound got what it asked for — an
        // answer; its caller reads the stop reason. A MAIN turn that hit the
        // record's `max_tokens` did not finish, and returned as if it had, it
        // parses as a truncated envelope (or, on vLLM, a turn of pure
        // thinking) and the chain dies downstream with the cause hidden — so it
        // fails, naming the cut. Same contract as every other backend (python
        // `VllmProvider.chat`).
        let cut = self
            .report
            .lock()
            .expect("report poisoned")
            .stop_reason
            .as_deref()
            == Some("max_tokens");
        if let (true, Some(bound)) = (cut, self.main_turn_max) {
            out.push_back(Err(ProviderError::raised(format!(
                "{NAME}: the turn was cut at max_tokens={bound} (finish_reason=length) — \
                 it did not finish; raise max_tokens on the record, or lower \
                 thinking_token_budget / reasoning_effort"
            ))));
        }
    }
}

// ─── the record ─────────────────────────────────────────────────────────────

/// A count the turn runs on — set on the record, or read from the server before
/// the turn ([`server::resolve`]); fails the turn loud otherwise.
fn required_int(meta: &Map<String, Value>, key: &str) -> Result<i64, ProviderError> {
    match meta.get(key) {
        Some(Value::Number(n)) if n.as_i64().is_some_and(|v| v >= 1) => Ok(n.as_i64().unwrap_or(1)),
        _ => Err(ProviderError::value_error(format!(
            "{NAME}: `{key}` is not on the agent record, a positive int, and was not read \
             from the server (update_agent <id> {key}=…)"
        ))),
    }
}

/// The effective sampling — the record's values over the defaults, then any
/// PASSTHROUGH field the record set — and whether `thinking_token_budget` was
/// set on the RECORD (explicit: sent as is) or defaulted (ours to adapt).
fn sampling(meta: &Map<String, Value>) -> (Map<String, Value>, bool) {
    let mut eff = Map::new();
    for (k, default) in sampling_defaults() {
        let v = meta
            .get(k)
            .filter(|v| !v.is_null())
            .cloned()
            .unwrap_or(default);
        eff.insert(k.to_string(), v);
    }
    for k in PASSTHROUGH {
        if let Some(v) = meta.get(k).filter(|v| !v.is_null()) {
            eff.insert(k.to_string(), v.clone());
        }
    }
    let explicit = meta
        .get("thinking_token_budget")
        .is_some_and(|v| !v.is_null());
    (eff, explicit)
}

/// python `float(rec.get(key) or default)`.
fn seconds(meta: &Map<String, Value>, key: &str, default: f64) -> Result<f64, ProviderError> {
    match meta.get(key).filter(|v| py_truthy(v)) {
        Some(v) => py_float(v).map_err(|e| ProviderError::value_error(e.to_string())),
        None => Ok(default),
    }
}

/// The read clock (#177): the record's `stream_idle_seconds` when it names one,
/// else the `read_timeout` the backend has always read — the same per-read bound
/// under the name every stream backend shares. Its default stays `read_timeout`:
/// a vLLM server may take well over a minute to prefill before its first byte.
fn read_clock_of(meta: &Map<String, Value>, read_timeout: f64) -> Result<f64, ProviderError> {
    match meta.get("stream_idle_seconds").filter(|v| !v.is_null()) {
        None => Ok(read_timeout),
        Some(v) => v
            .as_f64()
            .filter(|s| *s > 0.0 && s.is_finite())
            .ok_or_else(|| {
                ProviderError::value_error(format!(
                    "stream_idle_seconds must be a positive number of seconds, not {v}"
                ))
            }),
    }
}

/// The provider this agent is configured for — python `make_provider`, with
/// its checks in python's order: `kv_overflow`, then `endpoint`, `model`,
/// `max_tokens`, `kv_slots`. Built per call (cheap, and the record can change
/// between turns). A record missing a required field yields a provider whose
/// every pass raises that refusal.
fn build_provider(agent_id: &AgentId, kernel: &Arc<Kernel>, _client_id: &str) -> Arc<dyn Provider> {
    let meta = helpers::agent_meta(agent_id, kernel);
    let key: Arc<dyn KeySource> = Arc::new(key_reader(agent_id, kernel, NAME, false));
    match configure(agent_id, &meta, key) {
        Ok(p) => Arc::new(p),
        Err(e) => Arc::new(Unconfigured(e)),
    }
}

fn configure(
    agent_id: &AgentId,
    meta: &Map<String, Value>,
    key: Arc<dyn KeySource>,
) -> Result<VllmProvider, ProviderError> {
    let (sampling, explicit_budget) = sampling(meta);
    let overflow = meta.get("kv_overflow").and_then(Value::as_str);
    let Some(kv_overflow) = overflow.filter(|o| KV_OVERFLOWS.contains(o)) else {
        return Err(ProviderError::value_error(format!(
            "{NAME}: `kv_overflow` is required on the agent record — evict (the \
             least-recently-used conversation rotates out) or reject (refuse the \
             newcomer) (update_agent <id> kv_overflow=…)"
        )));
    };
    Ok(VllmProvider {
        agent_id: agent_id.as_str().to_string(),
        endpoint: helpers::required_meta(meta, "endpoint", NAME)?
            .trim_end_matches('/')
            .to_string(),
        model: helpers::required_meta(meta, "model", NAME)?,
        key,
        max_tokens: required_int(meta, "max_tokens")?,
        window: Some(required_int(meta, "context_window")?),
        kv_slots: required_int(meta, "kv_slots")?,
        kv_overflow: kv_overflow.to_string(),
        dialect: meta
            .get("server_dialect")
            .and_then(Value::as_str)
            .unwrap_or("vllm")
            .to_string(),
        kv_ttl: seconds(meta, "kv_ttl", KV_TTL)?,
        sampling,
        explicit_budget,
        connect_timeout: seconds(meta, "connect_timeout", CONNECT_TIMEOUT)?,
        read_clock: read_clock_of(meta, seconds(meta, "read_timeout", READ_TIMEOUT)?)?,
        report: ReportCell::default(),
    })
}

/// `reflect` — ai-core's shared base body, then python's `_reflect_extra`: the
/// EFFECTIVE sampling (defaults filled in), the timeouts and the endpoint's
/// shared KV session table, so one reflect shows what the next request will
/// send and how many conversations are live.
fn reflect_reply(agent_id: &AgentId, kernel: &Kernel) -> Result<Value, BundleError> {
    let mut out = verbs::reflect_base(agent_id, kernel, SENTENCE, CFG.model_window);
    let meta = helpers::agent_meta(agent_id, kernel);
    let (sampling, explicit) = sampling(&meta);
    let secs = |key: &str, default: f64| -> Result<f64, BundleError> {
        match meta.get(key).filter(|v| py_truthy(v)) {
            Some(v) => Ok(py_float(v)?),
            None => Ok(default),
        }
    };
    out.insert("sampling".into(), Value::Object(sampling));
    out.insert("usage".into(), usage_now(agent_id.as_str()));
    out.insert(
        "thinking_token_budget_source".into(),
        json!(if explicit { "record" } else { "default" }),
    );
    // what the last pass actually SENT (the default adapts to small calls)
    let last = last_budgets()
        .lock()
        .expect("vllm budgets poisoned")
        .get(agent_id.as_str())
        .copied()
        .flatten();
    out.insert("last_thinking_budget".into(), json!(last));
    out.insert(
        "max_tokens".into(),
        meta.get("max_tokens").cloned().unwrap_or(Value::Null),
    );
    out.insert(
        "timeouts".into(),
        json!({
            "connect": secs("connect_timeout", CONNECT_TIMEOUT)?,
            "read": secs("read_timeout", READ_TIMEOUT)?,
        }),
    );
    let mut kv = Map::new();
    kv.insert(
        "slots".into(),
        meta.get("kv_slots").cloned().unwrap_or(Value::Null),
    );
    kv.insert(
        "overflow".into(),
        meta.get("kv_overflow").cloned().unwrap_or(Value::Null),
    );
    kv.insert("ttl".into(), json!(secs("kv_ttl", KV_TTL)?));
    // shared by every vllm_backend agent on the same endpoint
    if let Some(endpoint) = meta.get("endpoint").and_then(Value::as_str) {
        kv.extend(with_table(endpoint, |t| t.view()));
    }
    out.insert("kv".into(), Value::Object(kv));
    out.insert(
        "server_dialect".into(),
        json!(meta
            .get("server_dialect")
            .and_then(Value::as_str)
            .unwrap_or("vllm")),
    );
    Ok(Value::Object(out))
}

/// python `_positive`: a bool is never a number; an int ≥ 1, or any number > 0.
fn positive(v: &Value, integer: bool) -> bool {
    let Value::Number(n) = v else {
        return false;
    };
    if integer {
        // an int wider than i64 is still a positive int (python ints are unbounded)
        match n.as_i64() {
            Some(i) => i >= 1,
            None => n.as_u64().is_some(),
        }
    } else {
        n.as_f64().is_some_and(|f| f > 0.0)
    }
}

/// Refuse a write this backend cannot honour, AT THE WRITE, naming the legal
/// set — python `validate_meta`, text for text. Checks the MERGED state,
/// because `thinking_token_budget` and `max_tokens` are only wrong together.
#[must_use]
pub fn validate(meta: &Map<String, Value>, record: &Map<String, Value>) -> Option<String> {
    let mut merged = record.clone();
    for (k, v) in meta {
        merged.insert(k.clone(), v.clone());
    }
    let set = |k: &str| meta.get(k).filter(|v| !v.is_null());
    for key in ["max_tokens", "context_window", "kv_slots"] {
        if set(key).is_some_and(|v| !positive(v, true)) {
            return Some(format!("{key} must be a positive int"));
        }
    }
    for key in [
        "kv_ttl",
        "connect_timeout",
        "read_timeout",
        "stream_idle_seconds",
    ] {
        if set(key).is_some_and(|v| !positive(v, false)) {
            return Some(format!("{key} must be a positive number of seconds"));
        }
    }
    if let Some(url) = set("metrics_url") {
        let ok = url
            .as_str()
            .is_some_and(|u| u.trim().starts_with("http://") || u.trim().starts_with("https://"));
        if !ok {
            return Some(
                "metrics_url must be an http(s) URL — the server's /metrics page".to_string(),
            );
        }
    }
    let endpoint = merged.get("endpoint").and_then(Value::as_str).unwrap_or("");
    if merged.get("kv_slots").is_none_or(Value::is_null)
        && !endpoint.trim().is_empty()
        && server::metrics_url(&merged).is_none()
    {
        return Some(format!(
            "kv_slots is left to the server, but the endpoint '{endpoint}' does not end in \
             /v1, so its metrics page is not known — set metrics_url (the server's /metrics \
             page) or kv_slots"
        ));
    }
    if let Some(v) = set("kv_overflow") {
        if !v.as_str().is_some_and(|s| KV_OVERFLOWS.contains(&s)) {
            return Some(format!(
                "kv_overflow={} is not a rotation policy — legal values: evict (the \
                 least-recently-used conversation rotates out), reject (refuse the \
                 newcomer)",
                py_repr_value(v)
            ));
        }
    }
    if let Some(v) = set("reasoning_effort") {
        if !v.as_str().is_some_and(|s| REASONING_EFFORTS.contains(&s)) {
            return Some(format!(
                "reasoning_effort={} is not one vLLM accepts — legal values: low, medium, \
                 xhigh (\"high\" answers 400; xhigh needs a thinking_token_budget or it can \
                 think without answering)",
                py_repr_value(v)
            ));
        }
    }
    let budget = merged.get("thinking_token_budget").filter(|v| !v.is_null());
    if set("thinking_token_budget").is_some() && budget.is_some_and(|b| !positive(b, true)) {
        return Some("thinking_token_budget must be a positive int".to_string());
    }
    let mx = merged.get("max_tokens");
    if let (Some(b), Some(m)) = (budget, mx) {
        if positive(b, true) && positive(m, true) {
            let (b, m) = (
                b.as_i64().unwrap_or(i64::MAX),
                m.as_i64().unwrap_or(i64::MAX),
            );
            if b >= m {
                return Some(format!(
                    "thinking_token_budget={b} is not below max_tokens={m} — it could \
                     never fire, and a turn would end mid-thought with no answer; keep it \
                     below max_tokens (half is a good start)"
                ));
            }
        }
    }
    if let Some(v) = set("server_dialect") {
        if !v.as_str().is_some_and(|s| DIALECTS.contains(&s)) {
            return Some("server_dialect must be one of vllm, strata, llamacpp".to_string());
        }
    }
    let dialect = merged.get("server_dialect").and_then(Value::as_str);
    if dialect == Some("llamacpp") && budget.is_some() {
        return Some(
            "llama.cpp takes the reasoning budget as a server flag, not per request — \
             unset thinking_token_budget (or use server_dialect strata/vllm)"
                .to_string(),
        );
    }
    // EVERY required field, checked at the WRITE: a model spawning a sub-AI
    // copies the fields it can see, and a record the write accepted but no turn
    // could use would fail only later, on every turn, far from the create that
    // caused it. Named ALL at once, from the schema, so one retry fixes the
    // record.
    schema::validate(record_schema(), NAME, meta, record)
        .or_else(|| caching::refusal(meta, record, NAME, CFG.provider_cache))
}

/// The sampling default for `key` — the one table the provider fills from.
fn sampling_default(key: &str) -> Value {
    sampling_defaults()
        .into_iter()
        .find(|(k, _)| *k == key)
        .map_or(Value::Null, |(_, v)| v)
}

/// THE RECORD SCHEMA — what this kind's record takes, read with
/// no agent existing (`reflect bundles=all`). Built from the constants the
/// provider runs on, and enforced by [`validate`], so the page, the write check
/// and the turn cannot disagree. `endpoint` and `kv_overflow` are required; the
/// counts a record leaves out are READ FROM THE SERVER before a turn
/// ([`server`]), never given a default. Python `RECORD_SCHEMA`, field for field.
#[must_use]
pub fn record_schema() -> &'static Value {
    static SCHEMA: OnceLock<Value> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        const FROM_SERVER: &str = "; left out, read from the server before a turn and written here";
        schema::schema(vec![
            Field::new("endpoint", "str", "the vLLM server's /v1 URL").required(),
            Field::new(
                "model",
                "str",
                format!("the served model name{FROM_SERVER} (the one model it serves)"),
            ),
            Field::new(
                "max_tokens",
                "int",
                format!("a turn's output budget (tokens){FROM_SERVER} (max_model_len)"),
            ),
            Field::new(
                "context_window",
                "int",
                format!("the server's window (tokens){FROM_SERVER} (max_model_len)"),
            ),
            Field::new(
                "kv_slots",
                "int",
                format!(
                    "how many conversations may be live on the endpoint{FROM_SERVER} \
                         (floor of kv_cache_max_concurrency on metrics_url)"
                ),
            ),
            Field::new(
                "metrics_url",
                "str",
                "the server's Prometheus page, read for kv_slots; left out, the endpoint \
                     with /v1 replaced by /metrics",
            ),
            Field::new(
                "kv_overflow",
                "enum",
                "all slots live: evict (the least-recently-used rotates out) or reject \
                     (the newcomer is refused)",
            )
            .required()
            .one_of(&KV_OVERFLOWS),
            Field::new(
                "server_dialect",
                "enum",
                "how this server names things: vllm (default) sends thinking_token_budget; \
                 strata sends reasoning_budget_tokens; llamacpp takes no per-request budget \
                 (a server flag) — set explicitly, never guessed (#207)",
            )
            .one_of(&DIALECTS),
            Field::new("temperature", "float", "sampling temperature")
                .default(sampling_default("temperature")),
            Field::new("top_p", "float", "nucleus sampling").default(sampling_default("top_p")),
            Field::new("top_k", "int", "top-k sampling").default(sampling_default("top_k")),
            Field::new(
                "reasoning_effort",
                "enum",
                "how much the model wants to think (xhigh needs a thinking_token_budget)",
            )
            .one_of(&REASONING_EFFORTS)
            .default(sampling_default("reasoning_effort")),
            Field::new(
                "thinking_token_budget",
                "int",
                "the hard cap on thinking tokens, below max_tokens; unset, a turn sends \
                     min(default, max_tokens/2)",
            )
            .default(sampling_default("thinking_token_budget")),
            Field::new("min_p", "float", "min-p sampling; sent only when set"),
            Field::new("presence_penalty", "float", "sent only when set"),
            Field::new("frequency_penalty", "float", "sent only when set"),
            Field::new("repetition_penalty", "float", "sent only when set"),
            Field::new("no_repeat_ngram_size", "int", "sent only when set"),
            Field::new("seed", "int", "sent only when set"),
            Field::new("stop", "list", "stop sequences; sent only when set"),
            Field::new(
                "kv_ttl",
                "float",
                "seconds an idle conversation keeps its slot",
            )
            .default(json!(KV_TTL)),
            Field::new("connect_timeout", "float", "seconds to reach the server")
                .default(json!(CONNECT_TIMEOUT)),
            Field::new("read_timeout", "float", "seconds to wait on a generation")
                .default(json!(READ_TIMEOUT)),
            Field::new(
                "stream_idle_seconds",
                "float",
                "the read clock on the answer stream: seconds with no byte from the server \
                 (headers included) before the turn is judged stalled and ends with an error \
                 row; default read_timeout",
            ),
            // the door's key, when it wants one — absent = no Authorization header
            schema::credentials_field(false),
        ])
    })
}

/// The fields a turn cannot run without — refused at the write when absent. The
/// schema's required fields, not a second list (python `REQUIRED`).
#[must_use]
pub fn required() -> Vec<String> {
    schema::required_names(record_schema())
}

// ─── bundle impl ────────────────────────────────────────────────────────────

/// The vLLM backend bundle.
pub struct VllmBundle;

#[async_trait]
impl Bundle for VllmBundle {
    fn name(&self) -> &str {
        NAME
    }

    fn verb_docs(&self) -> Value {
        let mut docs = fantastic_ai_core::verbs::verb_docs();
        if let Some(m) = docs.as_object_mut() {
            m.insert("usage".into(), json!(USAGE_DOC));
        }
        docs
    }

    fn emits_doc(&self) -> Option<Value> {
        Some(fantastic_ai_core::verbs::emits())
    }

    fn cancel_doc(&self) -> Option<Value> {
        Some(Value::String(
            fantastic_ai_core::verbs::CANCEL_DOC.to_string(),
        ))
    }

    fn notes_doc(&self) -> Option<Value> {
        Some(fantastic_ai_core::verbs::notes())
    }

    fn readme(&self) -> Option<&'static str> {
        Some(README)
    }

    fn validate_meta(
        &self,
        meta: &Map<String, Value>,
        record: &Map<String, Value>,
    ) -> Option<String> {
        validate(meta, record)
    }

    fn record_schema(&self) -> Option<Value> {
        Some(record_schema().clone())
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        if verb == "reflect" {
            return Ok(Some(reflect_reply(agent_id, kernel)?));
        }
        if verb == "usage" {
            return Ok(Some(usage_reply(agent_id.as_str(), payload)));
        }
        // A verb that runs a turn first reads what the record leaves to the server;
        // a server that does not answer refuses the verb, naming the field.
        if server::TURN_VERBS.contains(&verb) {
            if let Some(refused) = server::resolve(agent_id, kernel).await {
                return Ok(Some(refused));
            }
        }
        // EVERY OTHER ai_core VERB, one table for every backend (python `build`).
        let provider = |client: &str| build_provider(agent_id, kernel, client);
        if let Some(reply) = verbs::handle_common(agent_id, payload, kernel, CFG, provider).await? {
            return Ok(Some(reply));
        }
        Ok(Some({
            let raw = payload.get("type").cloned().unwrap_or(Value::Null);
            let mut r = fantastic_kernel::refusal::refusal(
                fantastic_kernel::refusal::UNHANDLED,
                format!("{NAME}: unknown type {}", py_repr_value(&raw)),
            );
            fantastic_kernel::refusal::with(&mut r, "verb", raw);
            r
        }))
    }

    async fn on_delete(&self, agent_id: &AgentId, kernel: &Arc<Kernel>) -> Result<(), BundleError> {
        verbs::on_delete(agent_id, kernel).await;
        last_budgets()
            .lock()
            .expect("vllm budgets poisoned")
            .remove(agent_id.as_str());
        // this agent's counts go with it: an id re-created later starts clean
        usage_table()
            .lock()
            .expect("usage table poisoned")
            .remove(agent_id.as_str());
        Ok(())
    }

    async fn on_cancel(
        &self,
        agent_id: &AgentId,
        task_id: &TaskId,
        kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        verbs::on_cancel(agent_id, &task_id.0.to_string(), kernel).await
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod tests_server;
