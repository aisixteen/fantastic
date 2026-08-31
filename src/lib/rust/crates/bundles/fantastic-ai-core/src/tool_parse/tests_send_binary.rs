//! `send_binary` — the `bytes` field carries a RAW program, read WITHOUT a strict
//! JSON parse. Ported from python `ai_core/tests/test_send_binary.py`.
//!
//! The failure this removes, measured live: a program in a JSON string field must
//! be perfectly escaped or the strict parse of the whole tool_call rejects it, and
//! a model cannot escape a big, quote-heavy program reliably. But the program is a
//! STRING WE READ, not data we parse — so for `send_binary` the `bytes` field is
//! lifted out RAW: everything from `"bytes":"` to the final `"` before the closing
//! braces is taken verbatim (no unescaping), and only the STRUCTURE around it is
//! JSON-parsed. `bytes` must be the LAST field.

use crate::tool_parse::extract_tool_calls;
use serde_json::json;

/// A `send_binary` tool_call with `program` written RAW as the ABSOLUTE last
/// field — `reply_to`/metadata first, then the payload ending in
/// `"bytes":"<program verbatim>"` with nothing escaped, so nothing follows the
/// program but closing braces. Mirrors python's `_binary_text`.
fn binary_text(target: &str, meta: serde_json::Value, program: &str) -> String {
    // The payload's metadata fields as JSON, minus the surrounding braces.
    let meta_str = serde_json::to_string(&meta).unwrap();
    let inner_fields = &meta_str[1..meta_str.len() - 1];
    let prefix = if inner_fields.is_empty() {
        String::new()
    } else {
        format!("{inner_fields},")
    };
    format!(
        "<tool_call>{{\"name\":\"send_binary\",\"arguments\":{{\"target_id\":{},\
         \"reply_to\":\"self\",\"payload\":{{{}\"bytes\":\"{}\"}}}}}}</tool_call>",
        serde_json::to_string(target).unwrap(),
        prefix,
        program
    )
}

#[test]
fn unescaped_program_parses_verbatim() {
    // real newlines + quotes + a backslash — nothing escaped
    let prog = "import os\nx = \"a \\ b\"\nprint(\"IMPORT_OK\", os.listdir(\".\"))\n";
    let text = binary_text(
        "store",
        json!({"type": "write_stream", "path": "main.py", "truncate": true}),
        prog,
    );
    let calls = extract_tool_calls(&text);
    assert_eq!(calls.len(), 1);
    let c = &calls[0];
    assert_eq!(c.name, "send_binary");
    let payload = &c.arguments["payload"];
    assert_eq!(
        payload["bytes"],
        json!(prog),
        "the program is taken verbatim — no unescaping"
    );
    assert_eq!(payload["path"], "main.py");
    assert_eq!(payload["truncate"], true);
}

#[test]
fn program_with_json_metacharacters_survives() {
    // braces, quotes, backslashes — the exact stuff that breaks a strict parse
    let prog = "d = {\"k\": \"v\"}\nprint(f\"{d}\")\npath = \"C:\\\\tmp\"\n";
    let text = binary_text(
        "store",
        json!({"type": "write_stream", "path": "x.py"}),
        prog,
    );
    let calls = extract_tool_calls(&text);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].arguments["payload"]["bytes"], json!(prog));
}

#[test]
fn bytes_is_the_last_field_boundary_is_the_final_quote() {
    // The program itself ends in a quote+newline; the boundary is the LAST `"` in
    // the envelope, so the whole program (its own trailing quote included) is
    // lifted, and the metadata before it stays structured.
    let prog = "print(\"done\")\n";
    let text = binary_text(
        "store",
        json!({"type": "write_stream", "path": "run.py", "truncate": true}),
        prog,
    );
    let calls = extract_tool_calls(&text);
    assert_eq!(calls.len(), 1);
    let p = &calls[0].arguments["payload"];
    assert_eq!(p["bytes"], json!(prog));
    assert_eq!(p["path"], "run.py");
    assert_eq!(p["type"], "write_stream");
}

#[test]
fn not_send_binary_is_untouched() {
    let plain = "<tool_call>{\"name\":\"send\",\"arguments\":\
         {\"target_id\":\"log\",\"payload\":{\"type\":\"note\",\"text\":\"hi\"}}}</tool_call>";
    let calls = extract_tool_calls(plain);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "send");
    assert_eq!(calls[0].arguments["payload"]["text"], "hi");
}
