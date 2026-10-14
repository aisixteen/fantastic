// Prompt assembly — the shared system-prompt builder behind every Swift LLM
// backend. Mirrors Rust's `fantastic-ai-core::assembly` and the Python
// `ai_core._assemble`: the model's DEFAULT system block is a lean primer
// (id-index of the tree + bundle catalog), the agent's own self-reflect, the
// how-to for REACHING another agent, and the context-window how-to.
//
// NO agent menu (the id-index plus `reflect` is how an agent is found), and — the
// line that matters — NO STRATEGY. Both how-tos state MECHANISMS: what the
// envelope looks like, where a reply goes, how to page dropped turns back. They
// never say when to call, how many times, or when to stop. The removed
// `_SEND_HOWTO` (recoverable at 2ce23d1) did exactly that — "either call again or
// write your final answer to finish", "ORIENT FIRST" — which is the loop
// dictated, and reusing that wording because it is "tuned" would quietly void
// every emergence result taken afterwards. Pinned by
// `promptStatesTheMechanismNotAStrategy`, banned-word list included.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

/// The fields of an agent's own reflect its system block shows — python `_SELF_KEYS`.
let SELF_KEYS = ["id", "description", "memory_store_id", "can_call", "callable_by"]

/// Render a reflect reply into a one-line `sentence  k=v  k=v` blurb.
///
/// NO JSON on the model path (python `_render_reflect`): a string is itself, a
/// list of scalars is comma-joined through `str(x)`, and anything nested is the
/// REPLY notation collapsed onto the line — `null` therefore prints as nothing,
/// `{"$self": "*"}` as `<$self>*</$self>`, and a `["core", …]` tree as
/// `core,web`. `serialize()` printed JSON here, which is the one notation this
/// path does not speak.
func renderReflect(_ v: JSON) -> String {
    guard var obj = v.asObject else { return "" }
    let sentence = obj["sentence"]?.asString ?? ""
    obj["sentence"] = nil
    var parts: [String] = []
    for (k, val) in obj {
        parts.append("\(k)=\(renderReflectValue(val))")
    }
    let fields = parts.joined(separator: "  ")
    return pyStrip("\(sentence)  \(fields)")
}

/// One reflect field as the line says it — python `_render_reflect._one`.
private func renderReflectValue(_ v: JSON) -> String {
    if case .string(let s) = v { return s }
    if case .array(let items) = v,
        items.allSatisfy({
            if case .object = $0 { return false }
            if case .array = $0 { return false }
            return true
        })
    {
        return items.map { pyStr($0) }.joined(separator: ",")
    }
    return pySplitWhitespace(renderReply(v)).joined(separator: " ")
}

/// How to reach another agent, and where that send's answer goes — the block
/// that makes the dispatch path usable. Byte-for-byte the Python
/// `_SEND_TO_HOWTO` and Rust `SEND_TO_HOWTO`.
///
/// It lands with ROUTING, never before it: a prompt that teaches an envelope
/// nothing dispatches is strictly worse than a prompt that stays quiet, because
/// the model then writes calls, receives no reply, and invents the replies it
/// never got.
///
/// MECHANISM, never strategy: the envelope's shape, that `reply_to` defaults to
/// the sender (so an omitted key is a chain, not a discard), how a document
/// rides as `<bytes>`, how `done`/`await` end a turn, and how replies arrive.
/// When to call and when to stop are the model's to decide.
let SEND_TO_HOWTO = #"""
    ## Reaching other agents
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

    """#

/// The context-window how-to, appended to the DEFAULT system block. Context is
/// the backend's (it owns the conversation), so it teaches the recall /
/// context_status affordances — the same footing as `SEND_TO_HOWTO`, which
/// teaches the send envelope the backend also owns. Byte-for-byte the Python
/// `_CONTEXT_HOWTO` (and Rust `CONTEXT_HOWTO`).
let CONTEXT_HOWTO = #"""
    ## Context window
    Your view is finite: when it overflows, older turns are compacted and a
    `[context-notice]` says so; the durable transcript stays whole. Send your OWN id
    `<type>recall</type>` (`<query>`, `<limit>`) to page dropped turns back, and
    `<type>context_status</type>` for your budget. Facts that must outlive this window
    (names, decisions, preferences) belong in a memory agent.

    """#

/// Titling, taught ONLY when `context_titles` is on the record: a title is what
/// REMAINS once a reply body is stripped from the view, so an agent whose
/// strategy never strips one gains nothing from the block and pays its tokens.
let TITLES_HOWTO = #"""
    ## Titling your calls
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

    """#

/// Replies-as-rows, taught ONLY when `context_async` is on the record. It
/// describes a digest, its origin line, `grep`, `history {fold}` and folds —
/// affordances an agent whose replies arrive inline does not have, and a prompt
/// must never name a verb the agent cannot use. Python `_ASYNC_HOWTO`, verbatim.
let ASYNC_HOWTO = #"""
    ## Replies are rows, not turns
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

    """#

/// The memory how-to, in EVERY default system block: where the conversation
/// lives. Python `_MEMORY_HOWTO` (a format
/// string — `{store}` is the agent's `memory_store_id`, `{{…}}` a literal brace).
func memoryHowto(store: String) -> String {
    #"""
    ## Your memory
    Your conversation lives in your MEMORY STORE `{store}` — every row under YOUR key.

    """#.replacingOccurrences(of: "{store}", with: store)
}
