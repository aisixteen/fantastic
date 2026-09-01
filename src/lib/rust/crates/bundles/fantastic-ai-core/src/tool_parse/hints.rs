//! What a malformed-envelope report SAYS: the per-grammar advice appended to the
//! error, and [`defang`], which keeps the report from re-arming the scanner.
//!
//! Ported from python `tool_parse.py:509-562`.

use super::channel::{CHANNEL_CLOSE, CHANNEL_OPEN};
use super::invoke_envelope::{INVOKE_CLOSE, INVOKE_OPEN};
use super::json_envelope::{CLOSE, OPEN};

/// The advice appended to a malformed JSON-bodied envelope's error. Carrying a
/// program inside a JSON string is THE failure mode measured on gemma-4-12B
/// (2026-08-06). Note what it actually got wrong, because it is the reverse of the
/// obvious guess: the body was escaped CORRECTLY throughout, and the closing quote
/// of the `code` value was simply never written —
///
/// ```text
/// …"code":"def update_totals():\n … return f\"…to {worker_res}\"}}
///              ^ opens here                                    ^ still open
/// ```
///
/// so every following brace was read as string content and the object ended
/// unterminated. A model that escapes diligently can still lose the whole call by
/// escaping the ONE quote that was supposed to close the value. Naming the rule is
/// cheaper than letting it rediscover it, and this is advice, not a repair: we do
/// not re-quote the body and retry.
pub const JSON_HINT: &str = concat!(
    "Your program broke its own JSON envelope. Do NOT debug the escaping — ",
    "switch to the unbreakable form: re-send the SAME call as `send_binary` ",
    "with the program in `bytes` written RAW, as the LAST field — real ",
    r#"newlines, real quotes, nothing escaped; everything from `"bytes":"` "#,
    r#"to the final `"` before the closing braces is taken verbatim."#
);

/// The `<|tool_call>` body is NOT JSON, so [`JSON_HINT`] would be actively
/// misleading there — it was, on 2026-08-06, when a single-quoted channel call was
/// answered with a lecture about escaping JSON. Advice has to match the grammar the
/// model actually used, or it teaches the wrong lesson.
pub const CHANNEL_HINT: &str = concat!(
    "The `<|tool_call>` body is NOT JSON: write it as ",
    "`call:<agent>{key: value, …}`. Strings may use ' or \" as long as the ",
    "SAME character closes them, and every { [ ( must be closed too."
);

/// Every envelope delimiter, in python's order (`tool_parse.py:560`).
const TAGS: [&str; 6] = [
    OPEN,
    CLOSE,
    CHANNEL_OPEN,
    CHANNEL_CLOSE,
    INVOKE_OPEN,
    INVOKE_CLOSE,
];

/// Neutralize envelope delimiters so quoted text cannot re-arm the scanner.
///
/// Anything we echo back becomes conversation the model reads, quotes and reasons
/// about — and a quoted `<tool_call>` is indistinguishable from an emitted one.
/// Measured 2026-08-06: echoing a block verbatim made this error self-amplifying,
/// each report seeding the next. Breaking the opener is enough; the text stays
/// perfectly legible, which is the point of showing it at all.
pub fn defang(text: &str) -> String {
    let mut out = text.to_string();
    for tag in TAGS {
        out = out.replace(tag, &tag.replace('<', "‹").replace('>', "›"));
    }
    out
}
