//! Prompt assembly: render_reflect, the two how-tos, and the per-turn message
//! list.
//!
//! The default system block states the MECHANISM (how to call, grants, replies,
//! the state lines) and a few short working rules (act early, batch, read
//! narrowly, verify before `done`, a retry cap). NO agent menu (the primer's
//! id-index is the tree, and the model reflects what it needs). It never
//! dictates a loop or a fixed procedure. A caller that wants a different role
//! supplies its own `system_prompt`, which REPLACES this block whole.
//!
//! The wording that dictated the loop stays out and is TESTED (`the_default_
//! prompt_states_the_mechanism_not_a_strategy`, mirroring python): "either call
//! again or write your final answer to finish", "ORIENT FIRST", "NEVER claim you
//! don't have access". Reusing that wording because it is "tuned" would quietly
//! void every emergence result taken afterwards.

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
/// States facts and a few short working rules: how to address an agent, what
/// `reflect` returns, where `reply_to` sends the answer, how to recognise a reply,
/// and when work counts as done. It never names a loop; stopping is `done` or
/// `await`.
pub const SEND_TO_HOWTO: &str = r#"## Reaching other agents
Everything is an agent with a string id; `send` reaches any of them. A call is a
`<call>` element, one child tag per argument — no JSON, no quotes, no escaping:
<call name="send">
<routing><target_id>AGENT</target_id></routing>
<payload><type>VERB</type><path>PATH</path><bytes>FIRST LINE
SECOND LINE, with "quotes"
</bytes></payload>
</call>
`<routing>` is how the call travels: `<target_id>` names the agent. `<payload>` is
delivered whole: `<type>` the verb, every other child an argument. A value is text as
written (`true`, `12`, a path); a list is `<item>` children; an object is child tags
(`<edits><item><path>PATH</path><hunks>N</hunks></item></edits>`).
A body (a file, a patch) goes in `<bytes>`…`</bytes>` VERBATIM — real newlines and quotes,
nothing escaped, no fence; it ends at the first `</bytes>`. `<reply_to>` in `<routing>` picks where the reply
goes: you by default (your next message), another id hands it to them, `null` discards it.
`reflect` goes to the agent itself and says what it IS; how to CALL it is `docs`.

## Grants — every call is DENY-ALL
You may call only what your `can_call` (in your self block) grants: `{key: "*" | [verbs]}`,
a key being an agent id, `$self`, `$parent`, `$subtree` (every agent below you) or `*`; the
target must also admit you (its `callable_by`). Plan inside `can_call` — any other call is
refused and costs a turn; never retry a refusal. `docs` is a grant too: on the root (`kernel`).

## Turns
Only your ANSWER acts: a `<call>` in your thinking does not run; write it in your answer,
after thinking; every call in it runs. End every turn with a call, with
`<call name="await"></call>` (stop until the next message wakes you; what you wait for is in
flight, not lost, and a job from `exec start` wakes you with `job_done`; refused when nothing
is pending) or with `<call name="done"></call>` (the work is done, checked as under Working). A turn with
no call is rerolled.

## Replies
Every call runs under a pid; its reply comes back tagged `[reply to <pid> · <agent>
<verb>]`. The calls of one turn run CONCURRENTLY (no order — if B needs A, send B after
A's reply) and are answered TOGETHER in one later message, one block each (`N of M in
batch …`). An unseen reply is in flight — never re-issue it; send `kernel` `<type>ps</type>`
to check. A message to an
agent has a deadline (600 s unless set otherwise); past it you are told of a dead letter —
resend it, or drop it with `kernel` `<type>ack_deadletter</type>`.

## How to call an agent — `docs`
`docs` is a KERNEL verb: send it to `kernel` with the agent you ask about in `<of>`
(an agent id, a bundle by its listed name, or
`kernel` for create_agent, update_agent, ps, …): `<payload><type>docs</type><of>AGENT</of></payload>`.
It answers one signature per verb; add `<verb>write</verb> <tier>detailed</tier>` for one
verb's full text. A docs reply is TRANSIENT: after your next message it becomes a
one-line stub — act on it then, or ask again. Reflect only agents you are granted; for part
of a tree, `reflect` takes `<show_subtrees>` or `<ignore_subtrees>`, each a list of `<item>` ids.

## Inquiries
Look it up, or take the obvious reading and say your assumption. Send an `inquiry` only when
no tool can answer and the choices lead to different work: `send` whoever owns it an
`inquiry` (`<text>`, `<kind>text|single|multi|confirm</kind>`, `<options>`). The answer
arrives LATER as a message under that call's pid: work on, or `<call name="await"></call>`
for it. Asked one yourself
(`[inquiry from …]`), do the work it needs, then answer with
<call name="inquiry_answer"><answer>what you concluded</answer></call>
— nothing else counts as your answer.
"#;

/// The titles how-to, appended when the record sets `context_titles`. A title
/// rides on the reply's tag and is what REMAINS once the body is stripped.
pub const TITLES_HOWTO: &str = r#"## Titling your calls
Every call carries a one-sentence `<title>` in its `<routing>`:
<call name="send">
<routing><target_id>AGENT</target_id><title>WHAT THIS CALL IS FOR</title></routing>
<payload><type>VERB</type><path>PATH</path></payload>
</call>
The title rides on the reply's tag and is what remains once the reply BODY is stripped
from your view (bodies stay fetchable by pid). Title the INTENT (what you read and what you look for in it);
what a reply told you goes in your task state — the body will go; the title and your state stay.
"#;

/// The FULL-ASYNC how-to, gated on the record's `context_async` — taught only
/// when replies really do arrive as rows. Python `_ASYNC_HOWTO`, verbatim.
pub const ASYNC_HOWTO: &str = r#"## Replies are rows, not turns
A reply is DATA, not a message to answer. A LARGE reply arrives as a one-line digest —
tag, your title, size, and its ORIGIN (how to get it again):
[reply to <pid> · <agent> <verb> · "<title>"]
(settled, <N> bytes — NOT inlined, held in memory only until this agent restarts. origin: <agent> <path> lines <a-b> <version> — read {path, range}. To find a line in it: send yourself `<type>grep</type> <query>substring</query> <ids><item>pid</item></ids>`)
GREP FIRST: `<type>grep</type> <query>substring</query>` on yourself finds the line —
in a body held this run, a folded stretch, an old turn, a row shared with you — with
its id and line. BODIES ARE NOT KEPT: after a restart only the origin line remains, so
re-read or re-run what you still need; never answer from a body you cannot see.
FOLDS: when your window fills, finished stretches collapse into `[fold <id> · N refs]
"<title>"` plus a short summary. Nothing is deleted — `<type>history</type>
<fold>id</fold>` lists a fold's members, grep with the fold's id searches inside it.
After a fold, and on your first turn after a boot, `context_recall` asks you what to keep
in full; a `[recall …]` line is your own earlier decision and names the LIVE task.
`<type>context_clear</type>` on yourself starts a NEW THREAD at a `[cleared …]` line; every
earlier row stays reachable through grep, recall and `history fold`.
"#;

/// The memory how-to — python `_MEMORY_HOWTO`, verbatim, `{store}` filled in with
/// the record's `memory_store_id` (or `(none)`).
pub const MEMORY_HOWTO: &str = r#"Your conversation lives in your MEMORY STORE `{store}` — every row under YOUR key.
"#;

/// The working rules, placed LAST in the default system block: the model follows the
/// most recent instructions most closely, and these must survive a long task brief.
pub const WORKING_HOWTO: &str = r#"## Working
Your thinking lasts one turn; your reply's own text stays. Keep the task's state there, in a
few plain lines before your calls (plain text, not a call or an agent): what was asked and how
you will know it is done; the steps and which one you are on; what you found, with where you
found it; what you tried that failed; and your next step. Restate them only when something
changed. A step counts as done only once its result is among what you found; a finding a
later result contradicts is corrected. Each turn, start from your latest state and think only
about the next step. Other text outside calls: only your last message.
Read only what the request names; no docs or reflect sweep before your first real call. Make
the first call that changes or tests something as soon as you can — an error reply names the
missing argument. The tree above lists what exists and `can_call` what you may use: do not
reflect to rediscover them; `docs` only for a verb you are about to call.
One answer may hold many <call> elements; they run at once. Put in it every call that does not
need another's reply; a call waits for a later turn only when it does.
Find before you read: grep or list, then read only the lines you need. Bound command output
(`head`, `tail`, `grep`, a count); put long logs in a file and search it. Never guess a path
or symbol you have not seen.
When an edit or command fails, read the error and the current file, then change the call;
never resend it unchanged. A step that fails or brings nothing new three times: stop, put it
in your state as failed, then take another route or send whoever asked you an `inquiry` with what you tried.
Before `done`: for each check that shows the task is done, one line with its evidence (test or
build output, a command result). Checks are run, not re-read: the test for your change first, the full suite
once. Do not re-read a file only to confirm an edit the tool acknowledged. Say what you could
not check.
Your last message: what changed, where (path:line), what you checked, what is left; under ten lines.
"#;

/// The context-window how-to, appended to the DEFAULT system block. Context is
/// the backend's (it owns the conversation), so it teaches the recall /
/// context_status affordances alongside [`SEND_TO_HOWTO`]'s addressing.
pub const CONTEXT_HOWTO: &str = r#"## Context window
Your view is finite: when it overflows, older turns are compacted and a
`[context-notice]` says so; the transcript stays whole. Send your OWN id
`<type>recall</type>` (`<query>`, `<limit>`) to page dropped turns back, `<type>context_status</type>`
for your budget.
A [folded …], [thought …] or [cut …] anchor stands for a body you already saw in full; `recall` with its id to read it again — it folds back after your next answer. A folded `reflect` or `docs` reply has no id: call it again for the current state.
Your thinking lasts one turn: after you answer it folds to a [thought …] anchor and is not kept. Use it to choose the next step, not to gather again what you already know. Do not repeat a call whose result you still have or noted; fetch again only what was folded and you need.
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

/// One piece of the system block, in the order the block concatenates them:
/// `part` names what it is (`system_text`, `prompt_file`, `reflect_tree`,
/// `self_reflect`, `verb_docs`), or is empty for the blank line between two pieces.
/// The block IS the pieces' texts end to end, so the request carries the same bytes
/// however they are told apart.
pub(crate) struct SystemPiece {
    pub(crate) part: &'static str,
    pub(crate) text: String,
}

const SEPARATOR: &str = "\n\n";

fn piece(part: &'static str, text: impl Into<String>) -> SystemPiece {
    SystemPiece {
        part,
        text: text.into(),
    }
}

/// `text` with every placeholder replaced in ONE pass, so a filled part is never
/// scanned again (python `_PLACEHOLDER.sub`), told apart piece by piece: the text
/// between placeholders is `literal`, and each placeholder's value is the part it
/// names (`{store}` is the id, plain text).
fn fill(
    text: &str,
    parts: &HashMap<&'static str, String>,
    literal: &'static str,
) -> Vec<SystemPiece> {
    let mut out: Vec<SystemPiece> = Vec::new();
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
                        "tree" => Some("reflect_tree"),
                        "self" => Some("self_reflect"),
                        "howto" => Some("verb_docs"),
                        _ => None,
                    };
                    match named {
                        Some(name) => {
                            out.push(piece(literal, std::mem::take(&mut run)));
                            out.push(piece(name, v.as_str()));
                        }
                        None => run.push_str(v),
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
/// python `_parts`.
async fn parts(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    need: &[&str],
) -> HashMap<&'static str, String> {
    let mut out: HashMap<&'static str, String> = HashMap::new();
    if need.contains(&"tree") {
        // Lean primer: a flat id-index of the tree + the bundle catalog by name.
        let primer = kernel.tree_ids_primer().await;
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
        // LAST: the working rules are what the model reads right before the task.
        blocks.push(WORKING_HOWTO.to_string());
        out.insert("howto", blocks.join("\n\n"));
    }
    out
}

/// A prompt with its placeholders filled for this agent and this turn — python
/// `_render_prompt`.
async fn render_prompt(
    text: &str,
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    literal: &'static str,
) -> Vec<SystemPiece> {
    let need = named(text);
    if need.is_empty() {
        return vec![piece(literal, text)];
    }
    fill(text, &parts(self_id, kernel, &need).await, literal)
}

/// The DEFAULT system block: `{tree}\n\n{self}\n\n{howto}` — the lean primer,
/// this agent's own reflect, and the how-to sections its record earns. Extracted
/// so `context_recall` can MEASURE the same block the next turn will carry —
/// python's `_default_system_content`.
pub async fn default_system_content(self_id: &AgentId, kernel: &Arc<Kernel>) -> String {
    default_pieces(self_id, kernel)
        .await
        .into_iter()
        .map(|p| p.text)
        .collect()
}

/// The default block as its pieces: the primer, this agent's own reflect and the
/// how-to sections, a blank line between.
async fn default_pieces(self_id: &AgentId, kernel: &Arc<Kernel>) -> Vec<SystemPiece> {
    let mut p = parts(self_id, kernel, &["tree", "self", "howto"]).await;
    vec![
        piece("reflect_tree", p.remove("tree").unwrap_or_default()),
        piece("", SEPARATOR),
        piece("self_reflect", p.remove("self").unwrap_or_default()),
        piece("", SEPARATOR),
        piece("verb_docs", p.remove("howto").unwrap_or_default()),
    ]
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
    Ok(system_pieces(self_id, kernel, system_prompt)
        .await?
        .into_iter()
        .map(|p| p.text)
        .collect())
}

/// [`system_content`] told apart piece by piece: the block is their texts end to end.
pub(crate) async fn system_pieces(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    system_prompt: Option<&str>,
) -> Result<Vec<SystemPiece>, PromptFileError> {
    // THE WRAP: the record's `pre_system_prompt` goes BEFORE
    // whichever prompt is in force and `post_system_prompt` AFTER it, each read
    // live and taken verbatim — no placeholders, and neither replaces anything.
    // Absent or null adds nothing (python `_system_content`).
    let meta = crate::helpers::agent_meta(self_id, kernel);
    let inner = inner_system_pieces(self_id, kernel, system_prompt).await?;
    let wrap = |key: &str| {
        crate::schema::record_wrap_prompt(&meta, key).map(|s| vec![piece("system_text", s)])
    };
    let mut out: Vec<SystemPiece> = Vec::new();
    for group in [
        wrap("pre_system_prompt"),
        Some(inner),
        wrap("post_system_prompt"),
    ]
    .into_iter()
    .flatten()
    {
        if group.iter().all(|p| py_strip(&p.text).is_empty()) {
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
    system_prompt: Option<&str>,
) -> Result<Vec<SystemPiece>, PromptFileError> {
    if let Some(s) = system_prompt.filter(|s| !s.trim().is_empty()) {
        return Ok(render_prompt(s, self_id, kernel, "system_text").await);
    }
    if let Some(s) =
        crate::schema::record_system_prompt(&crate::helpers::agent_meta(self_id, kernel))
    {
        return Ok(render_prompt(&s, self_id, kernel, "system_text").await);
    }
    // Set and unusable is an `Err` — the turn fails, never the default below.
    if let Some(s) = crate::prompt_file::text(self_id, kernel).await? {
        return Ok(render_prompt(&s, self_id, kernel, "prompt_file").await);
    }
    Ok(default_pieces(self_id, kernel).await)
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
    let pieces = system_pieces(self_id, kernel, system_prompt).await?;
    let system_content: String = pieces.iter().map(|p| p.text.as_str()).collect();
    let mut messages = vec![json!({"role": "system", "content": system_content})];
    // WHAT THE REQUEST CARRIES, piece by piece — kept for the row the turn writes;
    // never rendered into the request.
    let mut parts: Vec<Value> = pieces
        .iter()
        .filter(|p| !p.part.is_empty() && !p.text.is_empty())
        .map(|p| part_entry(p.part, None, None, &p.text))
        .collect();
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
    let rendered = render_history(&store, listed.as_deref(), caching.cleans());
    let pics = Pictures::of(self_id, state, kernel, &store, &rendered);
    for r in rendered {
        let (msg, form) = if caching.folds() && r.res == "full" {
            crate::history_view::autofold(&store, r.idx, &r.msg, Some(user_text))
        } else {
            (r.msg, if r.res == "full" { "whole" } else { "anchor" })
        };
        // a body a trimmer cut down is no longer the row whole
        let shown = msg.get("content").and_then(Value::as_str).unwrap_or("");
        let stored = store[r.idx].get("content").and_then(Value::as_str);
        let form = if form == "whole" && stored != Some(shown) {
            "anchor"
        } else {
            form
        };
        // THE IMAGES OF THE ROW: whole, a line naming them, or alt text (#622)
        let (msg, form) = match crate::images::parts_of(&msg) {
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
        let shown = msg.get("content").and_then(Value::as_str).unwrap_or("");
        parts.push(part_entry(
            "row",
            msg.get("id").and_then(Value::as_i64),
            Some(form),
            shown,
        ));
        messages.push(msg);
    }
    *state.request_parts.lock().expect("request_parts poisoned") = parts;
    let mut incoming = json!({"role": "user", "content": user_text});
    // the images this turn attached: shown whole to a model that can see them, else alt text
    if let Some(a) = crate::agent_loop::attached_in_flight(state) {
        incoming["images"] = Value::Array(a.parts.clone());
        let how = pics.form_of_new(&a.parts);
        incoming = crate::images::render(incoming, &a.info, None, how);
    }
    messages.push(incoming);
    Ok((messages, store_len))
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
        // the turn's own images count against the bound of the model first
        let current = crate::agent_loop::attached_in_flight(state).map_or(0, |a| a.parts.len());
        let mut out = Self {
            sees,
            limits,
            whole: Default::default(),
            recalled,
        };
        if !sees {
            return out;
        }
        // THE MODEL ANSWERED: the rows before its newest answer are anchors from now on
        let answered = store.iter().rposition(|r| r["role"] == "assistant");
        let mut room = limits.max_images.map(|n| (n - current as i64).max(0));
        for r in rendered.iter().rev() {
            let row = &store[r.idx];
            let Some(parts) = crate::images::parts_of(row) else {
                continue;
            };
            let wanted = answered.is_none_or(|a| r.idx > a)
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

    /// The form the images of the turn's own send take.
    fn form_of_new(&self, parts: &[Value]) -> crate::images::Form {
        if self.sees && parts.iter().all(|p| crate::images::fits(p, self.limits)) {
            crate::images::Form::Image
        } else {
            crate::images::Form::Alt
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

/// The parts of the request that is SENT: the assembly's own, with each history
/// message checked against the final list — the projection can fold, strip or drop
/// rows after the assembly, and a row it changed is a reduced one (`anchor`). The
/// last message is the new input.
pub(crate) fn request_parts(assembled: &[Value], sent: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = assembled
        .iter()
        .filter(|p| p["part"] != json!("row"))
        .cloned()
        .collect();
    let last = sent.len().saturating_sub(1);
    for (i, m) in sent.iter().enumerate().skip(1) {
        let text = m.get("content").and_then(Value::as_str).unwrap_or("");
        let id = m.get("id").and_then(Value::as_i64);
        if i == last {
            out.push(part_entry(
                "new_input",
                None,
                m.get("image_form").and_then(Value::as_str),
                text,
            ));
            continue;
        }
        let now = part_entry("row", id, Some("anchor"), text);
        let same = assembled.iter().find(|p| {
            p["part"] == json!("row") && p.get("ref") == now.get("ref") && p["hash"] == now["hash"]
        });
        out.push(same.cloned().unwrap_or(now));
    }
    out
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

#[cfg(test)]
mod tests_worknote {
    #[test]
    fn the_context_howto_teaches_one_turn_thinking_and_anchors() {
        let h = super::CONTEXT_HOWTO;
        assert!(h.contains("Your thinking lasts one turn"));
        assert!(h.contains("Do not repeat a call whose result you still have or noted"));
    }

    #[test]
    fn the_working_section_asks_to_keep_the_task_state_in_the_reply() {
        let h = super::WORKING_HOWTO.split_whitespace().collect::<Vec<_>>().join(" ");
        let h = h.as_str();
        assert!(h.starts_with("## Working Your thinking lasts one turn; your reply's own text stays."));
        assert!(h.contains("Keep the task's state there, in a few plain lines before your calls (plain text, not a call or an agent)"));
        assert!(h.contains("what was asked and how you will know it is done; the steps and which one you are on; what you found, with where you found it; what you tried that failed; and your next step."));
        assert!(h.contains("A step counts as done only once its result is among what you found"));
        assert!(h.contains("think only about the next step"));
        // the sections run in the agreed order
        let s = super::SEND_TO_HOWTO;
        assert!(!s.contains("## Working"));
        let order = ["## Reaching", "## Grants", "## Turns", "## Replies", "## How to call", "## Inquiries"];
        let at: Vec<usize> = order.iter().map(|k| s.find(k).unwrap()).collect();
        assert!(at.windows(2).all(|p| p[0] < p[1]), "{at:?}");
    }

    #[test]
    fn the_send_howto_says_act_early_batch_and_verify_before_done() {
        // (the text wraps lines; compare on single spaces)
        let h = format!("{} {}", super::SEND_TO_HOWTO, super::WORKING_HOWTO).split_whitespace().collect::<Vec<_>>().join(" ");
        let h = h.as_str();
        assert!(h.contains("Read only what the request names; no docs or reflect sweep before your first real call."));
        assert!(h.contains("an error reply names the missing argument"));
        assert!(h.contains("One answer may hold many <call> elements; they run at once."));
        assert!(h.contains("a call waits for a later turn only when it does"));
        assert!(h.contains("Before `done`: for each check that shows the task is done, one line with its evidence"));
        assert!(h.contains("the test for your change first, the full suite once"));
        assert!(h.contains("`done` (the work is done, checked as under Working)") || h.contains("checked as under Working"));
        assert!(h.contains("three times: stop"));
        assert!(h.contains("send whoever asked you an `inquiry`"));
        assert!(h.contains("Other text outside calls: only your last message."));
    }
}
