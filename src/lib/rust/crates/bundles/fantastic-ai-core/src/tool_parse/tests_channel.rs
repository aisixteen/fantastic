//! The `<|tool_call>` special-token channel grammar: its own string token, both
//! ordinary quote styles, escapes, bare scalars, nested structure, and the
//! THE-ARGS-DECIDE ladder. Mirrors the channel half of
//! `bundled_agents/ai/ai_core/tests/test_tool_parse.py`.
//!
//! Nearly every case here is VERBATIM text from a run that died on it. The body is
//! not JSON, so none of this is reachable by a JSON parser — that was exactly the
//! 2026-08-04 failure.

use super::{extract_tool_calls, malformed_calls};
use serde_json::json;

/// The one call in `text`. Every test here asserts on a single call, and the
/// `.pop().expect(…)` dance obscures which case failed.
fn one(text: &str) -> super::ToolCall {
    let mut calls = extract_tool_calls(text);
    assert_eq!(calls.len(), 1, "exactly one call in: {text}");
    calls.pop().expect("the call")
}

#[test]
fn the_channel_envelope_from_the_failed_run_parses_verbatim() {
    // VERBATIM from the 2026-08-04 run that died at pass 1: gemma-4-12B-it planned
    // correctly, picked the right agent and verb, and wrapped it in its OWN
    // template's shape. `<|tool_call>` is not `<tool_call>` — the pipe breaks the
    // match — so zero calls were extracted and the chain ended with the model never
    // getting a reply.
    let c = one(
        "Let's start by listing the agents.<|tool_call>call:kernel_state.list_agents{}<tool_call|>",
    );
    assert_eq!(c.name, "send");
    assert_eq!(
        c.arguments_value(),
        json!({"target_id": "kernel_state", "payload": {"type": "list_agents"}})
    );
}

#[test]
fn channel_strings_use_their_own_token_and_may_contain_commas() {
    // The documented shape (ai.google.dev). Strings wear `<|"|>`, NOT `"` — and a
    // comma inside one must not split the argument list, which is exactly what a
    // `split(",")` or a JSON parse would get wrong.
    let c = one(r#"<|tool_call>call:weather.get{location:<|"|>Tokyo, JP<|"|>}<tool_call|>"#);
    assert_eq!(
        c.arguments_value(),
        json!({"target_id": "weather", "payload": {"type": "get", "location": "Tokyo, JP"}})
    );
}

#[test]
fn channel_args_land_in_the_payload_beside_type() {
    // `call:<target>.<verb>{…}` IS the universal envelope written differently: the
    // braces are payload fields alongside `type`, not a second argument slot.
    let c = one(
        r#"<|tool_call>call:assets.write{path:<|"|>a.txt<|"|>,content:<|"|>x<|"|>}<tool_call|>"#,
    );
    assert_eq!(
        c.arguments_value(),
        json!({
            "target_id": "assets",
            "payload": {"type": "write", "path": "a.txt", "content": "x"},
        })
    );
}

#[test]
fn channel_bare_scalars_and_nested_objects() {
    // Numbers, booleans and nested objects are written BARE in this grammar. Also
    // covers the uncompressed shape — the model naming our taught `send` tool
    // directly.
    let c = one(concat!(
        r#"<|tool_call>call:send{target_id:<|"|>m<|"|>,"#,
        r#"payload:{type:<|"|>write<|"|>,count:3,ratio:0.5,ok:true,note:null}}<tool_call|>"#
    ));
    assert_eq!(c.name, "send");
    assert_eq!(
        c.arguments_value(),
        json!({
            "target_id": "m",
            "payload": {
                "type": "write",
                "count": 3,
                "ratio": 0.5,
                "ok": true,
                "note": null,
            },
        })
    );
}

#[test]
fn a_bare_list_is_a_value_too() {
    // The grammar writes arrays bare as well, nested to any depth. No python test
    // covers this because no run has emitted one yet — the reader exists, so it is
    // pinned rather than left to be discovered by a failed run.
    let c = one(r#"<|tool_call>call:a.b{ids: [1, 'two', {k: 3}], empty: []}<tool_call|>"#);
    assert_eq!(
        c.arguments["payload"],
        json!({"type": "b", "ids": [1, "two", {"k": 3}], "empty": []})
    );
}

#[test]
fn channel_envelope_with_json_quoted_args() {
    // VERBATIM from the 88-pass run of 2026-08-04. The envelope is the channel one,
    // the arguments are JSON — models mix the two. Reading the value as a bare token
    // kept its quote characters, so `id` never arrived and all THREE
    // posture-changing calls came back `{"error": "update_agent: id required"}`.
    let c = one(concat!(
        "<|tool_call>call:kernel_state.update_agent",
        r#"{"id":"assets","ingress_rule":"allow_all"}<tool_call|>"#
    ));
    assert_eq!(
        c.arguments_value(),
        json!({
            "target_id": "kernel_state",
            "payload": {
                "type": "update_agent",
                "id": "assets",
                "ingress_rule": "allow_all",
            },
        })
    );
}

#[test]
fn channel_args_carrying_target_id_are_a_plain_send() {
    // VERBATIM from the same run. `target_id` in the args means the model wrote
    // canonical `send` arguments and the dotted prefix is a namespace it invented,
    // not a target. Reading the prefix as the target produced
    // `{"error": "kernel_state: unknown type 'send'"}` eight times.
    let c = one(concat!(
        "<|tool_call>call:kernel_state.send",
        r#"{"payload":{"type":"reflect"},"target_id":"assets"}<tool_call|>"#
    ));
    assert_eq!(c.name, "send");
    assert_eq!(
        c.arguments_value(),
        json!({"target_id": "assets", "payload": {"type": "reflect"}})
    );
}

#[test]
fn channel_target_id_without_payload_keeps_the_verb() {
    // VERBATIM from the passing run of 2026-08-04.
    // `call:X.reflect{"target_id":"Y"}` means "reflect Y". An earlier revision saw
    // `target_id` and emitted a send with NO payload; python's router dispatched a
    // null payload and the handler raised `'NoneType' object has no attribute 'get'`
    // three times in one run.
    let c = one(r#"<|tool_call>call:kernel_state.reflect{"target_id":"transcripts"}<tool_call|>"#);
    assert_eq!(
        c.arguments_value(),
        json!({"target_id": "transcripts", "payload": {"type": "reflect"}})
    );
}

#[test]
fn a_send_with_neither_payload_nor_verb_is_not_a_call() {
    assert!(
        extract_tool_calls(r#"<|tool_call>call:send{"target_id":"x"}<tool_call|>"#).is_empty(),
        "nowhere to send it and nothing to say: no call"
    );
}

#[test]
fn channel_malformed_is_dropped_not_repaired() {
    // A parser that guesses what the model meant is measuring its own guess.
    for text in [
        "<|tool_call>call:a.b{unterminated<tool_call|>",
        r#"<|tool_call>call:a.b{k:<|"|>no close}<tool_call|>"#,
        "<|tool_call>garbage<tool_call|>",
    ] {
        assert!(extract_tool_calls(text).is_empty(), "{text}");
    }
}

// ─── both quote styles ──────────────────────────────────────────

/// Verbatim from gemma-4-12B, probe 05 re-run, 2026-08-06 — a completely correct
/// intent that the parser threw away because the strings were single-quoted. It was
/// the model's last act; it had nothing left to fix and the run stalled.
const GEMMA_SINGLE_QUOTED: &str = concat!(
    "<|tool_call>call:kernel_state{type: 'send', target_id: 'kernel', ",
    "payload: {type: 'reflect', readme: 'index'}, reply_to: 'self'}<tool_call|>"
);

#[test]
fn single_quoted_channel_args_parse() {
    let c = one(GEMMA_SINGLE_QUOTED);
    assert_eq!(c.arguments["target_id"], "kernel");
    assert_eq!(
        c.arguments["payload"],
        json!({"type": "reflect", "readme": "index"})
    );
    assert_eq!(c.arguments["reply_to"], "self");
    assert!(malformed_calls(GEMMA_SINGLE_QUOTED).is_empty());
}

#[test]
fn both_quote_styles_yield_the_same_call() {
    // Which quote a model reaches for is a fact about its training, not about our
    // grammar, so it must not change the result.
    let double = GEMMA_SINGLE_QUOTED.replace('\'', "\"");
    assert_eq!(
        one(&double).arguments_value(),
        one(GEMMA_SINGLE_QUOTED).arguments_value()
    );
}

#[test]
fn a_mismatched_quote_pair_is_an_error_not_a_guess() {
    // Opened with one style and closed with the other: obviously a mistake, so it
    // must not parse — and must be REPORTED rather than dropped.
    let text = "<|tool_call>call:a.b{k: 'value\"}<tool_call|>";
    assert!(extract_tool_calls(text).is_empty());
    assert_eq!(malformed_calls(text).len(), 1);
}

#[test]
fn quoted_channel_strings_may_contain_structural_characters() {
    // The bare-token reader stops at `,}]`; a quoted string must not.
    // (`call:a.b` is DOTTED, so `b` becomes the payload's verb — hence `type`.)
    let c = one("<|tool_call>call:a.b{k: 'a,b}c[d', j: 2}<tool_call|>");
    assert_eq!(
        c.arguments["payload"],
        json!({"type": "b", "k": "a,b}c[d", "j": 2})
    );
}

#[test]
fn the_other_quote_style_survives_inside_a_string() {
    let c = one("<|tool_call>call:a.b{k: 'it\"s', m: \"it's\"}<tool_call|>");
    assert_eq!(c.arguments["payload"]["k"], "it\"s");
    assert_eq!(c.arguments["payload"]["m"], "it's");
}

#[test]
fn escapes_decode_the_same_under_either_quote() {
    // `\n` is a newline and `\\` a single backslash under both styles; each style
    // also escapes its OWN quote.
    for (body, want) in [
        (r"{k: 'a\nb\\c\'d'}", "a\nb\\c'd"),
        (r#"{k: "a\nb\\c\"d"}"#, "a\nb\\c\"d"),
    ] {
        let c = one(&format!("<|tool_call>call:a.b{body}<tool_call|>"));
        assert_eq!(c.arguments["payload"]["k"], want, "{body}");
    }
}

#[test]
fn a_unicode_escape_decodes_under_either_quote() {
    // `\uXXXX` rides the same shared table. A malformed one is NOT a repair site: it
    // falls through to the unknown-escape rule below and survives as text.
    for body in [r"{k: '\u00e9'}", r#"{k: "\u00e9"}"#] {
        assert_eq!(
            one(&format!("<|tool_call>call:a.b{body}<tool_call|>")).arguments["payload"]["k"],
            "é",
            "{body}"
        );
    }
    assert_eq!(
        one(r"<|tool_call>call:a.b{k: '\uZZZZ'}<tool_call|>").arguments["payload"]["k"],
        r"\uZZZZ",
        "a broken escape is text, not a guess"
    );
}

#[test]
fn an_unknown_escape_survives_verbatim() {
    // A Windows path in a program body must not be silently mangled.
    let c = one(r"<|tool_call>call:a.b{k: 'C:\dir'}<tool_call|>");
    assert_eq!(c.arguments["payload"]["k"], r"C:\dir");
}

#[test]
fn a_quoted_key_in_either_style_loses_its_quotes() {
    // `"id"` as a literal key name is what killed three posture changes on
    // 2026-08-04; single quotes must not reintroduce it.
    for body in ["{'id': 'assets'}", r#"{"id": "assets"}"#] {
        let c = one(&format!("<|tool_call>call:a.update{body}<tool_call|>"));
        assert_eq!(c.arguments["payload"]["id"], "assets", "{body}");
        assert!(
            c.arguments["payload"].get(r#""id""#).is_none(),
            "the quotes must not ride along as part of the name: {body}"
        );
    }
}

// ─── THE ARGS DECIDE: the inline-payload ladder ─────────────────

#[test]
fn an_inline_payload_with_target_id_is_a_send() {
    // Measured 2026-08-06: `call:kernel_state{type:'reflect', target_id:'readings'}`
    // means "reflect readings". The verb was previously looked for ONLY in a dotted
    // name, so this complete instruction was discarded.
    let c =
        one("<|tool_call>call:kernel_state{type: 'reflect', target_id: 'readings'}<tool_call|>");
    assert_eq!(
        c.arguments_value(),
        json!({"target_id": "readings", "payload": {"type": "reflect"}})
    );
}

#[test]
fn an_undotted_name_with_an_inline_type_targets_the_name() {
    // `call:kernel_state{type:'reflect'}` is `send('kernel_state', {…})`. It used to
    // arrive with no `target_id` and earn an error for a clear request.
    let c = one("<|tool_call>call:kernel_state{type:'reflect'}<tool_call|>");
    assert_eq!(c.name, "send");
    assert_eq!(
        c.arguments_value(),
        json!({"target_id": "kernel_state", "payload": {"type": "reflect"}})
    );
}

#[test]
fn reply_to_stays_routing_and_never_enters_the_payload() {
    let c = one("<|tool_call>call:totals{type: 'set', key: 'k', reply_to: 'self'}<tool_call|>");
    assert_eq!(c.arguments["reply_to"], "self");
    assert_eq!(c.arguments["payload"], json!({"type": "set", "key": "k"}));
}

#[test]
fn an_explicit_payload_still_wins_over_an_inline_type() {
    // The 2026-08-04 shape must not regress: args carrying both `target_id` and
    // `payload` pass through verbatim, stray top-level `type` and all.
    let c = one(concat!(
        r#"<|tool_call>call:kernel_state.send{"payload":{"type":"reflect"},"#,
        r#""target_id":"kernel_state"}<tool_call|>"#
    ));
    assert_eq!(c.arguments["target_id"], "kernel_state");
    assert_eq!(c.arguments["payload"], json!({"type": "reflect"}));
}

#[test]
fn an_undotted_name_with_no_verb_anywhere_keeps_the_name() {
    // The ladder's last rung: no `target_id`, no dot, no `type`. There is nothing to
    // translate, so the call is handed on as the model wrote it and earns the
    // router's `no target_id` error — which teaches. We do not invent a target.
    let c = one("<|tool_call>call:list_agents{scope: 'all'}<tool_call|>");
    assert_eq!(c.name, "list_agents");
    assert_eq!(c.arguments_value(), json!({"scope": "all"}));
}
