// Tool-call parsing — the text shapes a model may use to name a `send`.
//
// Ported from the canonical Python `ai_core/tool_parse.py`. The COMMENTS are
// part of that port: each one records a MEASURED failure with its date, and it
// is the only thing that stops the behaviour being "simplified" away later.
//
// CONSUMERS, two, and the load-bearing one is `Routing.routeReply`: it scans
// EVERY completion and dispatches each call it finds, delivering each reply to
// that call's own `reply_to`. That is the path a chain continues on, so a shape
// this module fails to recognize is a turn that silently does nothing — the model
// wrote an instruction, nothing ran, and no error came back. (The second consumer
// is `AIBackend.deriveReaction`, which scans the DURABLE transcript for whether
// the model reacted to a compaction notice.) Same set of envelopes for both, one
// implementation — python's arrangement exactly.
//
// There is NO native provider tool API and no `tools=[]`: the provider streams
// plain text and this module reads it. THREE envelopes are recognized, because a
// model's trained format beats our instruction and refusing a shape costs a
// whole run (each is documented at its own constants below):
//
//   <tool_call>{"name":"send","arguments":{…}}</tool_call>     taught (Hermes-style)
//   <invoke name="send"><parameter …>…</parameter></invoke>    Claude's own
//   <|tool_call>call:<name>{…}<tool_call|>                     special-token channel

import FantasticJSON
import Foundation

/// Opener of the TAUGHT envelope — the shape the system prompt teaches.
let TOOL_OPEN = "<tool_call>"
/// Closer of the taught envelope.
let TOOL_CLOSE = "</tool_call>"

// A SECOND text envelope, because a model's trained format beats our instruction.
// Claude models routinely emit their own shape instead of the taught one:
//
//   <invoke name="send">
//     <parameter name="arguments">{"target_id":"notes", …, "reply_to":"self"}</parameter>
//   </invoke>
//
// with the arguments perfectly correct — only the wrapper differs. Refusing it
// dropped whole runs at pass 1 (observed on claude-haiku-4.5), after which the
// model, never receiving a reply, invented the entire remaining session.
//
// This is STILL raw prompt-and-parse: it is plain text the model typed. There is
// no `tools=[]`, no `tool_choice`, no native `tool_use` block — the provider is
// untouched and knows nothing. Parsing more text shapes is exactly this layer's job.
let INVOKE_OPEN = "<invoke"
/// Closer of Claude's own envelope.
let INVOKE_CLOSE = "</invoke>"

// A THIRD text envelope, from the SPECIAL-TOKEN family. A `|` inside the brackets
// marks a token that lives in the tokenizer's VOCABULARY rather than in ordinary
// text, so the delimiter cannot be forged by the content it wraps — the same idea
// as ChatML's `<|im_start|>`, Llama-3's `<|eot_id|>` / `<|python_tag|>`, harmony's
// `<|channel|>`, and DeepSeek's tool-call markers. What distinguishes THIS grammar
// is that the pipe MIRRORS across the pair — `<|tool_call>` opens, `<tool_call|>`
// closes — and that strings carry a quoting token of their own, `<|"|>`, instead
// of `"`:
//
//   <|tool_call>call:get_current_weather{location:<|"|>Tokyo, JP<|"|>}<tool_call|>
//
// So the argument body is NOT JSON (which is why it earns its own hint below).
//
// Measured 2026-08-04 on gemma-4-12B-it: it planned correctly in its thought
// channel, chose the right agent AND verb, emitted
// `<|tool_call>call:kernel_state.list_agents{}<tool_call|>`, and the run died at
// pass 1 with zero calls extracted. `<tool_call>` is not a substring of either tag
// — the pipe breaks the match — so the taught envelope's finder never saw it at all.
let CHANNEL_OPEN = "<|tool_call>"
/// Closer of the special-token channel envelope — the pipe mirrors.
let CHANNEL_CLOSE = "<tool_call|>"

// The advice appended to a malformed-envelope error. Carrying a program inside a
// JSON string is THE failure mode measured on gemma-4-12B (2026-08-06). Note what
// it actually got wrong, because it is the reverse of the obvious guess: the body
// was escaped CORRECTLY throughout, and the closing quote of the `code` value was
// simply never written —
//
//   …"code":"def update_totals():\n … return f\"…to {worker_res}\"}}
//                ^ opens here                                    ^ still open
//
// so every following brace was read as string content and the object ended
// unterminated. A model that escapes diligently can still lose the whole call by
// escaping the ONE quote that was supposed to close the value. Naming the rule is
// cheaper than letting it rediscover it, and this is advice, not a repair: we do
// not re-quote the body and retry.
let JSON_HINT =
    "Your program broke its own JSON envelope. Do NOT debug the escaping — "
    + "switch to the unbreakable form: re-send the SAME call as `send_binary` "
    + "with the program in `bytes` written RAW, as the LAST field — real "
    + #"newlines, real quotes, nothing escaped; everything from `"bytes":"` "#
    + #"to the final `"` before the closing braces is taken verbatim."#

// The `<|tool_call>` body is NOT JSON, so the advice above would be actively
// misleading there — it was, on 2026-08-06, when a single-quoted channel call was
// answered with a lecture about escaping JSON. Advice has to match the grammar the
// model actually used, or it teaches the wrong lesson.
let CHANNEL_HINT =
    "The `<|tool_call>` body is NOT JSON: write it as "
    + #"`call:<agent>{key: value, …}`. Strings may use ' or " as long as the "#
    + "SAME character closes them, and every { [ ( must be closed too."

/// One recognized envelope: how it opens, how it closes, how its body parses,
/// and the advice a body that did NOT parse earns.
///
/// ONE row per envelope, so the scanner has no per-envelope branches to keep in
/// step and a fourth envelope is one entry. Python spreads the same facts over
/// two dicts keyed by the opener (`_ENVELOPES`, `_HINTS`); merging them is the
/// same table with one place to edit.
struct Envelope: Sendable {
    /// The opening delimiter.
    let opener: String
    /// The closing delimiter.
    let closer: String
    /// Body parser: `nil` means "this envelope opened and its body did not parse",
    /// which `malformedCalls` reports rather than swallows.
    let parse: @Sendable (String) -> ToolCall?
    /// Grammar-matched advice for a body that did not parse.
    let hint: String
    /// The delimiters as `[Character]`, because the scanner indexes over
    /// `Array(text)` so its arithmetic matches python's string indices exactly.
    let openerChars: [Character]
    /// See `openerChars`.
    let closerChars: [Character]

    init(
        opener: String, closer: String, parse: @escaping @Sendable (String) -> ToolCall?,
        hint: String
    ) {
        self.opener = opener
        self.closer = closer
        self.parse = parse
        self.hint = hint
        self.openerChars = Array(opener)
        self.closerChars = Array(closer)
    }
}

/// Every envelope the scanner knows, in the order that breaks a positional TIE:
/// channel before invoke before taught, preserving python's precedence.
let TOOL_ENVELOPES: [Envelope] = [
    Envelope(
        opener: CHANNEL_OPEN, closer: CHANNEL_CLOSE, parse: parseChannelCall, hint: CHANNEL_HINT),
    Envelope(opener: INVOKE_OPEN, closer: INVOKE_CLOSE, parse: parseInvoke, hint: JSON_HINT),
    Envelope(opener: TOOL_OPEN, closer: TOOL_CLOSE, parse: parseOne, hint: JSON_HINT),
]
