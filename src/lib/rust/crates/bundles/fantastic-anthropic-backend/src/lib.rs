//! anthropic_backend — an LLM agent over Anthropic's Messages API. Mirrors python's
//! `anthropic_backend` (canonical): every refusal text and reflect key is python's.
//!
//! `POST {endpoint}` (the record's Messages URL) with `stream: true`. NO native
//! tools: no `tools` array is sent; ai-core parses the `<call>` elements out of the
//! streamed text, as for every backend.
//!
//! - ONE `system` string, top level — every system message joined; `messages` holds
//!   only `user` / `assistant`, consecutive turns of one role merged.
//! - `max_tokens` on every request (the API requires it): the record's (read from
//!   the model entry when left out, [`server`]), or a side call's own bound.
//! - THE THOUGHT CHANNEL: with `thinking_budget` set, `thinking_delta`s stream
//!   before the answer, wrapped once in `<think>…</think>`.
//! - The SSE stream ends with `message_stop`; a stream that ends without it did not
//!   finish. An `error` event is raised with the API's own type and message.
//! - THE KEY: the record's optional `credentials` ref, sent as `x-api-key` (with
//!   `anthropic-version`); none, no key header.
//! - CACHING (ai-core's `caching`): `provider_cache` sends ephemeral `cache_control`
//!   markers: on the `system` block, on the block that ends the primer row, on the
//!   block that ends the last fold, and the top-level automatic one. At most four.
//!   Every other mode sends none, and the body is unchanged.
//! - USAGE: each stream's token counts accumulate into the agent's [`Usage`] (RAM;
//!   the `usage` verb and `reflect` read it), side calls included.

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_ai_core::caching::{self, Caching};
use fantastic_ai_core::credentials::{key_reader, KeySource};
use fantastic_ai_core::provider::{
    stream_idle_of, within, Provider, ProviderError, ProviderEvent, ProviderReport, ProviderStream,
    ReportCell, Unconfigured,
};
use fantastic_ai_core::schema::{self, Field};
use fantastic_ai_core::think::ThinkWrap;
use fantastic_ai_core::{agent_loop::BackendConfig, helpers, verbs};
use fantastic_bundle::{py_float, py_repr_value};
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel, TaskId};
use futures_util::StreamExt;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

pub mod server;

/// The handler module id a record names to select this bundle.
pub const HANDLER_MODULE: &str = "anthropic_backend.tools";

/// The bundle readme — python's, byte for byte.
pub const README: &str = include_str!("readme.md");

/// The name every refusal carries.
const NAME: &str = "anthropic_backend";

/// The `usage` verb's doc — python `_usage`'s docstring, the verb's own.
const USAGE_DOC: &str = "usage — args: reset? (bool). The agent's token usage since its kernel started:\nrequests, input, cache write and read, output, the derived prompt_tokens, hit_rate\nand saved_tokens (cost_usd and saved_usd when the record has both prices). reset:\ntrue returns the counts, then zeroes them.";

/// What `reflect` says this agent IS — python `tools.py`'s `sentence`.
pub const SENTENCE: &str = "Anthropic Messages-API LLM agent (raw prompt-and-parse tool-calling).";

/// The API version every request names.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Seconds to reach the API. The stream has no read clock: the turn's one bound is
/// ai-core's send timeout.
pub const CONNECT_TIMEOUT: f64 = 15.0;
/// Below this no thinking is asked for: the API needs `budget_tokens >= 1024`.
pub const THINKING_FLOOR: i64 = 1024;

const CFG: BackendConfig = BackendConfig {
    name: NAME,
    model_window: fantastic_ai_core::constants::DEFAULT_CONTEXT_WINDOW,
    provider_cache: true,
};

/// ai-core's messages → (`system`, the user/assistant turns): every system message
/// joins the one `system` string; consecutive turns of one role merge, blank-line
/// separated. Python `to_messages`. The cache flags are not read: see [`to_wire`].
#[must_use]
pub fn to_messages(messages: &[Value]) -> (String, Vec<Value>) {
    to_wire(messages, false)
}

/// The cache flags ai-core sets on a message (`provider_cache` only).
#[derive(Clone, Copy, Default)]
struct Flags {
    /// The message holds the row the record's primer names: its end is marked.
    primer: bool,
    /// The message holds the last fold or clear row: its end is marked.
    fold: bool,
}

impl Flags {
    fn any(self) -> bool {
        self.primer || self.fold
    }
}

/// ai-core's messages → (`system`, the turns as the request sends them). `marks` reads
/// the cache flags; off, they are ignored and the turns are the plain merged strings.
///
/// With `marks`, a merged turn whose rows carry flags is cut into text blocks at the
/// end of each flagged row's text, so the marker lands there, not at the end of the
/// merged turn. The block texts concatenate to the merged string exactly: the blank
/// line that joins two rows opens the next block, and is never dropped. The one
/// exception is a trailing blank line with no row after it, which closes the block
/// before it (see [`cut_blocks`]), so that marker sits a blank line after its text.
fn to_wire(messages: &[Value], marks: bool) -> (String, Vec<Value>) {
    let mut systems: Vec<String> = Vec::new();
    let mut turns: Vec<(String, Vec<(String, Flags)>)> = Vec::new();
    for m in messages {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("");
        let content = match m.get("content") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => String::new(),
            Some(v) => fantastic_bundle::py_str(v),
        };
        let flag = |key: &str| marks && m.get(key).and_then(Value::as_bool) == Some(true);
        let flags = Flags {
            primer: flag("cache_primer"),
            fold: flag("cache_fold"),
        };
        match role {
            "system" => {
                if !content.is_empty() {
                    systems.push(content);
                }
            }
            "user" | "assistant" => match turns.last_mut() {
                Some((r, rows)) if r == role => rows.push((content, flags)),
                _ => turns.push((role.to_string(), vec![(content, flags)])),
            },
            _ => {}
        }
    }
    let turns = turns
        .into_iter()
        .map(|(role, rows)| json!({"role": role, "content": turn_content(&rows)}))
        .collect();
    (systems.join("\n\n"), turns)
}

/// One merged turn's `content`: the plain string when no row in it is flagged, else
/// its text blocks, the marked ones carrying the cache marker.
fn turn_content(rows: &[(String, Flags)]) -> Value {
    if !rows.iter().any(|(_, f)| f.any()) {
        let plain: Vec<&str> = rows.iter().map(|(t, _)| t.as_str()).collect();
        return json!(plain.join("\n\n"));
    }
    Value::Array(
        cut_blocks(rows)
            .into_iter()
            .map(|(text, marked)| {
                let mut block = json!({"type": "text", "text": text});
                if marked {
                    block["cache_control"] = cache_control();
                }
                block
            })
            .collect(),
    )
}

/// A merged turn cut at the end of each flagged row: `(block text, ends a flagged
/// row)`. A flagged row with no text (empty) opens no block of its own and carries no
/// marker; its text is none, so nothing is lost by it. A cut also needs the block so
/// far to hold text, so no cut block is blank: the API refuses a whitespace-only text
/// block. A flagged row whose text is only whitespace, reached while the block is
/// still blank (it opens its turn or follows blank rows), therefore carries no marker
/// either; its text joins the block that follows it. The tail after the last cut can
/// be blank separators only (a flagged row followed by empty rows): those separators
/// join the block before them. The marker then sits after the blank line that followed
/// the flagged row, not on its last character; the bytes are unchanged.
fn cut_blocks(rows: &[(String, Flags)]) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut cur = String::new();
    for (i, (text, flags)) in rows.iter().enumerate() {
        if i > 0 {
            cur.push_str("\n\n");
        }
        cur.push_str(text);
        if flags.any() && !text.is_empty() && !cur.trim().is_empty() {
            out.push((std::mem::take(&mut cur), true));
        }
    }
    if !cur.trim().is_empty() || out.is_empty() {
        out.push((cur, false));
    } else if let Some((last, _)) = out.last_mut() {
        last.push_str(&cur);
    }
    out
}

/// The cache marker a cached request sends: the 5-minute ephemeral TTL, the only one.
fn cache_control() -> Value {
    json!({"type": "ephemeral"})
}

/// `round(0.9 * cache_read - 0.25 * cache_write)` in full-price input tokens: a read
/// costs 0.1x the base input price, a write 1.25x. Exact integer arithmetic, halves
/// to even, so every runtime agrees. May be negative. Python `saved_tokens`.
#[must_use]
pub fn saved_tokens(cache_read: i64, cache_write: i64) -> i64 {
    let n = 18 * cache_read - 5 * cache_write;
    let (mut q, r) = (n.div_euclid(20), n.rem_euclid(20));
    if 2 * r > 20 || (2 * r == 20 && q.rem_euclid(2) == 1) {
        q += 1;
    }
    q
}

/// One agent's token counts since its kernel started (RAM — a restart zeroes them).
/// `input` is the uncached remainder; the two cache counts are what the API reports
/// for the prompt. Python `Usage`.
#[derive(Default)]
struct Usage {
    requests: i64,
    input: i64,
    cache_write: i64,
    cache_read: i64,
    output: i64,
}

impl Usage {
    /// What the counts derive to, with the record's prices when BOTH are set (USD per
    /// million tokens). Python `Usage.snapshot`.
    fn snapshot(&self, price_in: Option<f64>, price_out: Option<f64>) -> Value {
        let prompt = self.input + self.cache_write + self.cache_read;
        let saved = saved_tokens(self.cache_read, self.cache_write);
        let hit_rate = if prompt > 0 {
            json!(self.cache_read as f64 / prompt as f64)
        } else {
            json!(0)
        };
        let mut out = Map::new();
        out.insert("requests".into(), json!(self.requests));
        out.insert("input_tokens".into(), json!(self.input));
        out.insert("cache_write_tokens".into(), json!(self.cache_write));
        out.insert("cache_read_tokens".into(), json!(self.cache_read));
        out.insert("output_tokens".into(), json!(self.output));
        out.insert("prompt_tokens".into(), json!(prompt));
        out.insert("hit_rate".into(), hit_rate);
        out.insert("saved_tokens".into(), json!(saved));
        if let (Some(pin), Some(pout)) = (price_in, price_out) {
            let cost_in =
                self.input as f64 + 1.25 * self.cache_write as f64 + 0.1 * self.cache_read as f64;
            let cost = cost_in * pin / 1e6 + self.output as f64 * pout / 1e6;
            out.insert("cost_usd".into(), json!(cost));
            out.insert("saved_usd".into(), json!(saved as f64 * pin / 1e6));
        }
        Value::Object(out)
    }
}

/// Each agent's counts, keyed by id: the provider adds to it, the verbs read it.
fn usage_table() -> &'static Mutex<HashMap<String, Arc<Mutex<Usage>>>> {
    static TABLE: OnceLock<Mutex<HashMap<String, Arc<Mutex<Usage>>>>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The agent's running counts — created on first use.
fn usage_of(agent_id: &AgentId) -> Arc<Mutex<Usage>> {
    let mut table = usage_table().lock().expect("usage table poisoned");
    Arc::clone(
        table
            .entry(agent_id.as_str().to_string())
            .or_insert_with(|| Arc::new(Mutex::new(Usage::default()))),
    )
}

/// The Messages provider. PURE RAW TEXT.
struct AnthropicProvider {
    endpoint: String,
    model: String,
    key: Arc<dyn KeySource>,
    max_tokens: i64,
    temperature: Option<f64>,
    thinking_budget: Option<i64>,
    caching: Caching,
    usage: Arc<Mutex<Usage>>,
    report: ReportCell,
    /// The read clock: the longest the API may go without a byte, headers
    /// included ([`within`]).
    stream_idle: Duration,
}

impl AnthropicProvider {
    /// The request body. Python `AnthropicProvider.body`.
    fn body(&self, messages: &[Value], max_tokens: Option<i64>) -> Value {
        // Markers only in `provider_cache`: every other mode's body is the plain one.
        let marks = self.caching.marks();
        let (system, turns) = to_wire(messages, marks);
        let call_max = max_tokens.unwrap_or(self.max_tokens);
        let mut body = Map::new();
        body.insert("model".into(), json!(self.model));
        body.insert("max_tokens".into(), json!(call_max));
        body.insert("messages".into(), Value::Array(turns));
        body.insert("stream".into(), json!(true));
        if !system.is_empty() {
            // M1: the system block
            let system = if marks {
                json!([{"type": "text", "text": system, "cache_control": cache_control()}])
            } else {
                json!(system)
            };
            body.insert("system".into(), system);
        }
        if marks {
            // auto: the conversation tail, cached by the API itself
            body.insert("cache_control".into(), cache_control());
        }
        if let Some(t) = self.temperature {
            body.insert("temperature".into(), json!(t));
        }
        if let Some(b) = self
            .thinking_budget
            .filter(|b| *b >= THINKING_FLOOR && *b < call_max)
        {
            body.insert(
                "thinking".into(),
                json!({"type": "enabled", "budget_tokens": b}),
            );
        }
        Value::Object(body)
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
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
        let body = self.body(messages, max_tokens);
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs_f64(CONNECT_TIMEOUT))
            .build()
            .map_err(|e| ProviderError::raised(format!("{NAME}: http client: {e}")))?;
        let key = self
            .key
            .key(&self.endpoint)
            .await?
            .filter(|k| !k.is_empty());
        let mut req = client
            .post(&self.endpoint)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&body);
        if let Some(key) = &key {
            req = req.header("x-api-key", key);
        }
        // The headers are part of the read clock too: a connection that is up but
        // never answers is the same stall as a body that stops (#177).
        let sent = within(self.stream_idle, req.send())
            .await
            .map_err(|s| s.error(NAME, &self.endpoint))?;
        let resp = sent.map_err(|e| {
            ProviderError::raised(if e.is_timeout() {
                format!(
                    "{NAME}: no answer from {} within {CONNECT_TIMEOUT}s",
                    self.endpoint
                )
            } else {
                format!("{NAME}: could not connect to {}", self.endpoint)
            })
        })?;
        let status = resp.status().as_u16();
        if status >= 400 {
            let detail: String = resp
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(500)
                .collect();
            let why = if matches!(status, 401 | 403) {
                " — the API wants a key: name it in `credentials` ({store, ref})"
            } else {
                ""
            };
            return Err(ProviderError::raised(format!(
                "{NAME}: {} answered HTTP {status}{why}: {detail}",
                self.endpoint
            )));
        }
        Ok(consume_sse(
            resp,
            Arc::clone(&self.report),
            Arc::clone(&self.usage),
            self.endpoint.clone(),
            max_tokens.is_none().then_some(self.max_tokens),
            self.stream_idle,
        ))
    }
}

/// The SSE body as a LIVE stream of tokens, per line.
fn consume_sse(
    resp: reqwest::Response,
    report: ReportCell,
    usage: Arc<Mutex<Usage>>,
    endpoint: String,
    main_turn_max: Option<i64>,
    stream_idle: Duration,
) -> ProviderStream {
    let inner = resp.bytes_stream();
    let init = (
        inner,
        SseState {
            buf: Vec::new(),
            wrap: ThinkWrap::new(),
            report,
            usage,
            billed_out: 0,
            stopped: false,
            endpoint,
            main_turn_max,
        },
        VecDeque::new(),
        false,
    );
    Box::pin(futures_util::stream::unfold(
        init,
        move |(mut inner, mut st, mut pending, mut finished)| async move {
            loop {
                if let Some(ev) = pending.pop_front() {
                    return Some((ev, (inner, st, pending, finished)));
                }
                if finished {
                    return None;
                }
                // THE READ CLOCK: a body that goes quiet for the idle window is a
                // stalled stream — the connection is dropped with this stream and
                // the turn fails with the error row, instead of holding the lock
                // until the send ceiling (#177).
                let chunk = match within(stream_idle, inner.next()).await {
                    Ok(chunk) => chunk,
                    Err(stalled) => {
                        finished = true;
                        pending.push_back(Err(stalled.error(NAME, &st.endpoint)));
                        continue;
                    }
                };
                let Some(chunk_res) = chunk else {
                    finished = true;
                    st.end(&mut pending);
                    continue;
                };
                match chunk_res {
                    Ok(bytes) => st.buf.extend_from_slice(&bytes),
                    Err(_) => {
                        finished = true;
                        pending.push_back(Err(ProviderError::raised(format!(
                            "{NAME}: {} dropped the stream mid-answer; this turn is NOT \
                             complete",
                            st.endpoint
                        ))));
                        continue;
                    }
                }
                while let Some(nl) = st.buf.iter().position(|b| *b == b'\n') {
                    let line_bytes: Vec<u8> = st.buf.drain(..=nl).collect();
                    let line = String::from_utf8_lossy(&line_bytes[..line_bytes.len() - 1])
                        .trim_end_matches('\r')
                        .to_string();
                    if let Err(e) = st.decode_line(&line, &mut pending) {
                        finished = true;
                        pending.push_back(Err(e));
                        break;
                    }
                }
            }
        },
    ))
}

struct SseState {
    buf: Vec<u8>,
    wrap: ThinkWrap,
    report: ReportCell,
    usage: Arc<Mutex<Usage>>,
    /// The output tokens this stream has already billed to the agent's usage.
    billed_out: i64,
    stopped: bool,
    endpoint: String,
    /// `Some(bound)` on a MAIN turn, where a cut is a failure.
    main_turn_max: Option<i64>,
}

impl SseState {
    fn decode_line(
        &mut self,
        line: &str,
        out: &mut VecDeque<Result<ProviderEvent, ProviderError>>,
    ) -> Result<(), ProviderError> {
        let Some(rest) = line.strip_prefix("data:") else {
            return Ok(());
        };
        let data = rest.trim();
        if data.is_empty() {
            return Ok(());
        }
        let evt: Value = serde_json::from_str(data).map_err(|e| ProviderError {
            message: e.to_string(),
            kind: "JSONDecodeError",
        })?;
        match evt.get("type").and_then(Value::as_str).unwrap_or("") {
            "message_start" => {
                let u = &evt["message"]["usage"];
                let count = |k: &str| u.get(k).and_then(Value::as_i64).unwrap_or(0);
                let (input, write, read) = (
                    count("input_tokens"),
                    count("cache_creation_input_tokens"),
                    count("cache_read_input_tokens"),
                );
                let total = input + write + read;
                self.report.lock().expect("report poisoned").prompt_tokens =
                    (total > 0).then_some(total);
                let mut usage = self.usage.lock().expect("usage poisoned");
                usage.requests += 1;
                usage.input += input;
                usage.cache_write += write;
                usage.cache_read += read;
            }
            "content_block_delta" => {
                let delta = &evt["delta"];
                match delta.get("type").and_then(Value::as_str) {
                    Some("thinking_delta") => {
                        let t = delta.get("thinking").and_then(Value::as_str).unwrap_or("");
                        if !t.is_empty() {
                            if let Some(m) = self.wrap.open_marker() {
                                out.push_back(Ok(ProviderEvent::Token(m.to_string())));
                            }
                            out.push_back(Ok(ProviderEvent::Token(t.to_string())));
                        }
                    }
                    Some("text_delta") => {
                        let t = delta.get("text").and_then(Value::as_str).unwrap_or("");
                        if !t.is_empty() {
                            if let Some(m) = self.wrap.close_marker() {
                                out.push_back(Ok(ProviderEvent::Token(m.to_string())));
                            }
                            out.push_back(Ok(ProviderEvent::Token(t.to_string())));
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(r) = evt["delta"].get("stop_reason").and_then(Value::as_str) {
                    self.report.lock().expect("report poisoned").stop_reason = Some(r.to_string());
                }
                if let Some(n) = evt["usage"]
                    .get("output_tokens")
                    .and_then(Value::as_i64)
                    .filter(|n| *n > 0)
                {
                    self.report
                        .lock()
                        .expect("report poisoned")
                        .completion_tokens = Some(n);
                    // message_delta carries the running total: bill the step.
                    self.usage.lock().expect("usage poisoned").output += n - self.billed_out;
                    self.billed_out = n;
                }
            }
            "message_stop" => self.stopped = true,
            "error" => {
                let err = &evt["error"];
                return Err(ProviderError::raised(format!(
                    "{NAME}: {} stream error: {}: {}",
                    self.endpoint,
                    fantastic_bundle::py_str(err.get("type").unwrap_or(&Value::Null)),
                    fantastic_bundle::py_str(err.get("message").unwrap_or(&Value::Null)),
                )));
            }
            _ => {}
        }
        Ok(())
    }

    fn end(&mut self, out: &mut VecDeque<Result<ProviderEvent, ProviderError>>) {
        if let Some(m) = self.wrap.close_marker() {
            out.push_back(Ok(ProviderEvent::Token(m.to_string())));
        }
        if !self.stopped {
            out.push_back(Err(ProviderError::raised(format!(
                "{NAME}: the stream from {} ended without message_stop; this turn is NOT \
                 complete",
                self.endpoint
            ))));
            return;
        }
        let cut = self
            .report
            .lock()
            .expect("report poisoned")
            .stop_reason
            .as_deref()
            == Some("max_tokens");
        if let (true, Some(bound)) = (cut, self.main_turn_max) {
            out.push_back(Err(ProviderError::raised(format!(
                "{NAME}: the turn was cut at max_tokens={bound} (stop_reason=max_tokens) — it \
                 did not finish; raise max_tokens on the record, or lower thinking_budget"
            ))));
        }
    }
}

// ─── the record ─────────────────────────────────────────────────────────────

fn positive_int(v: Option<&Value>) -> Option<i64> {
    v.and_then(Value::as_i64).filter(|n| *n >= 1)
}

fn required_int(meta: &Map<String, Value>, key: &str) -> Result<i64, ProviderError> {
    positive_int(meta.get(key)).ok_or_else(|| {
        ProviderError::value_error(format!(
            "{NAME}: `{key}` is not on the agent record, a positive int, and was not read \
             from the API (update_agent <id> {key}=…)"
        ))
    })
}

fn build_provider(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Arc<dyn Provider> {
    let meta = helpers::agent_meta(agent_id, kernel);
    let built = (|| -> Result<AnthropicProvider, ProviderError> {
        Ok(AnthropicProvider {
            endpoint: helpers::required_meta(&meta, "endpoint", NAME)?
                .trim_end_matches('/')
                .to_string(),
            model: helpers::required_meta(&meta, "model", NAME)?,
            key: Arc::new(key_reader(agent_id, kernel, NAME, false)),
            max_tokens: required_int(&meta, "max_tokens")?,
            temperature: meta.get("temperature").and_then(Value::as_f64),
            thinking_budget: positive_int(meta.get("thinking_budget")),
            caching: caching::mode_of(&meta).map_err(ProviderError::value_error)?,
            usage: usage_of(agent_id),
            report: ReportCell::default(),
            stream_idle: stream_idle_of(&meta)?,
        })
    })();
    match built {
        Ok(p) => Arc::new(p),
        Err(e) => Arc::new(Unconfigured(e)),
    }
}

/// The counts with the record's prices applied (python `_usage_now`).
fn usage_now(agent_id: &AgentId, kernel: &Kernel) -> Value {
    let meta = helpers::agent_meta(agent_id, kernel);
    let price = |k: &str| meta.get(k).and_then(Value::as_f64);
    let counts = usage_of(agent_id);
    let usage = counts.lock().expect("usage poisoned");
    usage.snapshot(
        price("price_input_per_mtok"),
        price("price_output_per_mtok"),
    )
}

/// `usage` — args: reset? (bool). The agent's token usage since its kernel started.
/// `reset: true` returns the counts, then zeroes them. Python `_usage`.
fn usage_reply(agent_id: &AgentId, payload: &Value, kernel: &Kernel) -> Value {
    let reset = payload.get("reset").cloned().unwrap_or(json!(false));
    let Some(reset) = reset.as_bool() else {
        let mut r = fantastic_kernel::refusal::refusal(
            fantastic_kernel::refusal::BAD_ARG,
            "usage: reset must be true or false",
        );
        fantastic_kernel::refusal::with(&mut r, "arg", json!("reset"));
        return r;
    };
    let out = usage_now(agent_id, kernel);
    if reset {
        *usage_of(agent_id).lock().expect("usage poisoned") = Usage::default();
    }
    out
}

/// `reflect` — the shared base body, then what the next request will send, and the
/// usage so far.
fn reflect_reply(agent_id: &AgentId, kernel: &Kernel) -> Result<Value, BundleError> {
    let mut out = verbs::reflect_base(agent_id, kernel, SENTENCE, CFG.model_window);
    let meta = helpers::agent_meta(agent_id, kernel);
    for k in ["max_tokens", "temperature", "thinking_budget"] {
        let v = meta.get(k).cloned().unwrap_or(Value::Null);
        let v = if k == "temperature" && !v.is_null() {
            json!(py_float(&v)?)
        } else {
            v
        };
        out.insert(k.to_string(), v);
    }
    out.insert(
        "caching".into(),
        meta.get("caching")
            .cloned()
            .unwrap_or_else(|| json!(caching::DEFAULT_MODE)),
    );
    if let Some(row) = meta.get("cache_primer_until").filter(|v| !v.is_null()) {
        out.insert("cache_primer_until".into(), row.clone());
    }
    out.insert("usage".into(), usage_now(agent_id, kernel));
    Ok(Value::Object(out))
}

/// Refuse a write this backend cannot honour, AT THE WRITE, on the MERGED record —
/// then every required field, named at once. Python `validate_meta`, text for text.
#[must_use]
pub fn validate(meta: &Map<String, Value>, record: &Map<String, Value>) -> Option<String> {
    let mut merged = record.clone();
    for (k, v) in meta {
        merged.insert(k.clone(), v.clone());
    }
    for key in ["max_tokens", "context_window", "thinking_budget"] {
        if let Some(v) = meta.get(key).filter(|v| !v.is_null()) {
            if positive_int(Some(v)).is_none() || v.is_boolean() {
                return Some(format!("{key} must be a positive int"));
            }
        }
    }
    if let Some(t) = meta.get("temperature").filter(|v| !v.is_null()) {
        if !t.is_number() {
            return Some("temperature must be a number".to_string());
        }
    }
    for key in ["price_input_per_mtok", "price_output_per_mtok"] {
        if let Some(p) = meta.get(key).filter(|v| !v.is_null()) {
            if !p.is_number() || p.as_f64().unwrap_or(0.0) < 0.0 {
                return Some(format!("{key} must be a number, zero or more"));
            }
        }
    }
    if let Err(e) = stream_idle_of(meta) {
        return Some(e.message);
    }
    if let Some(budget) = positive_int(merged.get("thinking_budget")) {
        if budget < THINKING_FLOOR {
            return Some(format!(
                "thinking_budget={budget} is below the API's {THINKING_FLOOR}"
            ));
        }
        if let Some(mx) = positive_int(merged.get("max_tokens")) {
            if budget >= mx {
                return Some(format!(
                    "thinking_budget={budget} is not below max_tokens={mx} — the API refuses \
                     it; keep it below max_tokens"
                ));
            }
        }
    }
    let endpoint = merged
        .get("endpoint")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let left = server::missing(&merged);
    if !endpoint.is_empty() && !left.is_empty() && server::models_url(&endpoint).is_none() {
        return Some(format!(
            "{} is left to the API, but the endpoint '{endpoint}' does not end in /messages, \
             so the model list is not known — set {}",
            left[0],
            left.join(", ")
        ));
    }
    schema::validate(record_schema(), NAME, meta, record)
        .or_else(|| caching::refusal(meta, record, NAME, CFG.provider_cache))
}

/// THE RECORD SCHEMA — python `RECORD_SCHEMA`, field for field.
#[must_use]
pub fn record_schema() -> &'static Value {
    static SCHEMA: OnceLock<Value> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        const FROM_API: &str = "; left out, read from the API before a turn and written here";
        schema::schema(vec![
            Field::new(
                "endpoint",
                "str",
                "the Messages API URL, e.g. https://api.anthropic.com/v1/messages",
            )
            .required(),
            Field::new("model", "str", "the model id, e.g. claude-sonnet-5").required(),
            Field::new(
                "max_tokens",
                "int",
                format!("a turn's output budget (tokens){FROM_API} (the model's max_tokens)"),
            ),
            Field::new(
                "context_window",
                "int",
                format!("the model's input window (tokens){FROM_API} (max_input_tokens)"),
            ),
            Field::new(
                "temperature",
                "float",
                "sampling temperature; sent only when set",
            ),
            Field::new(
                "thinking_budget",
                "int",
                format!(
                    "extended thinking per turn (tokens, at least {THINKING_FLOOR}, below \
                     max_tokens); unset, no thinking is asked for"
                ),
            ),
            Field::new(
                "price_input_per_mtok",
                "float",
                "USD per million input tokens — with price_output_per_mtok, reflect and \
                 usage report cost_usd and saved_usd; no default price",
            ),
            Field::new(
                "price_output_per_mtok",
                "float",
                "USD per million output tokens — with price_input_per_mtok, reflect and \
                 usage report cost_usd and saved_usd; no default price",
            ),
            Field::new(
                "stream_idle_seconds",
                "float",
                format!(
                    "the read clock on the answer stream: seconds with no byte from the API \
                     (headers included) before the turn is judged stalled and ends with an \
                     error row; default {}",
                    fantastic_ai_core::constants::STREAM_IDLE_SECONDS
                ),
            ),
            // the API key — optional on the record: absent, no key header goes out
            schema::credentials_field(false),
        ])
    })
}

/// The Anthropic backend bundle.
pub struct AnthropicBundle;

#[async_trait]
impl Bundle for AnthropicBundle {
    fn name(&self) -> &str {
        NAME
    }

    fn verb_docs(&self) -> Value {
        // `usage` right after `history`, where python's build() merges extra_verbs.
        let core = fantastic_ai_core::verbs::verb_docs();
        let mut out = serde_json::Map::new();
        for (k, v) in core.as_object().cloned().unwrap_or_default() {
            let after = k == "history";
            out.insert(k, v);
            if after {
                out.insert("usage".into(), json!(USAGE_DOC));
            }
        }
        Value::Object(out)
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
            return Ok(Some(usage_reply(agent_id, payload, kernel)));
        }
        // A verb that runs a turn first reads what the record leaves to the API; an
        // API that does not answer refuses the verb, naming the field.
        if server::TURN_VERBS.contains(&verb) {
            if let Some(refused) = server::resolve(agent_id, kernel).await {
                return Ok(Some(refused));
            }
        }
        let provider = |_client: &str| build_provider(agent_id, kernel);
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
        // this agent's counts go with it: an id re-created later starts clean
        usage_table()
            .lock()
            .expect("usage table poisoned")
            .remove(agent_id.as_str());
        verbs::on_delete(agent_id, kernel).await;
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
