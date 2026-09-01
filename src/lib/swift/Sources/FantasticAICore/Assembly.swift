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
func renderReflect(_ v: JSON) -> String {
    guard var obj = v.asObject else { return "" }
    let sentence = obj["sentence"]?.asString ?? ""
    obj["sentence"] = nil
    var parts: [String] = []
    for (k, val) in obj {
        let rendered = val.asString ?? val.serialize()
        parts.append("\(k)=\(rendered)")
    }
    let fields = parts.joined(separator: "  ")
    return "\(sentence)  \(fields)".trimmingCharacters(in: .whitespaces)
}

/// How to reach another agent, and where that send's answer goes — the block
/// that makes the dispatch path usable. Byte-for-byte the Python
/// `_SEND_TO_HOWTO` (core.py:438-448) and Rust `SEND_TO_HOWTO`.
///
/// It lands with ROUTING, never before it: a prompt that teaches an envelope
/// nothing dispatches is strictly worse than a prompt that stays quiet, because
/// the model then writes calls, receives no reply, and — measured repeatedly on
/// gemma-4-12B — invents the replies it never got.
///
/// Three facts, no strategy: the envelope's shape, that `reply_to` defaults to
/// the sender (so an omitted key is a chain, not a discard), and that a turn
/// tagged `[reply to <id> <verb>]` is an answer rather than a human's message. When
/// to call and when to stop are the model's to decide.
let SEND_TO_HOWTO = """
## Reaching other agents
Everything is an agent with a string id. `send(target_id, payload)` reaches any of them —
the payload's `type` names the verb; `{type:'reflect'}` returns that agent's verbs and
state. To send, emit exactly this, alone on its line:
<tool_call>{"name":"send","arguments":{"target_id":"<id>","payload":{"type":"<verb>"},"reply_to":"self"}}</tool_call>
`reply_to` is where that send's reply goes. It defaults to you — leave it out and the
answer comes back as your next message. Name another agent's id to hand the answer to
them instead, or `null` to discard it. Text outside the tag is your message to whoever
asked. Interpret instructions literally — act on what is asked, not on inferred
intent. Keep prose brief: the work happens in calls, not in explanation.

## Writing file contents — `send_binary`
A stream sink like `write_stream` wants raw BYTES, but a `send` payload is JSON and
JSON has no bytes type. So when a payload field IS a file's contents — source you are
writing — use `send_binary`. It is `send`, one self-contained call, with ONE special
rule: the `bytes` field is read RAW and must be the LAST field. Put the program there
LITERALLY — do NOT escape anything, no \\n, no \\", write real newlines and real
quotes. Everything from `"bytes":"` to the final `"` before the closing braces is
taken verbatim as the file's bytes:
<tool_call>{"name":"send_binary","arguments":{"target_id":"<store id>","payload":{"type":"write_stream","path":"main.py","truncate":true,"bytes":"def main():
    print("hi")   # real newlines, real quotes — nothing escaped
"}}}</tool_call>
`bytes` LAST, written raw — that is the whole trick, and it is why a large program
never breaks the call. The metadata around it (`target_id`, `path`, `truncate`) is
ordinary JSON. Everything else is `send` — `reply_to`, `id`, `timeout`, `retries` —
but put those BEFORE `payload`: nothing may come after the program but closing braces.

A `<tool_call>` IS AN INVOCATION, WHEREVER YOU WRITE IT — including inside your own
thinking. The envelope is parsed out of everything you emit and dispatched the
moment it is complete, so there is no hypothetical or illustrative call: writing one
makes it happen.

Your thinking is NOT carried over between turns — it is stripped from history, so a
conclusion you only reasoned about is lost. Put what you worked out into the call you
make, or into a message. Reasoning longer never carries further than acting now does.
Before ending a turn, check one thing: every action you decided on exists as a
`<tool_call>` envelope in the turn. A decision without its envelope did not happen.

Stopping is EXPLICIT and costs one line: when the work is done, say so and emit
<tool_call>{"name":"done"}</tool_call>
— that ends your chain. A turn with NO call ends nothing: the runtime reminds you
once (a call decided in thinking does not execute unless the envelope is written
out), so never leave a finished task unsignalled — call `done`.

NEEDING A HUMAN IS ALSO NORMAL. When you cannot proceed without an answer, ask and
stop: put the question in your message — STRICT JSON on its own line, so a UI
can parse and render it:
`{"type":"question", "request_id":"<yours>", "kind":"text|single|multi|confirm",
"question":"...", "options":[...]}` — then emit no call. The chain
ends; the ANSWER arrives as your next turn (name your request_id to correlate),
with your full history intact — continue exactly where you stopped. Nothing you
completed before asking will re-run.

HOW REPLIES ARRIVE. Every send is dispatched under a PID (`<agent>:<token>`) and
every reply comes back tagged `[reply to <pid> · <agent> <verb>]`. Read that
literally:

- ALL the calls you make in ONE turn are answered TOGETHER in your next turn —
  one block per call, each tagged `· N of M in batch <token>` when you made
  several. M is complete on arrival: there is nothing still coming from that
  batch, so never wait, poll, or re-ask for a "missing" reply — if a call
  failed or timed out, its block says so.
- The calls of one turn run CONCURRENTLY. Their side effects have NO defined
  order — if B must happen after A, put B in the next turn, after A's reply.
- Each call may carry `timeout: <seconds>` in its arguments to bound that call;
  otherwise a generous default applies. A call that exceeds it is CANCELLED and
  its block reports the timeout.
- A call may carry `id: "<name>"` in its arguments — BESIDE `payload`, never
  inside it: `{"name":"send","arguments":{"target_id":"x","payload":{...},
  "id":"job-1"}}`. That NAMES the execution.
  Re-sending the SAME id NEVER runs the work twice: you get the first run's
  recorded outcome back, byte for byte (even if it failed). Use it for any call
  you must not double-fire (a write, a payment, a create) and to safely re-fetch
  a reply; a genuinely NEW attempt needs a NEW id. Ids may not contain ':'.
  Add `retries: N` and the KERNEL re-runs a failing or timing-out call up to N
  more times before answering — you send once and receive the final outcome
  (`attempts` says how many runs it took). Work that stays failed is parked:
  `send 'kernel' {type:'ledger'}` lists your executions and what became of
  them, outcomes included — inspect there instead of re-firing blind.
- `reply_to:null` means the call still runs but its answer is discarded — it is
  not in M and nothing comes back. `reply_to:<someone else>` hands that block to
  THEM instead of you.
- A reply whose PID you do not recognise is NORMAL — another agent can route its
  result to you. Read it, use it if useful, and move on.

A `reflect` reply is LARGE and TRANSIENT — you see its full text once, in the turn it
arrives, and the reply itself carries the instruction for what to record before it
goes. Follow it there. If you later need a detail you did not keep, reflect that agent
again — it is cheap and always current, and re-reading beats guessing.
"""

/// The context-window how-to, appended to the DEFAULT system block. Context is
/// the backend's (it owns the conversation), so it teaches the recall /
/// context_status affordances — the same footing as `SEND_TO_HOWTO`, which
/// teaches the send envelope the backend also owns. Byte-for-byte the Python
/// `_CONTEXT_HOWTO` (and Rust `CONTEXT_HOWTO`).
let CONTEXT_HOWTO = """
    ## Context window & compaction
    Your live context is finite. When the conversation outgrows it, the system COMPACTS your
    view: older turns are summarized or elided and a `[context-notice]` turn is inserted. The
    DURABLE transcript is ALWAYS whole — compaction only shapes what you see right now, and
    nothing is ever truly lost.
    - To page dropped turns back on demand: `send('<your_own_id>', {type:'recall', query?, limit?})`.
      `query` substring-filters your full history; `limit` caps the page. Use it whenever you
      need a detail from earlier that isn't in your current view.
    - When a compaction notice lands and the dropped span held durable facts (names, decisions,
      preferences), persist them to a memory agent (reflect the tree to find one) via the send
      tool — that is how they survive beyond this window.
    - To inspect your budget + the last compaction + your own last reaction:
      `send('<your_own_id>', {type:'context_status'})`.
    """
