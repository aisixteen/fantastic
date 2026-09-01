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

/// A model that serializes the WHOLE call as strict JSON (escaped `\n` in
/// `bytes`) must get real newlines — strict parse wins over the raw-tail path.
/// Measured 2026-08-31 (python E44): the raw path kept the escapes literal and
/// a valid patch died on "no blocks".
#[test]
fn escaped_json_send_binary_decodes_its_escapes() {
    let inner = serde_json::json!({
        "name": "send_binary",
        "arguments": {
            "target_id": "editor",
            "payload": {
                "type": "patch",
                "path": "config.txt",
                "checkpoint": true,
                "bytes": "<<<<<<< SEARCH\nport = 8080\n=======\nport = 9090\n>>>>>>> REPLACE\n",
            },
        },
    })
    .to_string();
    let calls = super::extract_tool_calls(&format!("<tool_call>{inner}</tool_call>"));
    assert_eq!(calls.len(), 1);
    let b = calls[0].arguments["payload"]["bytes"].as_str().unwrap();
    assert!(b.contains('\n') && !b.contains("\\n"), "{b:?}");
    assert!(b.starts_with("<<<<<<< SEARCH\nport = 8080"));
}

// ─── the FENCED form: the program rides OUTSIDE the JSON ────────────────────
// Ported from python `test_send_binary.py` (fence tests, 2026-09-01).

/// The gym's killer case: quote-heavy rust with apostrophes and braces —
/// verbatim through the fence, no escaping, no skeleton to balance.
#[test]
fn fenced_program_rides_verbatim() {
    let prog = "use std::collections::HashMap;\n\nfn f() { let c = '\\''; }\n";
    let text = format!(
        "<tool_call>{{\"name\":\"send_binary\",\"arguments\":{{\"target_id\":\"editor\",\
         \"payload\":{{\"type\":\"write\",\"path\":\"src/lib.rs\",\"truncate\":true,\
         \"bytes\":\"@body\"}}}}}}\n*** Begin Bytes\n{prog}*** End Bytes\n</tool_call>"
    );
    let calls = extract_tool_calls(&text);
    assert_eq!(calls.len(), 1);
    let p = &calls[0].arguments["payload"];
    assert_eq!(p["bytes"], json!(prog));
    assert_eq!(p["path"], json!("src/lib.rs"));
}

/// No placeholder written — the program still has exactly one home.
#[test]
fn fenced_without_placeholder_lands_in_payload_bytes() {
    let text = "<tool_call>{\"name\":\"send_binary\",\"arguments\":{\"target_id\":\"editor\",\
                \"payload\":{\"type\":\"write\",\"path\":\"m.py\"}}}\n\
                *** Begin Bytes\nx = 1\n*** End Bytes\n</tool_call>";
    let calls = extract_tool_calls(text);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].arguments["payload"]["bytes"], json!("x = 1\n"));
}

/// A missing close marker takes the rest of the block — the envelope's own
/// closing tag already bounds it (same stance as the unterminated envelope).
#[test]
fn fenced_missing_close_marker_takes_rest_of_block() {
    let text = "<tool_call>{\"name\":\"send_binary\",\"arguments\":{\"target_id\":\"editor\",\
                \"payload\":{\"type\":\"write\",\"path\":\"m.py\",\"bytes\":\"@body\"}}}\n\
                *** Begin Bytes\na = 1\nb = 2\n</tool_call>";
    let calls = extract_tool_calls(text);
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].arguments["payload"]["bytes"],
        json!("a = 1\nb = 2\n")
    );
}

/// Broken metadata above the fence earns the fence-grammar error, never the
/// strict-JSON red herring about the program's newlines.
#[test]
fn fenced_bad_metadata_gets_a_fence_error() {
    use crate::tool_parse::malformed_calls;
    // one closing brace SHORT — the measured live failure class
    let text = "<tool_call>{\"name\":\"send_binary\",\"arguments\":{\"target_id\":\"editor\",\
                \"payload\":{\"type\":\"write\",\"path\":\"m.py\",\"bytes\":\"@body\"}}\n\
                *** Begin Bytes\nx = 1\n*** End Bytes\n</tool_call>";
    assert!(extract_tool_calls(text).is_empty());
    let errs = malformed_calls(text);
    assert_eq!(errs.len(), 1);
    let error = &errs[0].error;
    assert!(
        error.contains("Begin Bytes"),
        "advice names the fence grammar: {error}"
    );
    assert!(!error.contains("control character"));
}
