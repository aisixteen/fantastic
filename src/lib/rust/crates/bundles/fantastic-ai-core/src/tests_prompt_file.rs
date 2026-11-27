//! The system block's placeholders and its FILE — python
//! `ai_core/tests/test_system_prompt_file.py`, over this harness.
//!
//! Placeholders `{self}` `{store}` `{howto}` are filled per agent and per
//! pass in whichever prompt is in force; `{self}\n\n{howto}` IS the
//! default block. `file_bridge_id` (a file view) + `system_prompt_file` (its path)
//! make a file — read THROUGH that view with its own verbs (`read`, `watch`), as the
//! agent — the block, re-read on the next pass after the view reports it changed. Not
//! set: nothing is looked up, the default block. Set and unusable: the pass FAILS,
//! loud. Precedence: per-call > record > file > default.

use super::*;

fn system_of(s: &Arc<MockScript>, n: usize) -> String {
    let msgs = s.request_messages(n);
    assert_eq!(msgs[0]["role"], "system", "{msgs:?}");
    msgs[0]["content"].as_str().unwrap_or_default().to_string()
}

async fn update(kernel: &Arc<Kernel>, backend: &AgentId, field: &str, v: Value) -> Value {
    let mut p = json!({"type": "update_agent", "id": backend.as_str()});
    p[field] = v;
    fantastic_kernel::test_support::send_settled(kernel, vec![Item::new(AgentId::from("kernel_state"), p)])
        .await
        .remove(0)
        .result
}

async fn run_pass(
    kernel: &Arc<Kernel>,
    backend: &AgentId,
    s: &Arc<MockScript>,
    n: usize,
    extra: Value,
) -> String {
    let mut p = json!({"type": "send", "text": "go"});
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        p[k] = v;
    }
    let _ = crate::tests::settled(kernel, fantastic_kernel::test_support::send_settled(kernel, vec![Item::new(backend.clone(), p)]))
        .await
        .remove(0)
        .result;
    assert!(until_passes(s, n).await, "pass {n} never ran");
    system_of(s, n - 1)
}

/// A pass that must fail loud: the model is not called, and the pass ends `failed`
/// (reason `error`) on the agent's inbox, its `error` naming why — `{error}`. The
/// message itself was acked when it was stored (#795).
async fn failed_pass(kernel: &Arc<Kernel>, backend: &AgentId, s: &Arc<MockScript>) -> Value {
    let mut rx = rebind_backend_inbox(kernel, backend);
    let before = s.passes_served();
    let r = crate::tests::settled(kernel, fantastic_kernel::test_support::send_settled(kernel, vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )]))
        .await
        .remove(0)
        .result;
    assert_eq!(r["accepted"], json!(true), "{r}");
    assert_eq!(
        s.passes_served(),
        before,
        "the model was called on another block"
    );
    let evs = drain(&mut rx).await;
    let failed = evs
        .iter()
        .find(|e| {
            e["type"] == json!("status")
                && e["phase"] == json!("failed")
                && e["detail"]["reason"] == json!("error")
        })
        .unwrap_or_else(|| panic!("{evs:#?}"));
    json!({"error": failed["detail"]["error"]})
}

/// Name a file view of `prompt.md` on the harness's own leg, with the agent's line on it.
async fn name_file(kernel: &Arc<Kernel>, backend: &AgentId) -> String {
    name_prompt_view(kernel, backend, "prompt.md").await
}

/// The private inbox the loader watches the view through, while it is armed.
fn watch_inbox(backend: &AgentId) -> AgentId {
    AgentId::from(format!("{backend}/prompt_file"))
}

/// The view's own `reflect`: is its change relay on?
async fn view_watching(kernel: &Arc<Kernel>, view: &str) -> Value {
    fantastic_kernel::test_support::send_settled(kernel, vec![Item::new(
            AgentId::from(view),
            json!({"type": "reflect"}),
        )])
        .await
        .remove(0)
        .result["watching"]
        .clone()
}

fn passes(n: usize) -> Vec<Vec<ScriptEvent>> {
    (0..n)
        .map(|_| pass(r#"ok <call name="done"></call>"#))
        .collect()
}

/// The harness's own leg (rooted at `.fantastic`) holds the prompt file.
fn leg(backend: &AgentId) -> String {
    format!("ff_{}", backend.as_str())
}

fn prompt_path(tmp: &TempDir) -> std::path::PathBuf {
    let dir = tmp.path().join(".fantastic");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("prompt.md")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_default_layout_in_placeholders_is_the_default_block() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "pf_ph").await;
    let s = script(&backend, passes(2), Duration::ZERO);
    let default = run_pass(&kernel, &backend, &s, 1, json!({})).await;
    update(
        &kernel,
        &backend,
        "system_prompt",
        json!("{self}\n\n{howto}"),
    )
    .await;
    assert_eq!(run_pass(&kernel, &backend, &s, 2, json!({})).await, default);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn placeholders_fill_and_unknown_ones_stay() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "pf_ph2").await;
    let store = crate::helpers::agent_meta(&backend, &kernel)["memory_store_id"]
        .as_str()
        .unwrap()
        .to_string();
    update(
        &kernel,
        &backend,
        "system_prompt",
        json!("store={store} {nope}"),
    )
    .await;
    let s = script(&backend, passes(2), Duration::ZERO);
    assert_eq!(
        run_pass(&kernel, &backend, &s, 1, json!({})).await,
        format!("store={store} {{nope}}")
    );
    update(&kernel, &backend, "system_prompt", json!("me={self}")).await;
    let per_call = run_pass(&kernel, &backend, &s, 2, json!({})).await;
    assert!(
        per_call.starts_with(&format!("me=You are `{}`.", backend.as_str())),
        "{per_call}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_file_is_the_block_and_an_edit_is_live_next_pass() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "pf_file").await;
    std::fs::write(prompt_path(&tmp), "FILE-ROLE\n").unwrap();
    let view = name_file(&kernel, &backend).await;
    let s = script(&backend, passes(12), Duration::ZERO);
    assert_eq!(
        run_pass(&kernel, &backend, &s, 1, json!({})).await,
        "FILE-ROLE\n"
    );
    // armed on the view with its own verbs: no refusal, the watch inbox held, the relay on
    assert!(crate::prompt_file::error_of(&backend).is_none());
    assert!(kernel.inboxes.contains_key(&watch_inbox(&backend)));
    assert_eq!(view_watching(&kernel, &view).await, json!(true));
    tokio::time::sleep(Duration::from_millis(300)).await;
    std::fs::write(prompt_path(&tmp), "EDITED {self}\n").unwrap();
    // the watcher marks it stale; the next pass after that reads it again
    let mut got = String::new();
    for n in 2..=10 {
        got = run_pass(&kernel, &backend, &s, n, json!({})).await;
        if got.starts_with("EDITED") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        got.starts_with(&format!("EDITED You are `{}`.", backend.as_str())),
        "{got}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_record_prompt_beats_the_file_and_clearing_the_field_drops_it() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "pf_prec").await;
    std::fs::write(prompt_path(&tmp), "FILE\n").unwrap();
    name_file(&kernel, &backend).await;
    update(&kernel, &backend, "system_prompt", json!("RECORD")).await;
    let s = script(&backend, passes(3), Duration::ZERO);
    assert_eq!(run_pass(&kernel, &backend, &s, 1, json!({})).await, "RECORD");
    update(&kernel, &backend, "system_prompt", Value::Null).await;
    assert_eq!(run_pass(&kernel, &backend, &s, 2, json!({})).await, "FILE\n");
    assert!(kernel.inboxes.contains_key(&watch_inbox(&backend)));
    update(&kernel, &backend, "system_prompt_file", Value::Null).await;
    let default = run_pass(&kernel, &backend, &s, 3, json!({})).await;
    assert!(
        default.contains(&format!("You are `{}`.", backend.as_str())),
        "{default}"
    );
    assert!(crate::prompt_file::error_of(&backend).is_none());
    // cleared: unarmed — the watch inbox is gone
    assert!(!kernel.inboxes.contains_key(&watch_inbox(&backend)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_file_fails_the_pass_until_it_appears() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "pf_miss").await;
    prompt_path(&tmp);
    name_file(&kernel, &backend).await;
    let s = script(&backend, passes(12), Duration::ZERO);
    let r = failed_pass(&kernel, &backend, &s).await;
    assert!(
        r["error"]
            .as_str()
            .unwrap_or_default()
            .contains("prompt.md"),
        "{r}"
    );
    let why = crate::prompt_file::error_of(&backend).unwrap_or_default();
    assert!(why.contains("prompt.md"), "{why}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    std::fs::write(prompt_path(&tmp), "NOW\n").unwrap();
    let mut got = String::new();
    // a pass before the watcher reported it still fails; one after reads it
    for _ in 1..=10 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let before = s.passes_served();
        let _ = crate::tests::settled(&kernel, fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(
                backend.clone(),
                json!({"type": "send", "text": "go"}),
            )]))
            .await
            .remove(0)
            .result;
        if s.passes_served() > before {
            got = system_of(&s, before);
            break;
        }
    }
    assert_eq!(got, "NOW\n");
    assert!(crate::prompt_file::error_of(&backend).is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_path_without_file_bridge_id_fails_and_a_bridge_alone_is_the_default() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "pf_nob").await;
    update(&kernel, &backend, "system_prompt_file", json!("prompt.md")).await;
    let s = script(&backend, passes(2), Duration::ZERO);
    let r = failed_pass(&kernel, &backend, &s).await;
    assert!(
        r["error"]
            .as_str()
            .unwrap_or_default()
            .contains("no file_bridge_id"),
        "{r}"
    );
    update(&kernel, &backend, "system_prompt_file", Value::Null).await;
    update(&kernel, &backend, "file_bridge_id", json!(leg(&backend))).await;
    let block = run_pass(&kernel, &backend, &s, 1, json!({})).await;
    assert!(
        block.contains(&format!("You are `{}`.", backend.as_str())),
        "{block}"
    );
}

/// The loader sends only a file view's verbs: a bridge named as the view (the agent holding
/// every verb on it) answers no `watch`, so the pass fails loud and nothing is read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bridge_named_as_the_view_fails_the_pass() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "pf_brg").await;
    std::fs::write(prompt_path(&tmp), "BRIDGE-READ\n").unwrap();
    let r = update(&kernel, &backend, "file_bridge_id", json!(leg(&backend))).await;
    assert!(r.get("error").is_none(), "{r}");
    let r = update(&kernel, &backend, "system_prompt_file", json!("prompt.md")).await;
    assert!(r.get("error").is_none(), "{r}");
    let s = script(&backend, passes(1), Duration::ZERO);
    let r = failed_pass(&kernel, &backend, &s).await;
    let why = r["error"].as_str().unwrap_or_default();
    assert!(why.contains(&format!("{} watch", leg(&backend))), "{r}");
    assert!(!why.contains("BRIDGE-READ"), "{r}");
    assert!(!kernel.inboxes.contains_key(&watch_inbox(&backend)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_file_fails_the_pass() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "pf_empty").await;
    std::fs::write(prompt_path(&tmp), " \n").unwrap();
    name_file(&kernel, &backend).await;
    let s = script(&backend, passes(1), Duration::ZERO);
    let r = failed_pass(&kernel, &backend, &s).await;
    assert!(
        r["error"].as_str().unwrap_or_default().contains("empty"),
        "{r}"
    );
}

#[test]
fn a_bad_value_is_refused_at_the_write() {
    // the engine refuses a value that is not text, or text with no non-space character
    for (field, bad) in [
        ("system_prompt_file", json!("")),
        ("system_prompt_file", json!("  ")),
        ("system_prompt_file", json!(5)),
        ("system_prompt_file", json!({"bridge": "x", "path": "p.md"})),
        ("file_bridge_id", json!("")),
        ("file_bridge_id", json!(5)),
        ("file_bridge_id", json!(["x"])),
    ] {
        let mut meta = serde_json::Map::new();
        meta.insert(field.to_string(), bad.clone());
        let e = crate::tests_decl::refused_at_the_write(&meta);
        assert!(e.is_some_and(|t| t.contains(field)), "{field}={bad}");
    }
    let ok = json!({"file_bridge_id": "x", "system_prompt_file": "p.md"})
        .as_object()
        .cloned()
        .unwrap();
    assert_eq!(crate::tests_decl::refused_at_the_write(&ok), None);
    let null = json!({"file_bridge_id": null, "system_prompt_file": null})
        .as_object()
        .cloned()
        .unwrap();
    assert_eq!(crate::tests_decl::refused_at_the_write(&null), None, "null clears");
}
