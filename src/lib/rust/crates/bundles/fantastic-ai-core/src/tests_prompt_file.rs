//! The system block's placeholders and its FILE (owner, 2026-10-08) — python
//! `ai_core/tests/test_system_prompt_file.py`, over this harness.
//!
//! Placeholders `{tree}` `{self}` `{store}` `{howto}` are filled per agent and per
//! turn in whichever prompt is in force; `{tree}\n\n{self}\n\n{howto}` IS the
//! default block. `system_prompt_file: {bridge, path}` makes a file — read THROUGH
//! that file_bridge, as the agent — the block, re-read on the next turn after the
//! bridge reports it changed. Precedence: per-call > record > file > default.

use super::*;

fn system_of(s: &Arc<MockScript>, n: usize) -> String {
    let msgs = s.request_messages(n);
    assert_eq!(msgs[0]["role"], "system", "{msgs:?}");
    msgs[0]["content"].as_str().unwrap_or_default().to_string()
}

async fn update(kernel: &Arc<Kernel>, backend: &AgentId, field: &str, v: Value) -> Value {
    let mut p = json!({"type": "update_agent", "id": backend.as_str()});
    p[field] = v;
    kernel
        .send(vec![Item::new(AgentId::from("kernel_state"), p)])
        .await
        .remove(0)
        .result
}

async fn turn(
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
    let _ = kernel
        .send(vec![Item::new(backend.clone(), p)])
        .await
        .remove(0)
        .result;
    assert!(until_passes(s, n).await, "turn {n} never ran");
    system_of(s, n - 1)
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
    let default = turn(&kernel, &backend, &s, 1, json!({})).await;
    update(
        &kernel,
        &backend,
        "system_prompt",
        json!("{tree}\n\n{self}\n\n{howto}"),
    )
    .await;
    assert_eq!(turn(&kernel, &backend, &s, 2, json!({})).await, default);
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
        turn(&kernel, &backend, &s, 1, json!({})).await,
        format!("store={store} {{nope}}")
    );
    let per_call = turn(
        &kernel,
        &backend,
        &s,
        2,
        json!({"system_prompt": "me={self}"}),
    )
    .await;
    assert!(
        per_call.starts_with(&format!("me=You are `{}`.", backend.as_str())),
        "{per_call}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_file_is_the_block_and_an_edit_is_live_next_turn() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "pf_file").await;
    std::fs::write(prompt_path(&tmp), "FILE-ROLE\n").unwrap();
    let r = update(
        &kernel,
        &backend,
        "system_prompt_file",
        json!({"bridge": leg(&backend), "path": "prompt.md"}),
    )
    .await;
    assert!(r.get("error").is_none(), "{r}");
    let s = script(&backend, passes(12), Duration::ZERO);
    assert_eq!(
        turn(&kernel, &backend, &s, 1, json!({})).await,
        "FILE-ROLE\n"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    std::fs::write(prompt_path(&tmp), "EDITED {self}\n").unwrap();
    // the watcher marks it stale; the next turn after that reads it again
    let mut got = String::new();
    for n in 2..=10 {
        got = turn(&kernel, &backend, &s, n, json!({})).await;
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
    update(
        &kernel,
        &backend,
        "system_prompt_file",
        json!({"bridge": leg(&backend), "path": "prompt.md"}),
    )
    .await;
    update(&kernel, &backend, "system_prompt", json!("RECORD")).await;
    let s = script(&backend, passes(3), Duration::ZERO);
    assert_eq!(turn(&kernel, &backend, &s, 1, json!({})).await, "RECORD");
    update(&kernel, &backend, "system_prompt", Value::Null).await;
    assert_eq!(turn(&kernel, &backend, &s, 2, json!({})).await, "FILE\n");
    update(&kernel, &backend, "system_prompt_file", Value::Null).await;
    let default = turn(&kernel, &backend, &s, 3, json!({})).await;
    assert!(
        default.contains(&format!("You are `{}`.", backend.as_str())),
        "{default}"
    );
    assert!(crate::prompt_file::error_of(&backend).is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_file_is_the_default_block_and_reflect_says_why() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "pf_miss").await;
    update(
        &kernel,
        &backend,
        "system_prompt_file",
        json!({"bridge": leg(&backend), "path": "prompt.md"}),
    )
    .await;
    let s = script(&backend, passes(12), Duration::ZERO);
    let block = turn(&kernel, &backend, &s, 1, json!({})).await;
    assert!(
        block.contains(&format!("You are `{}`.", backend.as_str())),
        "the default block"
    );
    // the harness backend is a mock with its own reflect: the shared state says why
    let why = crate::prompt_file::error_of(&backend).unwrap_or_default();
    assert!(why.contains("prompt.md"), "{why}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    std::fs::write(prompt_path(&tmp), "NOW\n").unwrap();
    let mut got = String::new();
    for n in 2..=10 {
        got = turn(&kernel, &backend, &s, n, json!({})).await;
        if got == "NOW\n" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(got, "NOW\n");
    assert!(crate::prompt_file::error_of(&backend).is_none());
}

#[test]
fn a_bad_value_is_refused_at_the_write() {
    // every real backend's `validate_meta` runs the shared schema check
    let sch = crate::schema::schema(vec![]);
    for bad in [
        json!("prompt.md"),
        json!({"bridge": "x"}),
        json!({"bridge": "", "path": "p.md"}),
        json!({"bridge": "x", "path": "p.md", "extra": 1}),
        json!({"bridge": 5, "path": "p.md"}),
    ] {
        let meta = json!({"system_prompt_file": bad.clone()})
            .as_object()
            .cloned()
            .unwrap();
        let rec = json!({"memory_store_id": "m"})
            .as_object()
            .cloned()
            .unwrap();
        let e = crate::schema::validate(&sch, "b", &meta, &rec);
        assert_eq!(e.as_deref(), Some(crate::prompt_file::REFUSAL), "{bad}");
    }
    let ok = json!({"system_prompt_file": {"bridge": "x", "path": "p.md"}})
        .as_object()
        .cloned()
        .unwrap();
    let rec = json!({"memory_store_id": "m"})
        .as_object()
        .cloned()
        .unwrap();
    assert_eq!(crate::schema::validate(&sch, "b", &ok, &rec), None);
    let names: Vec<String> = sch["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap().to_string())
        .collect();
    assert!(
        names.ends_with(&[
            "system_prompt".to_string(),
            "system_prompt_file".to_string()
        ]),
        "{names:?}"
    );
}
