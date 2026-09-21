//! The render sink's contract. Output goes to stdout, so what is pinned here is
//! the DISPATCH — which verbs are handled, which are dropped, which raise — and
//! the RENDERED LINE, through the pure renderers the handler prints.
//!
//! Every text is python's (`PB/cli/src/cli/tools.py`).

use super::*;
use serde_json::json;

fn kernel() -> Arc<Kernel> {
    Arc::new(Kernel::new())
}

async fn dispatch(payload: Value) -> Result<Reply, BundleError> {
    CliBundle
        .handle(&AgentId("cli".into()), &payload, &kernel())
        .await
}

fn status(payload: Value) -> Result<Option<String>, BundleError> {
    render_status(&payload)
}

fn context(payload: Value) -> Result<Option<String>, BundleError> {
    render_context(&payload)
}

#[tokio::test]
async fn reflect_advertises_exactly_what_it_accepts() {
    let reply = dispatch(json!({"type": "reflect"})).await.unwrap().unwrap();
    // A9.3 — python's `_reflect` builds the dict in this order.
    let keys: Vec<&str> = reply
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["id", "sentence", "verbs", "accepts"]);
    let accepts: Vec<String> = reply["accepts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    // The list and the dispatch must not drift: a verb advertised but not
    // rendered is a promise this sink does not keep.
    for verb in ACCEPTS {
        assert!(accepts.contains(&verb.to_string()), "missing {verb}");
        assert!(
            reply["verbs"].get(verb).is_some(),
            "{verb} accepted but undocumented"
        );
    }
}

/// A9.1 — the verb table is python's `VERBS`, in order, each doc the first line
/// of the python docstring. Two of them are cut mid-sentence on python; copying
/// the whole sentence would be a divergence, not a courtesy.
#[tokio::test]
async fn reflect_verbs_are_pythons_first_docstring_lines() {
    let reply = dispatch(json!({"type": "reflect"})).await.unwrap().unwrap();
    let verbs = reply["verbs"].as_object().unwrap();
    let names: Vec<&str> = verbs.keys().map(String::as_str).collect();
    assert_eq!(
        names,
        [
            "reflect",
            "token",
            "done",
            "say",
            "error",
            "status",
            "context",
            "intro_booting",
            "booted"
        ]
    );
    assert_eq!(
        verbs["token"],
        json!("args: text:str. Writes to stdout (no newline). Returns None.")
    );
    assert_eq!(
        verbs["say"],
        json!(
            "args: text:str, source:str?. Prints `  [source] text` on its own line. Returns None."
        )
    );
    assert_eq!(
        verbs["status"],
        json!("args: phase:str, source:str?, detail:dict?. Renders one-line phase markers for queued / thinking / tool_calling (entry+exit). `streaming` and `done` produce no output (token + done handlers cover them). Returns None.")
    );
    assert_eq!(
        verbs["context"],
        json!("args: phase:str ('compacted'|'too_small'), source:str?, detail:dict?. Renders a one-line context-protocol notice.")
    );
    assert_eq!(
        verbs["intro_booting"],
        json!("First PTY push, BEFORE boot. Identity + the pull/push control-plane map — port-independent, so it prints instantly. No args. Returns None.")
    );
    // …and so is `_booted`'s, quotes included.
    assert_eq!(
        verbs["booted"],
        json!("Final PTY push — the kernel's \"all booted\" signal, sent AFTER the boot loop.")
    );
}

/// A9.4 — the vendored readme is python's, byte for byte.
#[test]
fn readme_is_pythons_verbatim() {
    let py = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(4)
        .expect("repo's src/lib above the crate")
        .join("python/bundled_agents/cli/src/cli/readme.md");
    assert_eq!(README, std::fs::read_to_string(&py).expect("python readme"));
}

/// A14.1 — the vendored cheatsheet is python's `cli/help.md`, byte for byte.
/// The binary prints THIS file, so a drift here is a drift in `fantastic --help`.
#[test]
fn help_md_is_pythons_verbatim() {
    let py = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(4)
        .expect("repo's src/lib above the crate")
        .join("python/bundled_agents/cli/src/cli/help.md");
    assert_eq!(HELP, std::fs::read_to_string(&py).expect("python help.md"));
    assert_eq!(HELP.len(), 1545, "python's help.md is 1545 bytes");
}

#[tokio::test]
async fn an_unknown_verb_is_dropped_not_refused() {
    // A render surface that answered `unknown verb` would turn a cosmetic gap
    // into a FAILED SEND for whoever was streaming to it.
    let reply = dispatch(json!({"type": "nonsense_verb"}))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reply, Value::Null);
    assert!(reply.get("error").is_none());
}

#[tokio::test]
async fn every_accepted_verb_answers_null_rather_than_an_error() {
    for verb in ACCEPTS {
        let reply = dispatch(json!({"type": verb, "text": "x"}))
            .await
            .unwrap()
            .unwrap();
        assert!(reply.get("error").is_none(), "{verb} refused");
    }
}

/// A9.2 — the intro is the python map, and the identity names THIS runtime.
#[tokio::test]
async fn intro_booting_prints_the_control_plane_map() {
    let k = kernel();
    let lines = intro_booting_lines(&k);
    assert_eq!(lines.len(), 5);
    // No root yet in a bare kernel — python falls back to its own root id, so
    // rust falls back to rust's.
    assert_eq!(
        lines[0],
        format!(
            "[fantastic] rust · env=host · root=kernel_state · pid {} — booting…",
            std::process::id()
        )
    );
    assert_eq!(
        lines[1],
        "  one envelope: send(<id>, {\"type\":\"<verb>\", …})   ·   kernel = root   ·   full map: reflect readme=true"
    );
    assert_eq!(
        lines[2],
        "  PULL  ask → reply        REST POST /<rest>/<id>        ·  this REPL: @<id> <verb> k=v"
    );
    assert_eq!(
        lines[3],
        "  PUSH  async stream/emit  WS /<id>/ws : watch{src} · emit{target,payload} · state_subscribe"
    );
    assert_eq!(
        lines[4],
        "  REACH one call by id, any unit: compute(exec_bridge) · infer(ai) · memory(text_file) · shell(terminal_backend)"
    );
    assert_eq!(
        dispatch(json!({"type": "intro_booting"})).await.unwrap(),
        Some(Value::Null)
    );
}

/// A9.2 — `booted` is a DUMB SINK: one fixed line, no tree inspection. The
/// bare kernel here has no agents at all, and the verb still answers.
#[tokio::test]
async fn booted_is_a_dumb_sink_no_tree_inspection() {
    assert_eq!(
        BOOTED_LINE,
        "[kernel] up — all booted. attach via the map above, or reflect readme=true"
    );
    let k = kernel();
    assert!(k.root().is_none());
    let reply = CliBundle
        .handle(&AgentId("cli".into()), &json!({"type": "booted"}), &k)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reply, Value::Null);
}

/// A9.5 — the tool-call entry line renders `json.dumps(args or {})`, not
/// serde's compact spelling: python separates with `, ` and `: `, and escapes
/// non-ASCII.
#[test]
fn status_handler_renders_phase_markers() {
    let line = status(json!({
        "phase": "tool_calling",
        "source": "ai",
        "detail": {"tool": {"verb": "read", "target": "notes", "args": {"path": "é", "n": 2}}},
    }))
    .unwrap()
    .unwrap();
    assert_eq!(
        line,
        "  [ai] → read(notes)  {\"path\": \"\\u00e9\", \"n\": 2}"
    );
    // A falsy `args` is python's `or {}`.
    assert_eq!(
        status(json!({"phase": "tool_calling", "detail": {"tool": {"verb": "v", "target": "t", "args": null}}}))
            .unwrap()
            .unwrap(),
        "  → v(t)  {}"
    );
    // The exit line is chosen by KEY PRESENCE, so a null preview still exits.
    assert_eq!(
        status(json!({"phase": "tool_calling", "detail": {"tool": {"verb": "v", "target": "t", "reply_preview": null}}}))
            .unwrap()
            .unwrap(),
        "  ← v(t)  None"
    );
    // queued / thinking / rate-limited.
    assert_eq!(
        status(json!({"phase": "queued", "detail": {"ahead": 3}}))
            .unwrap()
            .unwrap(),
        "  queued (3 ahead)"
    );
    assert_eq!(
        status(json!({"phase": "queued", "source": "ai"}))
            .unwrap()
            .unwrap(),
        "  [ai] queued (0 ahead)"
    );
    assert_eq!(
        status(json!({"phase": "thinking"})).unwrap().unwrap(),
        "  thinking…"
    );
    assert_eq!(
        status(json!({"phase": "thinking", "detail": {"waiting_on": "rate_limit"}}))
            .unwrap()
            .unwrap(),
        "  rate-limited; waiting ?s"
    );
    // `streaming` and `done` are silent; so is an unknown phase.
    assert!(status(json!({"phase": "streaming"})).unwrap().is_none());
    assert!(status(json!({"phase": "done"})).unwrap().is_none());
    assert!(status(json!({})).unwrap().is_none());
}

/// A9.6 — every value goes through python's `str()`, every default is keyed on
/// PRESENCE, and the prefix is decided by python truthiness.
#[test]
fn status_values_render_like_python_str() {
    // A present null is `None`, a bool is `True`, a container is a python repr.
    assert_eq!(
        status(json!({"phase": "queued", "detail": {"ahead": null}}))
            .unwrap()
            .unwrap(),
        "  queued (None ahead)"
    );
    assert_eq!(
        status(json!({"phase": "queued", "source": true, "detail": {"ahead": [1, "a"]}}))
            .unwrap()
            .unwrap(),
        "  [True] queued ([1, 'a'] ahead)"
    );
    assert_eq!(
        status(json!({"phase": "thinking", "detail": {"waiting_on": "rate_limit", "wait_s": 1.5}}))
            .unwrap()
            .unwrap(),
        "  rate-limited; waiting 1.5s"
    );
    // A falsy source takes the one-space prefix; a truthy non-string is str()ed.
    assert_eq!(
        status(json!({"phase": "thinking", "source": null}))
            .unwrap()
            .unwrap(),
        "  thinking…"
    );
    assert_eq!(
        status(json!({"phase": "thinking", "source": 0}))
            .unwrap()
            .unwrap(),
        "  thinking…"
    );
    assert_eq!(
        status(json!({"phase": "thinking", "source": 7}))
            .unwrap()
            .unwrap(),
        "  [7] thinking…"
    );
    // Non-string verb/target/preview: python's f-string str()s them.
    assert_eq!(
        status(json!({"phase": "tool_calling", "detail": {"tool": {"verb": 1, "target": {"a": 1}, "reply_preview": true}}}))
            .unwrap()
            .unwrap(),
        "  ← 1({'a': 1})  True"
    );
    // Context values go the same way.
    assert_eq!(
        context(json!({"phase": "compacted", "detail": {}}))
            .unwrap()
            .unwrap(),
        "  ⟲ context compacted (?, 0 dropped; recall to page back)"
    );
    assert_eq!(
        context(json!({"phase": "compacted", "source": "ai", "detail": {"strategy": null, "dropped_messages": 2}}))
            .unwrap()
            .unwrap(),
        "  [ai] ⟲ context compacted (None, 2 dropped; recall to page back)"
    );
    assert_eq!(
        context(json!({"phase": "too_small", "detail": {"hint": 5}}))
            .unwrap()
            .unwrap(),
        "  ✗ context too small — 5"
    );
    assert_eq!(
        context(json!({"phase": "too_small"})).unwrap().unwrap(),
        "  ✗ context too small — "
    );
    assert!(context(json!({"phase": "nonsense"})).unwrap().is_none());
}

/// A9.6 — the raise paths. Python does not render these; it dies, and the
/// kernel answers `{"error": …, "raised": true}`.
#[tokio::test]
async fn python_raise_paths_raise_here_too() {
    // `sys.stdout.write(5)` — note the clinic's `None`, not `NoneType`.
    for (text, name) in [
        (json!(5), "int"),
        (json!(null), "None"),
        (json!(1.5), "float"),
        (json!(true), "bool"),
        (json!([1]), "list"),
        (json!({"a": 1}), "dict"),
    ] {
        let err = dispatch(json!({"type": "token", "text": text}))
            .await
            .expect_err("a non-str write raises");
        assert_eq!(
            err.to_string(),
            format!("TypeError: write() argument must be str, not {name}")
        );
    }
    // An absent `text` is python's `""` default — no raise, no output.
    assert_eq!(
        dispatch(json!({"type": "token"})).await.unwrap(),
        Some(Value::Null)
    );
    // A truthy non-dict `detail` has no `.get`.
    for (detail, name) in [
        (json!("x"), "str"),
        (json!(5), "int"),
        (json!(true), "bool"),
        (json!([1]), "list"),
        (json!(1.5), "float"),
    ] {
        let err = status(json!({"phase": "queued", "detail": detail.clone()}))
            .expect_err("a truthy non-dict detail raises");
        assert_eq!(
            err.to_string(),
            format!("AttributeError: '{name}' object has no attribute 'get'")
        );
        let err = context(json!({"phase": "compacted", "detail": detail}))
            .expect_err("a truthy non-dict detail raises");
        assert_eq!(
            err.to_string(),
            format!("AttributeError: '{name}' object has no attribute 'get'")
        );
    }
    // …and so does a truthy non-dict `tool`.
    let err = status(json!({"phase": "tool_calling", "detail": {"tool": "x"}}))
        .expect_err("a truthy non-dict tool raises");
    assert_eq!(
        err.to_string(),
        "AttributeError: 'str' object has no attribute 'get'"
    );
    // A FALSY non-dict is python's `or {}` — no raise.
    assert_eq!(
        status(json!({"phase": "queued", "detail": 0}))
            .unwrap()
            .unwrap(),
        "  queued (0 ahead)"
    );
    assert_eq!(
        status(json!({"phase": "tool_calling", "detail": {"tool": ""}}))
            .unwrap()
            .unwrap(),
        "  → ()  {}"
    );
    // A non-string `text` on `say`/`error` is str()ed, not a raise: python
    // renders those through an f-string, not through `stdout.write`.
    assert_eq!(
        dispatch(json!({"type": "say", "text": 5})).await.unwrap(),
        Some(Value::Null)
    );
    assert_eq!(
        dispatch(json!({"type": "error", "text": null}))
            .await
            .unwrap(),
        Some(Value::Null)
    );
}
