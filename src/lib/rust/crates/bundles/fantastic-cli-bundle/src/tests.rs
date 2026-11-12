//! The render sink's contract. Output goes to stdout, so what is pinned here is
//! the DISPATCH — which verbs are declared, what its own entry refuses — and
//! the RENDERED LINE, through the pure renderers the handler prints.

use super::*;
use fantastic_kernel::Item;
use serde_json::json;

/// A kernel with one `cli` agent, answering through its own entry.
async fn rig() -> Arc<Kernel> {
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, CliBundle);
    let kernel = Arc::new(kernel);
    let root = fantastic_kernel::Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        std::path::PathBuf::new(),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(root);
    let cli = fantastic_kernel::Agent::new(
        AgentId::from("cli"),
        Some(HANDLER_MODULE.to_string()),
        Some(AgentId::from("kernel_state")),
        json!({"callable_by": {"*": "*"}}).as_object().cloned().unwrap(),
        std::path::PathBuf::new(),
        false,
    );
    kernel.register(cli);
    kernel
}

async fn dispatch(payload: Value) -> Value {
    let kernel = rig().await;
    kernel
        .send(vec![Item::new(AgentId::from("cli"), payload)])
        .await
        .remove(0)
        .result
}

fn status(payload: Value) -> Option<String> {
    render_status(&serde_json::from_value(payload).expect("a status call"))
}

fn context(payload: Value) -> String {
    render_context(&serde_json::from_value(payload).expect("a context call"))
}

#[tokio::test]
async fn reflect_says_what_the_bundle_is_for() {
    let reply = dispatch(json!({"type": "reflect"})).await;
    assert_eq!(reply["sentence"], json!(MAN), "{reply}");
}

#[test]
fn the_verbs_are_declared_in_order_each_with_what_it_does() {
    let names: Vec<&str> = declared::VERBS.iter().map(|v| v.name).collect();
    assert_eq!(
        names,
        ["token", "done", "say", "error", "status", "context", "intro_booting", "booted"]
    );
    for v in declared::VERBS {
        assert!(!v.doc().is_empty(), "{} says what it does", v.name);
    }
    let token = declared::VERBS.iter().find(|v| v.name == "token").unwrap();
    assert_eq!(token.schema()["required"], json!(["text"]));
}

#[tokio::test]
async fn an_unknown_verb_is_refused_by_the_agents_own_entry() {
    let reply = dispatch(json!({"type": "nonsense_verb"})).await;
    assert_eq!(reply["reason"], json!("unhandled"), "{reply}");
    assert_eq!(reply["verb"], json!("nonsense_verb"));
}

#[tokio::test]
async fn a_token_that_is_not_text_is_refused_by_name() {
    for text in [json!(5), json!(null), json!(1.5), json!(true), json!([1]), json!({"a": 1})] {
        let reply = dispatch(json!({"type": "token", "text": text})).await;
        assert_eq!(reply["arg"], json!("text"), "{text}: {reply}");
    }
    let reply = dispatch(json!({"type": "token"})).await;
    assert_eq!(reply["reason"], json!("missing_arg"), "{reply}");
}

#[tokio::test]
async fn every_declared_verb_answers_null_rather_than_an_error() {
    for call in [
        json!({"type": "token", "text": "x"}),
        json!({"type": "done"}),
        json!({"type": "say", "text": "x"}),
        json!({"type": "error", "text": "x"}),
        json!({"type": "status", "phase": "thinking"}),
        json!({"type": "context", "phase": "compacted"}),
        json!({"type": "intro_booting"}),
        json!({"type": "booted"}),
    ] {
        let reply = dispatch(call.clone()).await;
        assert_eq!(reply, Value::Null, "{call}");
    }
}

/// The intro is the python map, and the identity names THIS runtime.
#[tokio::test]
async fn intro_booting_prints_the_control_plane_map() {
    let k = Arc::new(Kernel::new());
    let lines = intro_booting_lines(&k);
    assert_eq!(lines.len(), 2);
    // No root yet in a bare kernel — python falls back to its own root id, so
    // rust falls back to rust's.
    assert_eq!(
        lines[0],
        format!(
            "[fantastic] rust · env=host · root=kernel_state · pid {} — booting…",
            std::process::id()
        )
    );
    assert_eq!(lines[1], "  full map: reflect · verbs: docs of=<id>");
}

/// `booted` is a DUMB SINK: one fixed line, no tree inspection.
#[test]
fn booted_is_a_dumb_sink_no_tree_inspection() {
    assert_eq!(
        BOOTED_LINE,
        "[kernel] up — all booted. attach via the map above, or reflect"
    );
}

/// The tool-call entry line renders `json.dumps(args or {})`, not
/// serde's compact spelling: python separates with `, ` and `: `, and escapes
/// non-ASCII.
#[test]
fn status_handler_renders_phase_markers() {
    let line = status(json!({
        "phase": "tool_calling",
        "source": "ai",
        "detail": {"tool": {"verb": "read", "target": "notes", "args": {"path": "é", "n": 2}}},
    }))
    .unwrap();
    assert_eq!(
        line,
        "  [ai] → read(notes)  {\"path\": \"\\u00e9\", \"n\": 2}"
    );
    // No `args` is an empty object.
    assert_eq!(
        status(json!({"phase": "tool_calling", "detail": {"tool": {"verb": "v", "target": "t", "args": null}}}))
            .unwrap(),
        "  → v(t)  {}"
    );
    // The exit line is chosen by KEY PRESENCE, so a null preview still exits.
    assert_eq!(
        status(json!({"phase": "tool_calling", "detail": {"tool": {"verb": "v", "target": "t", "reply_preview": null}}}))
            .unwrap(),
        "  ← v(t)  None"
    );
    // queued / thinking.
    assert_eq!(
        status(json!({"phase": "queued", "detail": {"ahead": 3}})).unwrap(),
        "  queued (3 ahead)"
    );
    assert_eq!(
        status(json!({"phase": "queued", "source": "ai"})).unwrap(),
        "  [ai] queued (0 ahead)"
    );
    assert_eq!(status(json!({"phase": "thinking"})).unwrap(), "  thinking…");
    // `streaming` and `done` are silent.
    assert!(status(json!({"phase": "streaming"})).is_none());
    assert!(status(json!({"phase": "done"})).is_none());
}

/// Detail values are rendered as text (`str()`), every default is keyed on
/// PRESENCE, and an empty source takes the plain prefix.
#[test]
fn status_values_render_as_text() {
    assert_eq!(
        status(json!({"phase": "queued", "detail": {"ahead": null}})).unwrap(),
        "  queued (None ahead)"
    );
    assert_eq!(
        status(json!({"phase": "queued", "source": "ai", "detail": {"ahead": [1, "a"]}})).unwrap(),
        "  [ai] queued ([1, 'a'] ahead)"
    );
    assert_eq!(
        status(json!({"phase": "thinking", "source": null})).unwrap(),
        "  thinking…"
    );
    assert_eq!(
        status(json!({"phase": "thinking", "source": ""})).unwrap(),
        "  thinking…"
    );
    assert_eq!(
        status(json!({"phase": "tool_calling", "detail": {"tool": {"verb": 1, "target": {"a": 1}, "reply_preview": true}}}))
            .unwrap(),
        "  ← 1({'a': 1})  True"
    );
    assert_eq!(
        context(json!({"phase": "compacted", "detail": {}})),
        "  ⟲ context compacted (?, 0 dropped; recall to page back)"
    );
    assert_eq!(
        context(json!({"phase": "compacted", "source": "ai", "detail": {"strategy": null, "dropped_messages": 2}})),
        "  [ai] ⟲ context compacted (None, 2 dropped; recall to page back)"
    );
    assert_eq!(
        context(json!({"phase": "too_small", "detail": {"hint": 5}})),
        "  ✗ context too small — 5"
    );
    assert_eq!(
        context(json!({"phase": "too_small"})),
        "  ✗ context too small — "
    );
}

/// A phase the declaration does not list is refused by the entry, by name.
#[tokio::test]
async fn an_unknown_phase_is_refused_by_name() {
    for call in [
        json!({"type": "status", "phase": "nonsense"}),
        json!({"type": "context", "phase": "nonsense"}),
        json!({"type": "context", "phase": "thinking"}),
    ] {
        let reply = dispatch(call.clone()).await;
        assert_eq!(reply["arg"], json!("phase"), "{call}: {reply}");
    }
}

/// A `detail` that is not an object is refused by the entry, by name.
#[tokio::test]
async fn a_detail_that_is_not_an_object_is_refused_by_name() {
    for detail in [json!("x"), json!(5), json!(true), json!([1])] {
        let reply = dispatch(json!({"type": "status", "phase": "queued", "detail": detail})).await;
        assert_eq!(reply["arg"], json!("detail"), "{reply}");
    }
}

/// Every line this sink prints loses its C0/C1 control characters
/// (and DEL), newline and tab kept: an escape sequence in agent or model text was
/// INTERPRETED by the operator's terminal (a title, a clipboard write, a rewritten
/// line). The same strings as python's `test_agent_text_reaches_the_terminal_…`.
#[test]
fn terminal_safe_strips_control_characters_but_newline_and_tab() {
    // An OSC window title (BEL-terminated), an erase-line CSI, a C1 CSI, a
    // carriage return, DEL and NUL — plus the two kept: newline and tab.
    let hostile = "a\x1b]0;TEST-VALUE-TITLE\x07b\x1b[2Kc\u{9b}31md\re\x7ff\x00g\th\ni";
    assert_eq!(
        terminal_safe(hostile),
        "a]0;TEST-VALUE-TITLEb[2Kc31mdefg\th\ni"
    );
    // printable text, non-ASCII included, passes untouched
    let plain = "  [src] ⟲ context compacted (π, 3 dropped) — ok";
    assert_eq!(terminal_safe(plain), plain);
}
