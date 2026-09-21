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
/// the model then writes calls, receives no reply, and — measured repeatedly on
/// gemma-4-12B — invents the replies it never got.
///
/// MECHANISM, never strategy: the envelope's shape, that `reply_to` defaults to
/// the sender (so an omitted key is a chain, not a discard), how a document
/// rides as `<bytes>`, how `done`/`await` end a turn, and how replies arrive.
/// When to call and when to stop are the model's to decide.
let SEND_TO_HOWTO = #"""
    ## Reaching other agents
    Everything is an agent with a string id. `send(target_id, payload)` reaches any of them —
    `<type>` names the verb; `reflect` returns that agent's verbs and
    state. A call is a `<call>` element: one child tag per argument, nothing else — no
    JSON, no quotes, no escaping. `<target_id>` names the agent, `<type>` the verb, and
    every other child is that verb's argument:
    <call name="send">
    <target_id>notes</target_id>
    <reply_to>self</reply_to>
    <type>reflect</type>
    </call>
    `<reply_to>` is where that send's reply goes. It defaults to you — leave it out and the
    answer comes back as your next message. Name another agent's id to hand the answer to
    them instead, or `null` to discard it. A value is text as written (`true`, `12`, a
    path); a LIST is one tag whose children are all `<item>`, one element each:
    `<ids><item>12</item><item>15</item></ids>`; an object is a tag with child tags,
    and a list of objects is `<item>`s with children:
    `<edits><item><path>a.py</path><hunks>2</hunks></item></edits>`. Text outside the call
    is your message to whoever asked. Interpret instructions literally — act on what is
    asked, not on inferred intent. Keep prose brief: the work happens in calls.
    EMIT CALLS BEFORE YOU DELIBERATE. A plan written in thought and never sent is a wasted
    turn: the moment you know the next call, send it — reasoning continues after the reply,
    with the reply in hand. Never end a turn on "let me do X" without X in a `<call>`.

    ## Sending a document — `<bytes>` is VERBATIM
    When a verb takes a body of text (a file to write, a patch, a program), it names the
    argument `bytes`. Write the text between `<bytes>` and `</bytes>` exactly as it should
    land — real newlines, real quotes, nothing escaped, no fence — and close the call after
    it. The document ends at the first `</bytes>`, so a document must not contain that tag.
    <call name="send">
    <target_id>notes</target_id>
    <type>write</type>
    <path>notes.md</path>
    <bytes>line one
    line two, with "quotes" and anything else
    </bytes>
    </call>
    Which verbs take a body is each agent's own story — reflect it; every one names `bytes`.

    ## Talking to several agents at once
    A send may name several recipients instead of one:
    <call name="send">
    <to><item>solver_2</item><item>solver_3</item></to>
    <type>send</type>
    <text>…</text>
    </call>
    Every recipient gets its own copy, and every copy opens with the same header:
    [from solver_1 · to solver_2, solver_3 · msg 412]
    `msg` is the id of the turn that sent it — the same number in every copy, so a group
    can refer to one message; a message from any agent opens with `[from … · msg N]`.
    Reply to a group message with the SAME `<to>` list (everyone except yourself) and
    `<re>412</re>` naming it; a reply sent to fewer agents, or to someone who was not on
    the list, forks a private conversation the others will never see. What a peer must
    know goes in the text: cite your own facts in words, never an id from your own
    history — it means nothing to them.

    A `<call>` IS AN INVOCATION, WHEREVER YOU WRITE IT — including inside your own
    thinking. The envelope is parsed out of everything you emit and dispatched the
    moment it is complete, so there is no hypothetical or illustrative call: writing one
    makes it happen.

    Your thinking is NOT carried over between turns — it is stripped from history, so a
    conclusion you only reasoned about is lost. Put what you worked out into the call you
    make, or into a message. Reasoning longer never carries further than acting now does.
    Before ending a turn, check one thing: every action you decided on exists as a
    `<call>` in the turn. A decision without its call did not happen.

    Stopping is EXPLICIT and costs one line: when the work is done, say so and emit
    <call name="done"></call>
    — that ends your chain. WAITING is explicit too: when what you need is still in
    flight (a reply that has not come, a peer who has not answered), emit
    <call name="await"></call>
    — you stop, nothing reminds you, and the next message that lands wakes you. That is
    the honest alternative to re-issuing a call: this is an ASYNC system, and a reply you
    have not seen is on its way, not lost. `await` with nothing of yours in flight and
    nothing queued is refused — then act, or call `done`. A turn with NO call ends
    nothing: the runtime reminds you once (a call decided in thinking does not execute
    unless the envelope is written out), so never leave a turn unsignalled — call, `await`,
    or `done`.

    AN INQUIRY IS A CALL, and whoever receives it is AN AGENT LIKE ANY OTHER. When
    you cannot proceed without a decision that is not yours to make, `send` them
    an `inquiry` — `<type>inquiry</type> <text>…</text> <kind>text|single|multi|confirm</kind>
    <options><item>…</item></options>` — and the ANSWER COMES BACK AS THE REPLY, in your next turn,
    tagged with its pid like every other reply. Nothing suspends and nothing is
    scraped out of your prose: an inquiry is an ordinary call, so it is gated, it
    appears in the ledger, and someone who cannot answer REFUSES rather than
    inventing one. The same shape reaches a HUMAN (reflect the tree for the agent
    whose sentence says so) and a PEER AI — consulting a person and consulting a
    colleague are one verb, not two. Reflect before assuming anyone is reachable.

    RECEIVING ONE IS THE OTHER HALF. When someone `send`s YOU an `inquiry`, the turn
    you run IS your reply — say what your answer is:
    <call name="inquiry_answer"><answer>what you concluded</answer></call>
    That call is not a send and dispatches nothing; it hands your answer back to
    whoever sent it. A turn that never emits it did not answer, and they are told so —
    your prose is not harvested as an answer behind your back. Answer from what you
    know NOW: this one turn is the reply, so if you need to look something up
    first, say that instead of leaving them waiting.

    HOW REPLIES ARRIVE. Every send is dispatched under a PID (an integer message id) and
    every reply comes back tagged `[reply to <pid> · <agent> <verb>]`. Read that
    literally:

    - ALL the calls you make in ONE turn are answered TOGETHER, as ONE message — one
      block per call, each tagged `· N of M in batch <token>` when you made several.
      M is complete on arrival: nothing is still coming from that batch; if a call
      failed or timed out, its block says so.
    - That message arrives in a LATER turn, not necessarily the next: this is an
      ASYNC system, and messages from people and peers queued before it are handed
      to you first, one per turn. A reply you have not seen yet is IN FLIGHT, never
      lost — do not re-issue the call; if you must know, send `kernel` `<type>ps</type>`
      and look for your own task. Re-issuing only lengthens the queue that delays it.
    - The calls of one turn run CONCURRENTLY. Their side effects have NO defined
      order — if B must happen after A, put B in the next turn, after A's reply.
    - Each call may carry `timeout: <seconds>` in its arguments to bound that call;
      otherwise a generous default applies. A call that exceeds it is CANCELLED and
      its block reports the timeout.
    - A call may carry `<exec_id>job-1</exec_id>` beside its `<target_id>`. That NAMES
      the execution. Re-sending the SAME exec_id NEVER runs the work twice: you get the
      first run's recorded outcome back, byte for byte (even if it failed). Use it for
      any call you must not double-fire (a write, a payment, a create) and to safely
      re-fetch a reply; a genuinely NEW attempt needs a NEW exec_id. An exec_id you
      choose is a NAME (a word); the kernel's own ids are integers and cannot be
      created, only joined.
      Add `<retries>N</retries>` and the KERNEL re-runs a failing or timing-out call up
      to N more times before answering — you send once and receive the final outcome
      (`attempts` says how many runs it took). Work that stays failed is parked: send
      `kernel` `<type>ledger</type>` to list your executions and what became of them,
      outcomes included — inspect there instead of re-firing blind.
    - `reply_to:null` means the call still runs but its answer is discarded — it is
      not in M and nothing comes back. `reply_to:<someone else>` hands that block to
      THEM instead of you.
    - A reply whose PID you do not recognise is NORMAL — another agent can route its
      result to you. Read it, use it if useful, and move on.

    A `reflect` reply is LARGE and TRANSIENT — you see its full text once, in the turn it
    arrives, and the reply itself carries the instruction for what to record before it
    goes. Follow it there. If you later need a detail you did not keep, reflect that agent
    again — it is cheap and always current, and re-reading beats guessing.

    """#

/// The context-window how-to, appended to the DEFAULT system block. Context is
/// the backend's (it owns the conversation), so it teaches the recall /
/// context_status affordances — the same footing as `SEND_TO_HOWTO`, which
/// teaches the send envelope the backend also owns. Byte-for-byte the Python
/// `_CONTEXT_HOWTO` (and Rust `CONTEXT_HOWTO`).
let CONTEXT_HOWTO = #"""
    ## Context window & compaction
    Your live context is finite. When the conversation outgrows it, the system COMPACTS your
    view: older turns are summarized or elided and a `[context-notice]` turn is inserted. The
    DURABLE transcript is ALWAYS whole — compaction only shapes what you see right now, and
    nothing is ever truly lost.
    - To page dropped turns back on demand: send your OWN id `<type>recall</type>` with an
      optional `<query>` and `<limit>`.
      `query` substring-filters your full history; `limit` caps the page. Use it whenever you
      need a detail from earlier that isn't in your current view.
    - When a compaction notice lands and the dropped span held durable facts (names, decisions,
      preferences), persist them to a memory agent (reflect the tree to find one) via the send
      tool — that is how they survive beyond this window.
    - To inspect your budget + the last compaction + your own last reaction:
      send your OWN id `<type>context_status</type>`.

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
/// describes a digest, `grep`, `pull` and folds — affordances an agent whose
/// replies arrive inline does not have, and a prompt must never name a verb the
/// agent cannot use.
let ASYNC_HOWTO = #"""
    ## Replies are rows, not turns
    A reply is DATA, not a message to answer. A LARGE reply arrives as a one-line digest —
    tag, your title, size — with the body stored:
    [reply to <pid> · notes read · "survey filler_2"]
    (settled, 6812 bytes — NOT inlined. To find a line in it: send yourself `<type>grep</type> <query>substring</query> <ids><item>pid</item></ids>`. To read it: `<type>pull</type> <pid>pid</pid> <offset>0</offset>`)
    GREP BEFORE YOU PAGE: `<type>grep</type> <query>substring</query>` on yourself finds the line —
    in a stored body, a folded stretch, an old turn — with its id and line; pull only what
    matched. A `pull` page always fits inline; `next_offset` continues it, `eof` ends it.
    FOLDS: when your window fills, finished stretches collapse into `[fold <id> · N refs]
    "<title>"` plus a short summary. Nothing is deleted — grep or pull with the fold's id to
    look inside. After a fold, and on boot with a history, `context_recall` fits the head of
    your conversation to your budget and asks you what to keep in full; a `[recall …]` line
    is your own earlier decision and names the LIVE task. You may call
    `<type>context_recall</type>` on yourself any time.

    """#
