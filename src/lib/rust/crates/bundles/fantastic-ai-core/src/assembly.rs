//! Prompt assembly: render_reflect, the two how-tos, and the per-turn message
//! list.
//!
//! The default system block states the MECHANISM (how to call, grants, replies,
//! the state lines) and a few short working rules (act early, batch, read
//! narrowly, verify before `done`, a retry cap). NO agent menu and no tree: the
//! model reflects what it needs, and the block stays the same as the tree changes. It never
//! dictates a loop or a fixed procedure. A caller that wants a different role
//! supplies its own `system_prompt`, which REPLACES this block whole.
//!
//! The wording that dictated the loop stays out and is TESTED (`the_default_
//! prompt_states_the_mechanism_not_a_strategy`, mirroring python): "either call
//! again or write your final answer to finish", "ORIENT FIRST", "NEVER claim you
//! don't have access". Reusing that wording because it is "tuned" would quietly
//! void every emergence result taken afterwards.

use crate::caching::Caching;
use crate::memory::{load_history, MemoryStoreError};
use crate::model_format::map::{Env, Howto, Piece, SysBody, SysPiece};
use crate::model_format::{Decl, Section};
use crate::prompt_file::PromptFileError;
use crate::state::BackendState;
use fantastic_bundle::{py_split_whitespace, py_strip};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::Arc;

/// ONE reflect field as the model reads it — python `_render_reflect._one`.
///
/// NO JSON ON THE MODEL PATH: a string goes through as-is, a list of scalars is
/// comma-joined through `str()`, and anything nested is the REPLY NOTATION
/// collapsed onto the line — never JSON: python's says `parent_id=`,
/// `can=<memory><call>append,read</call></memory>` and `tree=kernel_state,web`.
fn reflect_field(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(items)
            if items
                .iter()
                .all(|x| !matches!(x, Value::Object(_) | Value::Array(_))) =>
        {
            items
                .iter()
                .map(crate::helpers::shown)
                .collect::<Vec<_>>()
                .join(",")
        }
        other => py_split_whitespace(&crate::tool_parse::render_reply(other)).join(" "),
    }
}

/// Render a reflect reply into a one-line `sentence  k=v  k=v` blurb.
///
/// `shift_remove`, not `remove`: with `preserve_order` on, `serde_json`'s
/// `remove` is a SWAP-remove, which would move the last key into `sentence`'s
/// slot and reorder every field the model reads.
pub fn render_reflect(v: &Value) -> String {
    let mut obj = match v.as_object() {
        Some(o) => o.clone(),
        None => return String::new(),
    };
    let sentence = obj
        .shift_remove("sentence")
        .map(|s| crate::helpers::shown(&s))
        .unwrap_or_default();
    let fields = obj
        .iter()
        .map(|(k, val)| format!("{k}={}", reflect_field(val)))
        .collect::<Vec<_>>()
        .join("  ");
    py_strip(&format!("{sentence}  {fields}")).to_string()
}

/// The fields of an agent's own reflect its system block shows — python `_SELF_KEYS`.
const SELF_KEYS: [&str; 3] = ["id", "description", "memory_store_id"];

/// THE PLACEHOLDERS a system prompt may hold, filled per agent and per turn —
/// so a prompt that REPLACES the default block keeps its
/// live parts: `{self}` this agent's identity and grants, `{store}` its memory
/// store id, `{howto}` the fixed how-to blocks. `{self}\n\n{howto}` IS the
/// default block, byte for byte. Only the
/// parts a prompt names are built; any other `{…}` stays as written. Python
/// `_PLACEHOLDER`.
pub const PLACEHOLDERS: [&str; 3] = ["self", "store", "howto"];

/// The placeholders `text` names (python `_PLACEHOLDER.findall`).
fn named(text: &str) -> Vec<&'static str> {
    PLACEHOLDERS
        .iter()
        .copied()
        .filter(|p| text.contains(&format!("{{{p}}}")))
        .collect()
}

const SEPARATOR: &str = "\n\n";

fn piece(part: &str, text: impl Into<String>) -> SysPiece {
    SysPiece {
        part: part.to_string(),
        body: SysBody::Text(text.into()),
    }
}

/// `text` with every placeholder replaced in ONE pass, so a filled part is never
/// scanned again (python `_PLACEHOLDER.sub`), told apart piece by piece: the text
/// between placeholders is `literal`, and each placeholder's value is the part it
/// names (`{store}` is the id, plain text).
fn fill(
    text: &str,
    parts: &HashMap<&'static str, SysBody>,
    literal: &'static str,
) -> Vec<SysPiece> {
    let mut out: Vec<SysPiece> = Vec::new();
    let mut run = String::new();
    let mut rest = text;
    'scan: while let Some(i) = rest.find('{') {
        run.push_str(&rest[..i]);
        let tail = &rest[i..];
        for p in PLACEHOLDERS {
            let token = format!("{{{p}}}");
            if tail.starts_with(&token) {
                if let Some(v) = parts.get(p) {
                    let named = match p {
                        "self" => Some("self_reflect"),
                        "howto" => Some("verb_docs"),
                        _ => None,
                    };
                    match (named, v) {
                        (Some(name), body) => {
                            out.push(piece(literal, std::mem::take(&mut run)));
                            out.push(SysPiece {
                                part: name.to_string(),
                                body: body.clone(),
                            });
                        }
                        (None, SysBody::Text(t)) => run.push_str(t),
                        (None, SysBody::Howto(_)) => {}
                    }
                    rest = &tail[token.len()..];
                    continue 'scan;
                }
            }
        }
        run.push('{');
        rest = &tail[1..];
    }
    run.push_str(rest);
    out.push(piece(literal, run));
    out
}

/// The live parts of the system block, built only where `need` names them —
/// python `_parts`. `{howto}` is the how-to as sections: the pass's type writes them
/// ([`crate::model_format::map::system`]), so a type switch maps the same block again.
fn parts(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    need: &[&str],
    env: Option<Env>,
    me: &Value,
) -> HashMap<&'static str, SysBody> {
    let mut out: HashMap<&'static str, SysBody> = HashMap::new();
    if need.contains(&"self") {
        out.insert("self", SysBody::Text(self_text(self_id, me)));
    }
    let meta = crate::helpers::agent_meta(self_id, kernel);
    let store = meta
        .get("memory_store_id")
        .and_then(Value::as_str)
        .map_or_else(|| "(none)".to_string(), str::to_string);
    if need.contains(&"store") {
        out.insert("store", SysBody::Text(store.clone()));
    }
    if need.contains(&"howto") {
        // OPTIONAL SECTIONS, gated on the record: a title only earns its line
        // when titles are what survives a stripped body, the async/rows how-to
        // only when replies arrive as rows (both exactly as python gates them),
        // and the `line_summary` line only when the record offers the call.
        // Teaching any unconditionally spends window on an affordance the agent
        // does not have.
        let flag = |k: &str| meta.get(k).and_then(Value::as_bool) == Some(true);
        // FIRST the environment (the date the thread began), then how to reach agents
        let mut sections = vec![
            Section::Env,
            Section::Send,
            Section::Reflect,
            Section::Context,
            Section::Memory,
        ];
        if flag("context_titles") {
            sections.push(Section::Titles);
        }
        if flag("context_async") {
            sections.push(Section::Async);
        }
        if flag(crate::line_summary::FIELD) {
            sections.push(Section::LineSummary);
        }
        // LAST: the working rules are what the model reads right before the task.
        sections.push(Section::Working);
        out.insert(
            "howto",
            SysBody::Howto(Howto {
                sections,
                store,
                env,
            }),
        );
    }
    out
}

/// A prompt with its placeholders filled for this agent and this turn — python
/// `_render_prompt`. `me`: the agent's own `reflect`.
fn render_prompt(
    text: &str,
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    literal: &'static str,
    env: Option<Env>,
    me: &Value,
) -> Vec<SysPiece> {
    let need = named(text);
    if need.is_empty() {
        return vec![piece(literal, text)];
    }
    fill(text, &parts(self_id, kernel, &need, env, me), literal)
}

/// The DEFAULT system block as `decl` writes it: `{self}\n\n{howto}` — this agent's own
/// reflect and the how-to sections its record earns. Nothing in it changes when the tree
/// does, nor with the clock (its date is the thread's first row's), so the cached prefix
/// holds for the whole conversation. Extracted so `context_recall` can MEASURE the same
/// block the next turn will carry — python's `_default_system_content`.
///
/// # Errors
/// The memory store refused the read of the thread's first row.
#[cfg(test)]
pub(crate) async fn default_system_content(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    decl: &Decl<'_>,
) -> Result<String, MemoryStoreError> {
    let env = thread_env(self_id, kernel).await?;
    let me = self_reflect(self_id, kernel).await;
    Ok(crate::model_format::map::system(
        decl,
        &default_pieces(self_id, kernel, env, &me),
    ))
}

/// TESTS: the agent's own `reflect`, read inline (a pass reads it as a step,
/// [`crate::steps::own_reflect`]).
#[cfg(test)]
pub(crate) async fn self_reflect(self_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    kernel
        .send(vec![fantastic_kernel::Item::new(
            self_id.clone(),
            json!({"type": "reflect", "tree": "none"}),
        )])
        .await
        .remove(0)
        .result
}

/// The default block as its pieces: this agent's own reflect and the how-to
/// sections, a blank line between.
fn default_pieces(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    env: Option<Env>,
    me: &Value,
) -> Vec<SysPiece> {
    let mut p = parts(self_id, kernel, &["self", "howto"], env, me);
    vec![
        SysPiece {
            part: "self_reflect".into(),
            body: p.remove("self").unwrap_or(SysBody::Text(String::new())),
        },
        piece("", SEPARATOR),
        SysPiece {
            part: "verb_docs".into(),
            body: p.remove("howto").unwrap_or(SysBody::Text(String::new())),
        },
    ]
}

/// `You are <id>. ` + this agent's own reflect `me`, cut to who it is.
pub(crate) fn self_text(self_id: &AgentId, me: &Value) -> String {
    let me = me.clone();
    // WHO YOU ARE, not how you are tuned: identity and memory — nothing that changes
    // during a run (grants change on every spawn; `reflect` reads them live). The rest of
    // the agent's own reflect (sampling, kv table, timeouts, endpoint) is operator
    // posture the model never acts on. Python `_SELF_KEYS`.
    let me = match me {
        Value::Object(m) => Value::Object(
            SELF_KEYS
                .iter()
                .filter_map(|k| m.get(*k).map(|v| ((*k).to_string(), v.clone())))
                .collect(),
        ),
        other => other,
    };
    format!("You are `{}`. {}", self_id, render_reflect(&me))
}

/// The first row of the current thread as the environment section reads it: the head's
/// first row (the store's first, or the newest `[cleared …]` row). `None`: an empty
/// thread (the arriving message is then the first row).
async fn thread_env(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
) -> Result<Option<Env>, MemoryStoreError> {
    Ok(load_history(self_id, kernel, false)
        .await?
        .first()
        .and_then(Env::of))
}

/// THE system block a turn uses, as `decl` writes it — the one choice
/// `assemble_messages` builds the turn on and `context_recall` sizes the body budget
/// against, so the two never drift (python `_system_content`). PRECEDENCE:
/// the per-call argument (this delivery) > the record's own `system_prompt` (read
/// LIVE, so an `update_agent` lands on the next turn — a main turn, a
/// self-delivery, a turn woken by a reply) > the file the record's
/// `system_prompt_file` names ([`crate::prompt_file`]) > the default block.
/// Whichever prompt is in force has its placeholders filled; none is ever
/// appended to another.
///
/// # Errors
/// The memory store failed, or a set `system_prompt_file` is unusable.
pub(crate) async fn system_content(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    decl: &Decl<'_>,
    me: &Value,
) -> Result<String, BlockFail> {
    let env = thread_env(self_id, kernel).await?;
    Ok(crate::model_format::map::system(
        decl,
        &system_pieces(self_id, kernel, env, me).await?,
    ))
}

/// The system block a turn uses, piece by piece: the block is their mapped texts end to
/// end. `env`: the first row of the thread.
pub(crate) async fn system_pieces(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    env: Option<Env>,
    me: &Value,
) -> Result<Vec<SysPiece>, PromptFileError> {
    // THE WRAP: the record's `pre_system_prompt` goes BEFORE
    // whichever prompt is in force and `post_system_prompt` AFTER it, each read
    // live and taken verbatim — no placeholders, and neither replaces anything.
    // Absent or null adds nothing (python `_system_content`).
    let meta = crate::helpers::agent_meta(self_id, kernel);
    let inner = inner_system_pieces(self_id, kernel, env, me).await?;
    let wrap = |key: &str| {
        crate::record::record_wrap_prompt(&meta, key).map(|s| vec![piece("system_text", s)])
    };
    let blank = |p: &SysPiece| match &p.body {
        SysBody::Text(t) => py_strip(t).is_empty(),
        SysBody::Howto(_) => false,
    };
    let mut out: Vec<SysPiece> = Vec::new();
    for group in [
        wrap("pre_system_prompt"),
        Some(inner),
        wrap("post_system_prompt"),
    ]
    .into_iter()
    .flatten()
    {
        if group.iter().all(blank) {
            continue;
        }
        if !out.is_empty() {
            out.push(piece("", SEPARATOR));
        }
        out.extend(group);
    }
    Ok(out)
}

/// The prompt in force, before the pre/post wrap of [`system_content`].
async fn inner_system_pieces(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    env: Option<Env>,
    me: &Value,
) -> Result<Vec<SysPiece>, PromptFileError> {
    if let Some(s) =
        crate::record::record_system_prompt(&crate::helpers::agent_meta(self_id, kernel))
    {
        return Ok(render_prompt(&s, self_id, kernel, "system_text", env, me));
    }
    // Set and unusable is an `Err` — the turn fails, never the default below.
    if let Some(s) = crate::prompt_file::text(self_id, kernel).await? {
        return Ok(render_prompt(&s, self_id, kernel, "prompt_file", env, me));
    }
    Ok(default_pieces(self_id, kernel, env, me))
}

/// Why a turn's block or view could not be built: the memory store failed, or a
/// set `system_prompt_file` is unusable. Either fails the turn, said so (python
/// `MemoryStoreError` / `PromptFileError`, both caught by `_send`).
#[derive(Debug)]
pub enum BlockFail {
    /// The memory store refused or is unreachable.
    Store(MemoryStoreError),
    /// `system_prompt_file` is set and cannot be the block.
    PromptFile(PromptFileError),
    /// A fold's summary would not fit its budget (a `context_recall` fold).
    FoldSummary(crate::strategies::fold::FoldSummaryError),
    /// A fold's anchor cannot be known (a `context_recall` fold).
    FoldAnchor(crate::folds::FoldAnchorError),
}

impl From<crate::strategies::fold::FoldFail> for BlockFail {
    fn from(e: crate::strategies::fold::FoldFail) -> Self {
        match e {
            crate::strategies::fold::FoldFail::Store(e) => Self::Store(e),
            crate::strategies::fold::FoldFail::Summary(e) => Self::FoldSummary(e),
            crate::strategies::fold::FoldFail::Anchor(e) => Self::FoldAnchor(e),
        }
    }
}

impl From<MemoryStoreError> for BlockFail {
    fn from(e: MemoryStoreError) -> Self {
        Self::Store(e)
    }
}

impl From<PromptFileError> for BlockFail {
    fn from(e: PromptFileError) -> Self {
        Self::PromptFile(e)
    }
}

impl From<BlockFail> for fantastic_kernel::bundle::BundleError {
    fn from(e: BlockFail) -> Self {
        match e {
            BlockFail::Store(e) => e.into(),
            BlockFail::PromptFile(e) => e.into(),
            BlockFail::FoldSummary(e) => e.into(),
            BlockFail::FoldAnchor(e) => e.into(),
        }
    }
}

/// Build a pass's message list: system block + the MODEL VIEW of the persisted
/// history, whose newest rows are the messages the pass reads (stored when they
/// arrived, #795), as pieces: the pass's type writes them
/// ([`crate::model_format::map`]). A caller-supplied `system_prompt` REPLACES
/// the default system block (a caller that wants a different role, or a different
/// convention, owns it whole); absent that, the record's own `system_prompt` does;
/// the default is self-identity + the how-to sections, and NO agent menu.
///
/// The conversation is ALWAYS this agent's own (its memory store): there is no
/// caller-supplied history. The environment section's date is the thread's first row's.
///
/// Returns `(messages, store_len)`. `store_len` is how many turns the head held
/// when this view was built, and it is NOT derivable from the messages: the
/// history is a deduped/trimmed VIEW (see [`crate::history_view`]), so counting
/// the view puts the compaction reaction cursor short by exactly the turns the
/// trimmers dropped.
#[cfg(test)]
pub(crate) async fn assemble_messages(
    self_id: &AgentId,
    state: &Arc<BackendState>,
    kernel: &Arc<Kernel>,
    client_id: &str,
    caching: Caching,
) -> Result<(Vec<Value>, usize), BlockFail> {
    let _ = client_id;
    let store = load_history(self_id, kernel, false).await?;
    let me = self_reflect(self_id, kernel).await;
    assemble_read(self_id, state, kernel, caching, store, &me)
        .await
        .map(|(messages, store_len, _)| (messages, store_len))
}

/// [`assemble_messages`] over `store`, the history the pass read (a step,
/// [`crate::steps::history`]), and `me`, its own `reflect`: the messages, the head's
/// length and the ids of the rows the history read held — a pass reads every message
/// stored by then, the ones that arrived after its tail was taken too.
pub(crate) async fn assemble_read(
    self_id: &AgentId,
    state: &Arc<BackendState>,
    kernel: &Arc<Kernel>,
    caching: Caching,
    store: Vec<Value>,
    me: &Value,
) -> Result<(Vec<Value>, usize, Vec<i64>), BlockFail> {
    // A trimmed VIEW, not a rewrite of the store: stale whole-state reads deduped,
    // identical bodies stubbed, and answered reflects reduced to a stub — the
    // model's own distillation carries the facts forward.
    let store_len = store.len();
    let read: Vec<i64> = store.iter().filter_map(|r| r.get("id").and_then(Value::as_i64)).collect();
    // THE DATE THE THREAD BEGAN: its first row's, which is the first message's; a first
    // row with no `ts` (an old or imported one) shows no date rather than the clock's. A
    // restart shows the same. An empty thread shows today's.
    let env = if store.is_empty() {
        crate::row_attrs::Stamp::now().env()
    } else {
        store.first().and_then(Env::of)
    };
    let pieces = system_pieces(self_id, kernel, env, me).await?;
    let mut messages = vec![json!({"role": "system", "sys": pieces})];
    // THE VIEW ARRAY IS THE STATE. When a projection (or a recall) has written
    // one, the body is exactly what it chose — each listed row at its recorded
    // resolution — plus only the rows that landed AFTER it, trimmed. Python
    // `_assemble`.
    let view = crate::memory::load_view(self_id, kernel).await?;
    let listed = listed_entries(&store, view.as_ref());
    // An EMPTY ids list is a `context_clear`'s view, not a stale one: the body is
    // the store from the clear on. Python `view is None or not view["ids"]`.
    let source = match (&listed, &view) {
        (Some(_), _) => "array",
        (None, None) => "store",
        (None, Some(v)) => {
            if v.get("ids")
                .and_then(Value::as_array)
                .is_some_and(|a| a.is_empty())
            {
                "store"
            } else {
                "store (stale array)"
            }
        }
    };
    *state.view_source.lock().expect("view_source poisoned") = Some(source.to_string());
    let rendered = render_history(&store, listed.as_deref());
    let pics = Pictures::of(self_id, state, kernel, &store, &rendered);
    // the thought markers waiting for the next user-side message
    let mut thoughts: Vec<Piece> = Vec::new();
    for r in rendered {
        let folded = if caching.folds() && r.res == "full" {
            crate::history_view::autofold(&store, r.idx, &r.msg)
        } else {
            crate::history_view::Autofolded {
                msg: r.msg,
                form: if r.res == "full" { "whole" } else { "anchor" },
                thought: None,
            }
        };
        let (msg, form) = (folded.msg, folded.form);
        // THE IMAGES OF THE ROW: whole, a line naming them, or alt text (#622)
        let (mut msg, form) = match crate::images::parts_of(&msg) {
            Some(parts) => {
                let how = pics.form_of(
                    r.idx,
                    msg.get("id").and_then(Value::as_i64),
                    parts,
                    form == "whole",
                );
                let info = store[r.idx]["attributes"]["image"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let id = msg.get("id").and_then(Value::as_i64);
                (crate::images::render(msg, &info, id, how), how.name())
            }
            None => (msg, form),
        };
        msg["form"] = json!(form);
        match msg.get("role").and_then(Value::as_str) {
            Some("assistant") => thoughts.extend(folded.thought),
            Some("user") if !thoughts.is_empty() => {
                msg = on_top(msg, std::mem::take(&mut thoughts));
                // a whole row under a thought marker: shown whole, its bytes the marker's more
                if form == "whole" {
                    msg["form"] = json!("thought");
                }
            }
            _ => {}
        }
        messages.push(msg);
    }
    Ok((messages, store_len, read))
}

/// `message` with `markers` at the top of what it shows.
fn on_top(message: Value, markers: Vec<Piece>) -> Value {
    let mut pieces = markers;
    pieces.extend(crate::model_format::map::pieces_of(&message));
    crate::model_format::map::with_pieces(message, &pieces)
}

/// What decides how the image rows of a request show: what the chat's model can take, which
/// rows the model has not answered yet, which a `recall` brought back, and how many images
/// the model takes in one request (newest first).
struct Pictures {
    sees: bool,
    limits: crate::images::Limits,
    /// Store indexes of the rows shown whole.
    whole: std::collections::HashSet<usize>,
    /// The ids a `recall` brought back for this request.
    recalled: Vec<i64>,
}

impl Pictures {
    fn of(
        self_id: &AgentId,
        state: &Arc<BackendState>,
        kernel: &Arc<Kernel>,
        store: &[Value],
        rendered: &[Rendered],
    ) -> Self {
        let meta = crate::helpers::agent_meta(self_id, kernel);
        let sees = crate::images::can(&meta, crate::images::NEEDS);
        let limits = crate::images::limits(&meta);
        let recalled =
            std::mem::take(&mut *state.recalled_images.lock().expect("recalled poisoned"));
        let mut out = Self {
            sees,
            limits,
            whole: Default::default(),
            recalled,
        };
        if !sees {
            return out;
        }
        // THE MODEL ANSWERED: the rows its newest answer read are anchors from now on —
        // up to that answer's parent; a message that arrived while it worked was not read
        // (#795)
        let answered = store.iter().rposition(|r| r["role"] == "assistant");
        let read_upto = answered.and_then(|a| store[a].get("parent").and_then(Value::as_i64));
        let mut room = limits.max_images.map(|n| n.max(0));
        for r in rendered.iter().rev() {
            let row = &store[r.idx];
            let Some(parts) = crate::images::parts_of(row) else {
                continue;
            };
            let unread = match (read_upto, row.get("id").and_then(Value::as_i64)) {
                (Some(upto), Some(id)) => id > upto,
                _ => answered.is_none_or(|a| r.idx > a),
            };
            let wanted = unread
                || row
                    .get("id")
                    .and_then(Value::as_i64)
                    .is_some_and(|id| out.recalled.contains(&id));
            let n = parts.len() as i64;
            if wanted && room.is_none_or(|left| n <= left) {
                room = room.map(|left| left - n);
                out.whole.insert(r.idx);
            }
        }
        out
    }

    /// The form a stored row's images take; `text_whole`: its text is shown as it is stored.
    fn form_of(
        &self,
        idx: usize,
        id: Option<i64>,
        parts: &[Value],
        text_whole: bool,
    ) -> crate::images::Form {
        use crate::images::Form;
        if !self.sees || !parts.iter().all(|p| crate::images::fits(p, self.limits)) {
            return Form::Alt;
        }
        if !text_whole || !self.whole.contains(&idx) {
            return Form::Anchor;
        }
        if id.is_some_and(|id| self.recalled.contains(&id)) {
            Form::Recalled
        } else {
            Form::Image
        }
    }
}

/// One part of a request: `{part, ref?, form?, bytes, hash}` — `bytes` the length
/// of the piece's text as the request carries it, `hash` the first 16 hex digits
/// of its SHA-256.
fn part_entry(part: &str, id: Option<i64>, form: Option<&str>, text: &str) -> Value {
    let mut e = json!({"part": part});
    if let Some(id) = id {
        e["ref"] = json!(id);
    }
    if let Some(form) = form {
        e["form"] = json!(form);
    }
    e["bytes"] = json!(text.len());
    e["hash"] = json!(crate::row_attrs::short_hash(text));
    e
}

/// THE PARTS OF THE REQUEST THAT IS SENT, measured on its bytes: `messages` in flight
/// and `sent`, the same messages as the pass's type wrote them (`decl`). The system
/// block piece by piece, each history message as a `row` with the form it took (`anchor`
/// when a strategy made it), the last message as the `new_input`.
pub(crate) fn request_parts(decl: &Decl<'_>, messages: &[Value], sent: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let last = sent.len().saturating_sub(1);
    for (i, (m, s)) in messages.iter().zip(sent).enumerate() {
        let shown = crate::model_format::map::Shown::of(m);
        if let crate::model_format::map::Body::System(sys) = &shown.body {
            for (part, text) in crate::model_format::map::system_parts(decl, sys) {
                if !part.is_empty() && !text.is_empty() {
                    out.push(part_entry(&part, None, None, &text));
                }
            }
            continue;
        }
        let text = s.get("content").and_then(Value::as_str).unwrap_or("");
        if i == last {
            out.push(part_entry(
                "new_input",
                None,
                m.get("image_form").and_then(Value::as_str),
                text,
            ));
            continue;
        }
        out.push(part_entry(
            "row",
            shown.id,
            Some(shown.form.as_deref().unwrap_or("anchor")),
            text,
        ));
    }
    out
}

/// One entry of the persisted view array, resolved against the store: the row's
/// index and the resolution it is shown at.
pub struct ViewEntry {
    /// Index of the row in the store.
    pub idx: usize,
    /// The resolution the row is shown at.
    pub res: String,
}

/// One message of the history part of the model view: the store row's index, the
/// resolution it is shown at, and the message the provider receives for it.
pub struct Rendered {
    /// Index of the row in the store.
    pub idx: usize,
    /// The resolution the row is shown at.
    pub res: String,
    /// The message the provider receives for the row.
    pub msg: Value,
}

/// The persisted view array's entries that name a row the store still has, in
/// array order. `None` when there is no usable array: none, an empty one, or a
/// stale one that names a row the store no longer has.
pub fn listed_entries(
    store: &[Value],
    view: Option<&Map<String, Value>>,
) -> Option<Vec<ViewEntry>> {
    let idx: HashMap<i64, usize> = store
        .iter()
        .enumerate()
        .filter_map(|(i, r)| r.get("id").and_then(Value::as_i64).map(|id| (id, i)))
        .collect();
    let ids = view?.get("ids")?.as_array()?;
    let mut out = Vec::with_capacity(ids.len());
    for e in ids {
        let id = e.get("id")?.as_i64()?;
        // a stale array names a row this store no longer has
        let &idx = idx.get(&id)?;
        out.push(ViewEntry {
            idx,
            res: e
                .get("res")
                .and_then(Value::as_str)
                .unwrap_or("full")
                .to_string(),
        });
    }
    (!out.is_empty()).then_some(out)
}

/// The history part of the model view (no system block, no user turn), as
/// `assemble_messages` sends it: the listed entries each at its recorded
/// resolution, then every row after the newest of them as stored; with no listed
/// array, the whole store as stored. Shared with `context_fold`, which persists
/// exactly what this renders. A `context_clear` starts a new thread, and the old
/// rows leave the view with it.
pub fn render_history(store: &[Value], listed: Option<&[ViewEntry]>) -> Vec<Rendered> {
    let Some(entries) = listed else {
        return recall_first(store, rows_from(store, 0));
    };
    let mut out: Vec<Rendered> = entries
        .iter()
        .map(|e| Rendered {
            idx: e.idx,
            res: e.res.clone(),
            msg: crate::view::apply_res(&store[e.idx], &e.res),
        })
        .collect();
    let newest = entries.iter().map(|e| e.idx).max().unwrap_or(0);
    out.extend(rows_from(store, newest + 1));
    recall_first(store, out)
}

/// THE MESSAGES A PASS READS COME LAST (#795): they are stored when they arrive, so the
/// `recall` row the pass's owed recall writes lands after them. Between two answers,
/// each `recall` row is shown before the other rows, on every request alike, so the
/// request ends with the messages and its prefix stays the same from pass to pass.
fn recall_first(store: &[Value], rows: Vec<Rendered>) -> Vec<Rendered> {
    let is = |r: &Rendered, kind: &str| store[r.idx].get("kind").and_then(Value::as_str) == Some(kind);
    let mut out = Vec::with_capacity(rows.len());
    let mut span: Vec<Rendered> = Vec::new();
    let flush = |span: &mut Vec<Rendered>, out: &mut Vec<Rendered>| {
        let (recalls, rest): (Vec<Rendered>, Vec<Rendered>) =
            std::mem::take(span).into_iter().partition(|r| is(r, "recall"));
        out.extend(recalls);
        out.extend(rest);
    };
    for r in rows {
        if store[r.idx].get("role").and_then(Value::as_str) == Some("assistant") {
            flush(&mut span, &mut out);
            out.push(r);
        } else {
            span.push(r);
        }
    }
    flush(&mut span, &mut out);
    out
}

/// The rows of `store` from `from` on, each with its store index, as stored.
fn rows_from(store: &[Value], from: usize) -> Vec<Rendered> {
    store
        .iter()
        .enumerate()
        .skip(from)
        .map(|(idx, row)| Rendered {
            idx,
            res: crate::view::res_of(row).to_string(),
            msg: row.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests_worknote {
    #[test]
    fn the_context_howto_teaches_one_turn_thinking_and_anchors() {
        let h = crate::model_format::default::HOWTO_CONTEXT;
        assert!(h.contains("Your thinking lasts one turn"));
        assert!(h.contains("Do not repeat a call whose result you still have or noted"));
    }

    #[test]
    fn the_working_section_asks_to_keep_the_task_state_in_the_reply() {
        let h = crate::model_format::default::HOWTO_WORKING
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let h = h.as_str();
        assert!(
            h.starts_with("## Working Your thinking lasts one answer; your reply's own text stays.")
        );
        assert!(h.contains("Keep the task's state there, in a few plain lines before your calls (plain text, not a call or an agent)"));
        assert!(h.contains("what was asked and how you will know it is done; the steps and which one you are on; what you found, with where you found it; what you tried that failed; and your next step."));
        assert!(h.contains("A step counts as done only once its result is among what you found"));
        assert!(h.contains("think only about the next step"));
        // the sections run in the agreed order
        let s = crate::model_format::default::HOWTO_SEND;
        assert!(!s.contains("## Working"));
        let order = [
            "## Reaching",
            "## Lines",
            "## Ending an answer",
            "## Replies",
            "## How to call",
            "## Inquiries",
        ];
        let at: Vec<usize> = order.iter().map(|k| s.find(k).unwrap()).collect();
        assert!(at.windows(2).all(|p| p[0] < p[1]), "{at:?}");
    }

    #[test]
    fn the_send_howto_says_act_early_batch_and_verify_before_done() {
        // (the text wraps lines; compare on single spaces)
        let h = format!(
            "{} {}",
            crate::model_format::default::HOWTO_SEND,
            crate::model_format::default::HOWTO_WORKING
        )
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
        let h = h.as_str();
        assert!(h.contains("Read only what the request names; no docs or reflect sweep before your first real call."));
        assert!(h.contains("an error reply names the missing argument"));
        assert!(h.contains("One answer may hold many <call> elements; they run at once."));
        assert!(h.contains("a call that does goes in a later answer, after that reply arrives"));
        assert!(h.contains(
            "Before `done`: for each check that shows the task is done, the reply that proves it."
        ));
        assert!(h.contains("Checks are run, not re-read"));
        assert!(
            h.contains("`done` (the work is done, checked as under Working)")
                || h.contains("checked as under Working")
        );
        assert!(h.contains("three times: stop"));
        assert!(h.contains("send whoever asked you an `inquiry`"));
        assert!(h.contains("Other text outside calls: only your last message."));
    }
}
