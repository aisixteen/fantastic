//! Prompt assembly: render_reflect, the two how-tos, and the per-turn message
//! list.
//!
//! The default system block states the MECHANISM and NO strategy. NO agent menu
//! (the primer's id-index is the tree, and the model reflects what it needs);
//! nothing about looping, orienting, iterating or finishing. A caller that wants
//! a different role supplies its own `system_prompt`, which REPLACES this block
//! whole.
//!
//! That line is load-bearing and it is TESTED (`the_default_prompt_states_the_
//! mechanism_not_a_strategy`, mirroring python). The removed strategy prompt said
//! "either call again or write your final answer to finish", "ORIENT FIRST",
//! "NEVER claim you don't have access" — that is the loop, dictated. Reusing that
//! wording because it is "tuned" would quietly void every emergence result taken
//! afterwards.

use crate::caching::Caching;
use crate::history_view::{dedupe_repeat_bodies, dedupe_stale_reads, drop_read_docs};
use crate::memory::{load_history, MemoryStoreError};
use crate::prompt_file::PromptFileError;
use crate::state::BackendState;
use fantastic_bundle::{py_split_whitespace, py_str, py_strip, py_truthy};
use fantastic_kernel::{AgentId, Item, Kernel};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::Arc;

/// ONE reflect field as the model reads it — python `_render_reflect._one`.
///
/// NO JSON ON THE MODEL PATH: a string goes through as-is, a list of scalars is
/// comma-joined through `str()`, and anything nested is the REPLY NOTATION
/// collapsed onto the line — never JSON: python's says `parent_id=`,
/// `can_call=<$self>*</$self>` and `tree=kernel_state,web`.
fn reflect_field(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(items)
            if items
                .iter()
                .all(|x| !matches!(x, Value::Object(_) | Value::Array(_))) =>
        {
            items.iter().map(py_str).collect::<Vec<_>>().join(",")
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
        .map(|s| py_str(&s))
        .unwrap_or_default();
    let fields = obj
        .iter()
        .map(|(k, val)| format!("{k}={}", reflect_field(val)))
        .collect::<Vec<_>>()
        .join("  ");
    py_strip(&format!("{sentence}  {fields}")).to_string()
}

/// How to reach another agent — the ENVELOPE this crate actually dispatches (see
/// [`crate::route::route_reply`]).
///
/// Lands in the default system block only because routing exists: a prompt that
/// teaches an envelope nothing dispatches is strictly worse than no prompt at all,
/// so this text and the router are one change.
///
/// States facts, never a strategy: how to address an agent, what `reflect`
/// returns, where `reply_to` sends the answer, how to recognise a reply. It does
/// not say to loop, to keep going, or how to decide it is finished — stopping is
/// emitting no call, which costs nothing and needs no instruction.
pub const SEND_TO_HOWTO: &str = r#"## Reaching other agents
Everything is an agent with a string id; `send` reaches any of them. A call is a
`<call>` element, one child tag per argument — no JSON, no quotes, no escaping:
<call name="send">
<target_id>notes</target_id>
<type>reflect</type>
</call>
`<target_id>` names the agent, `<type>` the verb, every other child is an argument. A
value is text as written (`true`, `12`, a path); a list is `<item>` children
(`<ids><item>12</item><item>15</item></ids>`); an object is child tags
(`<edits><item><path>a.py</path><hunks>2</hunks></item></edits>`); a map's keys are its
tags, `$` included (`<callable_by><$self>*</$self><web><item>read</item></web></callable_by>`).
`<reply_to>` picks
where the reply goes: you by default (your next message), another id hands it to them,
`null` discards it. `reflect` goes to the agent itself (`<target_id>runner</target_id>`)
and says what it IS (sentence, state, tree); how to CALL it is `docs` (below). Text outside calls is your message to whoever asked — keep it
brief, and take instructions literally. EMIT CALLS BEFORE YOU DELIBERATE: the moment you
know the next call, send it; never end a turn on "let me do X" without X in a `<call>`.

## Grants — every call is DENY-ALL
You may call only what your `can_call` (in your self block) grants: `{key: "*" | [verbs]}`,
a key being an agent id, `$self`, `$parent` or `$subtree` (every agent below you), `*`
every verb; the target must also admit you (its `callable_by`). Read `can_call` BEFORE
your first call and plan inside it — any other call is refused and costs a turn; never
retry a refusal. `docs` too is a grant: `docs` on the root (`kernel`).

## `<bytes>` is VERBATIM
A verb that takes a body (a file, a patch, a program) names it `bytes`: write the text
between `<bytes>` and `</bytes>` exactly as it should land — real newlines and quotes,
nothing escaped, no fence. The body ends at the first `</bytes>`.
<call name="send">
<target_id>notes</target_id>
<type>write</type>
<path>notes.md</path>
<bytes>line one
line two, with "quotes"
</bytes>
</call>

## How to call an agent — `docs`
<call name="send">
<target_id>kernel</target_id>
<type>docs</type>
<of>notes</of>
</call>
`docs` is a KERNEL verb: the target is always `kernel` and the agent you ask about goes
in `<of>` (an agent id — `<guid>|<agent>` for one on a connected kernel — a bundle by its
listed name, or `kernel` for create_agent, update_agent, ps, …); it answers one signature
per verb. It is the only verb that takes
`<of>` — every other verb, `reflect` included, goes to the agent itself. Add `<verb>write</verb> <tier>detailed</tier>` for that verb's
full text. Pull docs once per kind of agent, `detailed` only for a verb you are about to
use. A docs reply is TRANSIENT: after your next message it becomes a
one-line stub — act on it then, or ask again. Reflect only agents you are granted; to see just part of a tree, reflect with
`<show_subtrees><item>id</item></show_subtrees>` (only those branches) or
`<ignore_subtrees><item>id</item></ignore_subtrees>` (all but those, each shown as its
id and `pruned: <n>`).

## Several recipients
`<to><item>a</item><item>b</item></to>` in place of `<target_id>` sends each a copy
headed `[from <you> · to a, b · msg N]` (N = the sending turn, the same in every copy).
Answer a group with the SAME `<to>` (minus yourself) and `<re>N</re>`; fewer recipients
fork a private thread. Tell peers facts in words — your ids mean nothing to them.

## Turns
Only your ANSWER acts: a `<call>` written in your thinking does not run — write it
in your answer, after the thinking; every call in the answer runs. Thinking is stripped
from history: put what you concluded into a call or your message. End every turn with a
call, with `<call name="await"></call>`
(stop until the next message wakes you — what you wait for is in flight, not lost;
refused when nothing of yours is pending) or with `<call name="done"></call>` (the work
is finished). A turn with no call is reminded and rerolled; turn after turn of none and
the chain gives up, telling your caller you could not answer.

## Inquiries
When a decision is not yours to make, `send` whoever owns it an `inquiry` (`<text>`,
`<kind>text|single|multi|confirm</kind>`, `<options>`). It is accepted at once; the
answer arrives LATER as a message under that call's pid — a person reads it first, an
agent does its work first: keep working, or `<call name="await"></call>` for it. A human is an agent too —
find it among the agents your `can_call` grants. Asked one yourself (`[inquiry from …]`),
do the work it needs, then answer with
<call name="inquiry_answer"><answer>what you concluded</answer></call>
— it dispatches nothing, and nothing else counts as your answer.

## Replies
Every call runs under a pid; its reply comes back tagged `[reply to <pid> · <agent>
<verb>]`. The calls of one turn run CONCURRENTLY (no order — if B needs A, send B after
A's reply) and are answered TOGETHER in one later message, one block each (`N of M in
batch …`; M is complete). Messages queued earlier arrive first: an unseen reply is in
flight — never re-issue it; send `kernel` `<type>ps</type>` to check. A reply whose pid
you do not know was routed to you — use it. Per call: `<timeout>seconds</timeout>`
bounds it; `<exec_id>name</exec_id>` makes it idempotent (the same exec_id never runs
twice — you get the first outcome back; a new attempt needs a new name);
`<retries>N</retries>` lets the kernel re-run a failing call; `kernel`
`<type>ledger</type>` lists your executions and what became of them.
"#;

/// The titles how-to, appended when the record sets `context_titles`. A title
/// rides on the reply's tag and is what REMAINS once the body is stripped.
pub const TITLES_HOWTO: &str = r#"## Titling your calls
Every call carries a one-sentence `<title>` beside its `<target_id>`:
<call name="send">
<target_id>notes</target_id>
<title>read filler_2 — looking for the audit marker</title>
<type>read</type>
<path>filler_2.txt</path>
</call>
The title rides on the reply's tag and is what remains once the reply BODY is stripped
from your view (bodies stay fetchable by pid). Title the INTENT ("read filler_2 — looking
for the audit marker"), and after a reply SAY WHAT IT TOLD YOU in one sentence — the body
will go; your sentence will not.
"#;

/// The FULL-ASYNC how-to, gated on the record's `context_async` — taught only
/// when replies really do arrive as rows. Python `_ASYNC_HOWTO`, verbatim.
pub const ASYNC_HOWTO: &str = r#"## Replies are rows, not turns
A reply is DATA, not a message to answer. A LARGE reply arrives as a one-line digest —
tag, your title, size, and its ORIGIN (how to get it again):
[reply to <pid> · notes read · "survey filler_2"]
(settled, 6812 bytes — NOT inlined, held in memory only until this agent restarts. origin: notes notes.md lines 1-200 v3f2a — read {path, range}. To find a line in it: send yourself `<type>grep</type> <query>substring</query> <ids><item>pid</item></ids>`)
GREP FIRST: `<type>grep</type> <query>substring</query>` on yourself finds the line —
in a body held this run, a folded stretch, an old turn, a row shared with you — with
its id and line. BODIES ARE NOT KEPT: after a restart only the origin line remains, so
re-read or re-run what you still need; never answer from a body you cannot see.
FOLDS: when your window fills, finished stretches collapse into `[fold <id> · N refs]
"<title>"` plus a short summary. Nothing is deleted — `<type>history</type>
<fold>id</fold>` lists a fold's members, grep with the fold's id searches inside it.
After a fold, and on your first turn after a boot, `context_recall` fits the head of
your conversation to your budget and asks you what to keep in full; a `[recall …]` line
is your own earlier decision and names the LIVE task. You may call
`<type>context_recall</type>` on yourself any time. `<type>context_clear</type>` on yourself
starts a NEW THREAD: your view begins at the `[cleared …]` line; every earlier row is
that clear's member, reachable through grep, recall and `history fold`.
"#;

/// The memory how-to — python `_MEMORY_HOWTO`, verbatim, `{store}` filled in with
/// the record's `memory_store_id` (or `(none)`).
pub const MEMORY_HOWTO: &str = r#"## Your memory
Your conversation lives in your MEMORY STORE `{store}` — every row under YOUR key.
"#;

/// The context-window how-to, appended to the DEFAULT system block. Context is
/// the backend's (it owns the conversation), so it teaches the recall /
/// context_status affordances alongside [`SEND_TO_HOWTO`]'s addressing.
pub const CONTEXT_HOWTO: &str = r#"## Context window
Your view is finite: when it overflows, older turns are compacted and a
`[context-notice]` says so; the durable transcript stays whole. Send your OWN id
`<type>recall</type>` (`<query>`, `<limit>`) to page dropped turns back, and
`<type>context_status</type>` for your budget. Facts that must outlive this window
(names, decisions, preferences) belong in a memory agent.
"#;

/// The fields of an agent's own reflect its system block shows — python `_SELF_KEYS`.
const SELF_KEYS: [&str; 5] = [
    "id",
    "description",
    "memory_store_id",
    "can_call",
    "callable_by",
];

/// THE PLACEHOLDERS a system prompt may hold, filled per agent and per turn —
/// so a prompt that REPLACES the default block keeps its
/// live parts: `{tree}` the kernel primer, `{self}` this agent's identity and
/// grants, `{store}` its memory store id, `{howto}` the fixed how-to blocks.
/// `{tree}\n\n{self}\n\n{howto}` IS the default block, byte for byte. Only the
/// parts a prompt names are built; any other `{…}` stays as written. Python
/// `_PLACEHOLDER`.
pub const PLACEHOLDERS: [&str; 4] = ["tree", "self", "store", "howto"];

/// The placeholders `text` names (python `_PLACEHOLDER.findall`).
fn named(text: &str) -> Vec<&'static str> {
    PLACEHOLDERS
        .iter()
        .copied()
        .filter(|p| text.contains(&format!("{{{p}}}")))
        .collect()
}

/// `text` with every placeholder replaced in ONE pass, so a filled part is never
/// scanned again (python `_PLACEHOLDER.sub`).
fn fill(text: &str, parts: &HashMap<&'static str, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    'scan: while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        for p in PLACEHOLDERS {
            let token = format!("{{{p}}}");
            if tail.starts_with(&token) {
                if let Some(v) = parts.get(p) {
                    out.push_str(v);
                    rest = &tail[token.len()..];
                    continue 'scan;
                }
            }
        }
        out.push('{');
        rest = &tail[1..];
    }
    out.push_str(rest);
    out
}

/// The live parts of the system block, built only where `need` names them —
/// python `_parts`.
async fn parts(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    need: &[&str],
) -> HashMap<&'static str, String> {
    let mut out: HashMap<&'static str, String> = HashMap::new();
    if need.contains(&"tree") {
        // Lean primer: a flat id-index of the tree + the bundle catalog by name.
        let primer = kernel
            .send(vec![Item::new(
                AgentId::from("kernel"),
                json!({"type": "reflect", "tree": "ids", "bundles": "ids"}),
            )])
            .await
            .remove(0)
            .result;
        out.insert("tree", render_reflect(&primer));
    }
    if need.contains(&"self") {
        out.insert("self", self_part(self_id, kernel).await);
    }
    let meta = crate::helpers::agent_meta(self_id, kernel);
    let store = meta
        .get("memory_store_id")
        .filter(|v| py_truthy(v))
        .map_or_else(|| "(none)".to_string(), py_str);
    if need.contains(&"store") {
        out.insert("store", store.clone());
    }
    if need.contains(&"howto") {
        // TWO OPTIONAL SECTIONS, gated on the record exactly as python gates
        // them: a title only earns its line when titles are what survives a
        // stripped body, and the async/rows how-to only when replies arrive
        // as rows. Teaching either unconditionally spends window on an
        // affordance the agent does not have.
        // PYTHON TRUTHINESS (`if rec_.get(...)`): `1`/`"on"` turn the block on.
        let flag = |k: &str| meta.get(k).is_some_and(py_truthy);
        let mut blocks = vec![
            SEND_TO_HOWTO.to_string(),
            CONTEXT_HOWTO.to_string(),
            MEMORY_HOWTO.replace("{store}", &store),
        ];
        if flag("context_titles") {
            blocks.push(TITLES_HOWTO.to_string());
        }
        if flag("context_async") {
            blocks.push(ASYNC_HOWTO.to_string());
        }
        out.insert("howto", blocks.join("\n\n"));
    }
    out
}

/// A prompt with its placeholders filled for this agent and this turn — python
/// `_render_prompt`.
async fn render_prompt(text: &str, self_id: &AgentId, kernel: &Arc<Kernel>) -> String {
    let need = named(text);
    if need.is_empty() {
        return text.to_string();
    }
    fill(text, &parts(self_id, kernel, &need).await)
}

/// The DEFAULT system block: `{tree}\n\n{self}\n\n{howto}` — the lean primer,
/// this agent's own reflect, and the how-to sections its record earns. Extracted
/// so `context_recall` can MEASURE the same block the next turn will carry —
/// python's `_default_system_content`.
pub async fn default_system_content(self_id: &AgentId, kernel: &Arc<Kernel>) -> String {
    let p = parts(self_id, kernel, &["tree", "self", "howto"]).await;
    [&p["tree"], &p["self"], &p["howto"]]
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// `You are <id>. ` + this agent's own reflect, cut to who it is.
async fn self_part(self_id: &AgentId, kernel: &Arc<Kernel>) -> String {
    let me = kernel
        .send(vec![Item::new(
            self_id.clone(),
            json!({"type": "reflect", "tree": "none"}),
        )])
        .await
        .remove(0)
        .result;
    // WHO YOU ARE, not how you are tuned: identity, memory and grants. The rest of
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

/// THE system block a turn uses — the one choice `assemble_messages` builds the
/// turn on and `context_recall` sizes the body budget against, so the two never
/// drift (python `_system_content`). PRECEDENCE:
/// the per-call argument (this delivery) > the record's own `system_prompt` (read
/// LIVE, so an `update_agent` lands on the next turn — a main turn, a
/// self-delivery, a turn woken by a reply) > the file the record's
/// `system_prompt_file` names ([`crate::prompt_file`]) > the default block.
/// Whichever prompt is in force has its placeholders filled; none is ever
/// appended to another.
pub async fn system_content(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    system_prompt: Option<&str>,
) -> Result<String, PromptFileError> {
    // THE WRAP: the record's `pre_system_prompt` goes BEFORE
    // whichever prompt is in force and `post_system_prompt` AFTER it, each read
    // live and taken verbatim — no placeholders, and neither replaces anything.
    // Absent or null adds nothing (python `_system_content`).
    let meta = crate::helpers::agent_meta(self_id, kernel);
    let inner = inner_system_content(self_id, kernel, system_prompt).await?;
    let pre = crate::schema::record_wrap_prompt(&meta, "pre_system_prompt");
    let post = crate::schema::record_wrap_prompt(&meta, "post_system_prompt");
    Ok([pre, Some(inner), post]
        .into_iter()
        .flatten()
        .filter(|s| !py_strip(s).is_empty())
        .collect::<Vec<_>>()
        .join("\n\n"))
}

/// The prompt in force, before the pre/post wrap of [`system_content`].
async fn inner_system_content(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    system_prompt: Option<&str>,
) -> Result<String, PromptFileError> {
    if let Some(s) = system_prompt.filter(|s| !s.trim().is_empty()) {
        return Ok(render_prompt(s, self_id, kernel).await);
    }
    if let Some(s) =
        crate::schema::record_system_prompt(&crate::helpers::agent_meta(self_id, kernel))
    {
        return Ok(render_prompt(&s, self_id, kernel).await);
    }
    // Set and unusable is an `Err` — the turn fails, never the default below.
    if let Some(s) = crate::prompt_file::text(self_id, kernel).await? {
        return Ok(render_prompt(&s, self_id, kernel).await);
    }
    Ok(default_system_content(self_id, kernel).await)
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

/// Build the per-turn message list: system block + the MODEL VIEW of the
/// persisted history + the user turn. A caller-supplied `system_prompt` REPLACES
/// the default system block (a caller that wants a different role, or a different
/// convention, owns it whole); absent that, the record's own `system_prompt` does;
/// the default is a lean primer + self-identity +
/// [`SEND_TO_HOWTO`] + [`CONTEXT_HOWTO`] + [`MEMORY_HOWTO`], and NO agent menu.
///
/// The conversation is ALWAYS this agent's own (its memory store): there is no
/// caller-supplied history.
///
/// Returns `(messages, store_len)`. `store_len` is how many turns the head held
/// when this view was built, and it is NOT derivable from the messages: the
/// history is a deduped/trimmed VIEW (see [`crate::history_view`]), so counting
/// the view puts the compaction reaction cursor short by exactly the turns the
/// trimmers dropped.
pub async fn assemble_messages(
    self_id: &AgentId,
    state: &Arc<BackendState>,
    user_text: &str,
    kernel: &Arc<Kernel>,
    _client_id: &str,
    system_prompt: Option<&str>,
    caching: Caching,
) -> Result<(Vec<Value>, usize), BlockFail> {
    let system_content = system_content(self_id, kernel, system_prompt).await?;
    let mut messages = vec![json!({"role": "system", "content": system_content})];
    // A trimmed VIEW, not a rewrite of the store: stale whole-state reads deduped,
    // identical bodies stubbed, and answered reflects reduced to a stub — the
    // model's own distillation carries the facts forward.
    let store = load_history(self_id, kernel, false).await?;
    let store_len = store.len();
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
    messages.extend(
        render_history(&store, listed.as_deref(), caching.cleans())
            .into_iter()
            .map(|r| r.msg),
    );
    messages.push(json!({"role": "user", "content": user_text}));
    Ok((messages, store_len))
}

/// One entry of the persisted view array, resolved against the store: the row's
/// index, the resolution it is shown at, and the body a trimmer left on it (absent,
/// the row renders as its resolution gives it).
pub struct ViewEntry {
    /// Index of the row in the store.
    pub idx: usize,
    /// The resolution the row is shown at.
    pub res: String,
    /// The trimmed body, when a trimmer left one.
    pub content: Option<String>,
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
            content: e.get("content").and_then(Value::as_str).map(str::to_string),
        });
    }
    (!out.is_empty()).then_some(out)
}

/// The history part of the model view (no system block, no user turn), as
/// `assemble_messages` sends it: the listed entries each at its recorded
/// resolution, then the rows after the newest of them with the trimmers applied;
/// with no listed array, the whole store with the trimmers applied. Shared with
/// `context_fold`, which persists exactly what this renders.
///
/// `cleanup` is the record's `caching` cleanup (see [`crate::caching::Caching::cleans`]).
/// Off, the view is append-only: no trimmer runs, and a trimmed body an earlier
/// cleanup left on a listed entry is ignored, so every row renders as stored.
///
/// The listed array is the record of what the view is, whatever mode wrote it. A row
/// an earlier cleanup left out of a listed view stays out after a switch to `none`
/// or `provider_cache`: only the rows after the newest listed entry render as stored.
/// A `context_clear` starts a new thread, and the old rows leave the view with it.
pub fn render_history(
    store: &[Value],
    listed: Option<&[ViewEntry]>,
    cleanup: bool,
) -> Vec<Rendered> {
    let Some(entries) = listed else {
        return trim_tail(store, 0, cleanup);
    };
    let mut out: Vec<Rendered> = entries
        .iter()
        .map(|e| {
            let row = &store[e.idx];
            let msg = match &e.content {
                Some(c) if cleanup => crate::history_view::with_content(row, c.clone()),
                _ => crate::view::apply_res(row, &e.res),
            };
            Rendered {
                idx: e.idx,
                res: e.res.clone(),
                msg,
            }
        })
        .collect();
    let newest = entries.iter().map(|e| e.idx).max().unwrap_or(0);
    out.extend(trim_tail(store, newest + 1, cleanup));
    out
}

/// The rows of `store` from `from` on, each with its store index. With `cleanup`
/// they run through the three trimmers, and a trimmed row keeps its id while a
/// dropped row is simply absent. Without it, every row is kept as stored, in order.
fn trim_tail(store: &[Value], from: usize, cleanup: bool) -> Vec<Rendered> {
    if !cleanup {
        return store
            .iter()
            .enumerate()
            .skip(from)
            .map(|(idx, row)| Rendered {
                idx,
                res: crate::view::res_of(row).to_string(),
                msg: row.clone(),
            })
            .collect();
    }
    let idx: HashMap<i64, usize> = store
        .iter()
        .enumerate()
        .filter_map(|(i, r)| r.get("id").and_then(Value::as_i64).map(|id| (id, i)))
        .collect();
    let tail: Vec<Value> = store[from..].to_vec();
    drop_read_docs(&dedupe_stale_reads(&dedupe_repeat_bodies(&tail)))
        .into_iter()
        .map(|msg| Rendered {
            idx: msg
                .get("id")
                .and_then(Value::as_i64)
                .and_then(|id| idx.get(&id).copied())
                .expect("a trimmed row keeps its store id"),
            res: crate::view::res_of(&msg).to_string(),
            msg,
        })
        .collect()
}
