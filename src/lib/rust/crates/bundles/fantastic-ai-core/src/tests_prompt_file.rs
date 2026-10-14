//! The system block's placeholders and its FILE — python
//! `ai_core/tests/test_system_prompt_file.py`, over this harness.
//!
//! Placeholders `{tree}` `{self}` `{store}` `{howto}` are filled per agent and per
//! turn in whichever prompt is in force; `{tree}\n\n{self}\n\n{howto}` IS the
//! default block. `file_bridge_id` + `system_prompt_file` (a path on it) make a file
//! — read THROUGH that file_bridge, as the agent — the block, re-read on the next
//! turn after the bridge reports it changed. Not set: nothing is looked up, the
//! default block. Set and unusable: the turn FAILS, loud. Precedence: per-call >
//! record > file > default.

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

/// A turn that must fail loud: its reply names the reason and the model is not
/// called.
async fn failed_turn(kernel: &Arc<Kernel>, backend: &AgentId, s: &Arc<MockScript>) -> Value {
    let before = s.passes_served();
    let r = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], "system_prompt_file", "{r}");
    assert_eq!(
        s.passes_served(),
        before,
        "the model was called on another block"
    );
    r
}

/// Name the harness's own leg and a path on it.
async fn name_file(kernel: &Arc<Kernel>, backend: &AgentId) {
    let r = update(kernel, backend, "file_bridge_id", json!(leg(backend))).await;
    assert!(r.get("error").is_none(), "{r}");
    let r = update(kernel, backend, "system_prompt_file", json!("prompt.md")).await;
    assert!(r.get("error").is_none(), "{r}");
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
    name_file(&kernel, &backend).await;
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
    name_file(&kernel, &backend).await;
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
async fn a_missing_file_fails_the_turn_until_it_appears() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "pf_miss").await;
    prompt_path(&tmp);
    name_file(&kernel, &backend).await;
    let s = script(&backend, passes(12), Duration::ZERO);
    let r = failed_turn(&kernel, &backend, &s).await;
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
    // a turn before the watcher reported it still fails; one after reads it
    for _ in 1..=10 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let before = s.passes_served();
        let _ = kernel
            .send(vec![Item::new(
                backend.clone(),
                json!({"type": "send", "text": "go"}),
            )])
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
    let r = failed_turn(&kernel, &backend, &s).await;
    assert!(
        r["error"]
            .as_str()
            .unwrap_or_default()
            .contains("no file_bridge_id"),
        "{r}"
    );
    update(&kernel, &backend, "system_prompt_file", Value::Null).await;
    update(&kernel, &backend, "file_bridge_id", json!(leg(&backend))).await;
    let block = turn(&kernel, &backend, &s, 1, json!({})).await;
    assert!(
        block.contains(&format!("You are `{}`.", backend.as_str())),
        "{block}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_file_fails_the_turn() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "pf_empty").await;
    std::fs::write(prompt_path(&tmp), " \n").unwrap();
    name_file(&kernel, &backend).await;
    let s = script(&backend, passes(1), Duration::ZERO);
    let r = failed_turn(&kernel, &backend, &s).await;
    assert!(
        r["error"].as_str().unwrap_or_default().contains("empty"),
        "{r}"
    );
}

#[test]
fn a_bad_value_is_refused_at_the_write() {
    // every real backend's `validate_meta` runs the shared schema check
    let sch = crate::schema::schema(vec![]);
    let rec = json!({"memory_store_id": "m"})
        .as_object()
        .cloned()
        .unwrap();
    for (field, bad, refusal) in [
        (
            "system_prompt_file",
            json!(""),
            crate::prompt_file::SYSTEM_PROMPT_FILE_REFUSAL,
        ),
        (
            "system_prompt_file",
            json!("  "),
            crate::prompt_file::SYSTEM_PROMPT_FILE_REFUSAL,
        ),
        (
            "system_prompt_file",
            json!(5),
            crate::prompt_file::SYSTEM_PROMPT_FILE_REFUSAL,
        ),
        (
            "system_prompt_file",
            json!({"bridge": "x", "path": "p.md"}),
            crate::prompt_file::SYSTEM_PROMPT_FILE_REFUSAL,
        ),
        (
            "file_bridge_id",
            json!(""),
            crate::prompt_file::FILE_BRIDGE_ID_REFUSAL,
        ),
        (
            "file_bridge_id",
            json!(5),
            crate::prompt_file::FILE_BRIDGE_ID_REFUSAL,
        ),
        (
            "file_bridge_id",
            json!(["x"]),
            crate::prompt_file::FILE_BRIDGE_ID_REFUSAL,
        ),
    ] {
        let mut meta = serde_json::Map::new();
        meta.insert(field.to_string(), bad.clone());
        let e = crate::schema::validate(&sch, "b", &meta, &rec);
        assert_eq!(e.as_deref(), Some(refusal), "{field}={bad}");
    }
    let ok = json!({"file_bridge_id": "x", "system_prompt_file": "p.md"})
        .as_object()
        .cloned()
        .unwrap();
    assert_eq!(crate::schema::validate(&sch, "b", &ok, &rec), None);
    let null = json!({"file_bridge_id": null, "system_prompt_file": null})
        .as_object()
        .cloned()
        .unwrap();
    assert_eq!(
        crate::schema::validate(&sch, "b", &null, &rec),
        None,
        "null clears"
    );
    let names: Vec<String> = sch["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap().to_string())
        .collect();
    assert!(
        names.ends_with(&[
            "system_prompt".to_string(),
            "file_bridge_id".to_string(),
            "system_prompt_file".to_string(),
            "pre_system_prompt".to_string(),
            "post_system_prompt".to_string()
        ]),
        "{names:?}"
    );
}
