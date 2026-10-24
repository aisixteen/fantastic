//! Inline agent views in a chat (#339): `embed {id}` / `unembed {id}` store a card
//! row in the chat's conversation; history replays it; the prompt never sees it.
//! A child of `tests`, so it drives the same harness.

use super::*;

async fn call(kernel: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    kernel
        .send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
        .result
}

/// A host agent with a panel: a `view` record and the user's grants on it.
async fn panel(kernel: &Arc<Kernel>, id: &str, callable_by: Value) -> Value {
    call(
        kernel,
        "kernel_state",
        json!({
            "type": "create_agent", "parent_id": "kernel", "id": id,
            "handler_module": fantastic_ram_store::HANDLER_MODULE,
            "view": {"kind": "html", "bytes_file": "panel.html", "calls": ["get_html", "watch"]},
            "callable_by": callable_by,
        }),
    )
    .await
}

fn good_grants() -> Value {
    json!({"user": ["get_html", "watch"]})
}

async fn digests(kernel: &Arc<Kernel>, backend: &AgentId) -> Vec<Value> {
    let h = call(kernel, backend.as_str(), json!({"type": "history"})).await;
    h["rows"].as_array().cloned().unwrap_or_default()
}

fn rec_of(kernel: &Arc<Kernel>, id: &str) -> Value {
    let a = kernel
        .agents
        .get(&AgentId::from(id))
        .map(|e| Arc::clone(&e));
    serde_json::to_value(a.expect("a live agent").record()).unwrap()
}

fn card_ids(rows: &[Value]) -> Vec<String> {
    crate::embed::cards(rows)
}

#[test]
fn the_chat_card_cap_is_1200() {
    assert_eq!(crate::CHAT_CARD_MAX_HEIGHT, 1200);
}

#[tokio::test]
async fn embed_stores_a_row_and_history_replays_it() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "emb_row").await;
    let id = id_for(&tmp, "panel");
    let made = panel(&kernel, &id, good_grants()).await;
    assert!(made.get("error").is_none(), "{made}");

    let r = call(
        &kernel,
        backend.as_str(),
        json!({"type": "embed", "id": id}),
    )
    .await;
    assert_eq!(r["embedded"], json!(id), "{r}");
    assert_eq!(r["card"]["max_height"], json!(1200), "{r}");
    assert_eq!(r["card"]["width"], json!("100%"), "{r}");

    let rows = digests(&kernel, &backend).await;
    let embed = rows
        .iter()
        .find(|d| d["kind"] == "embed")
        .expect("an embed row");
    assert_eq!(embed["head"], json!(id));
    assert_eq!(card_ids(&rows), vec![id.clone()]);

    // a restart: the head cache is dropped, the store still has it
    crate::memory::forget_head(&backend, &kernel);
    let rows = digests(&kernel, &backend).await;
    assert_eq!(card_ids(&rows), vec![id.clone()]);

    // embedding it again is idempotent: no second row
    let again = call(
        &kernel,
        backend.as_str(),
        json!({"type": "embed", "id": id}),
    )
    .await;
    assert_eq!(again["already"], json!(true), "{again}");
    assert_eq!(digests(&kernel, &backend).await.len(), rows.len());
}

#[tokio::test]
async fn embed_refuses_missing_view_missing_agent_and_missing_grants() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "emb_refuse").await;
    let before = call(&kernel, backend.as_str(), json!({"type": "history"})).await["total"].clone();

    let gone = call(
        &kernel,
        backend.as_str(),
        json!({"type": "embed", "id": "nobody_here"}),
    )
    .await;
    assert_eq!(gone["reason"], json!("not_found"), "{gone}");

    // a bare agent: no `view` record
    let bare = id_for(&tmp, "bare");
    call(
        &kernel,
        "kernel_state",
        json!({"type": "create_agent", "parent_id": "kernel", "id": bare,
            "handler_module": fantastic_ram_store::HANDLER_MODULE}),
    )
    .await;
    let nv = call(
        &kernel,
        backend.as_str(),
        json!({"type": "embed", "id": bare}),
    )
    .await;
    assert_eq!(nv["reason"], json!("no_view"), "{nv}");

    // a view whose callable_by lacks get_html for the user
    let weak = id_for(&tmp, "weak");
    panel(&kernel, &weak, json!({"user": ["watch"]})).await;
    let gm = call(
        &kernel,
        backend.as_str(),
        json!({"type": "embed", "id": weak}),
    )
    .await;
    assert_eq!(gm["reason"], json!("grants_missing"), "{gm}");
    assert_eq!(
        gm["grants_missing"],
        json!({ weak.clone(): ["get_html"] }),
        "{gm}"
    );

    let me = call(
        &kernel,
        backend.as_str(),
        json!({"type": "embed", "id": backend.as_str()}),
    )
    .await;
    assert!(me.get("error").is_some(), "self-embed refused: {me}");

    let after = call(&kernel, backend.as_str(), json!({"type": "history"})).await["total"].clone();
    assert_eq!(before, after, "nothing appended");
}

#[tokio::test]
async fn unembed_removes_card_live_and_deleted_and_leaves_agent() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "emb_un").await;
    let id = id_for(&tmp, "panel");
    panel(&kernel, &id, good_grants()).await;
    call(
        &kernel,
        backend.as_str(),
        json!({"type": "embed", "id": id}),
    )
    .await;

    let rec_before = rec_of(&kernel, &id);
    let r = call(
        &kernel,
        backend.as_str(),
        json!({"type": "unembed", "id": id}),
    )
    .await;
    assert_eq!(r["unembedded"], json!(id), "{r}");
    let rows = digests(&kernel, &backend).await;
    assert!(card_ids(&rows).is_empty(), "{rows:?}");
    let rec_after = rec_of(&kernel, &id);
    assert_eq!(rec_before, rec_after, "the agent is untouched");

    // embed again shows it again
    call(
        &kernel,
        backend.as_str(),
        json!({"type": "embed", "id": id}),
    )
    .await;
    assert_eq!(
        card_ids(&digests(&kernel, &backend).await),
        vec![id.clone()]
    );

    // a deleted agent's card is cleared by unembed too
    let del = call(
        &kernel,
        "kernel_state",
        json!({"type": "delete_agent", "id": id}),
    )
    .await;
    assert!(del.get("error").is_none(), "{del}");
    let r = call(
        &kernel,
        backend.as_str(),
        json!({"type": "unembed", "id": id}),
    )
    .await;
    assert_eq!(r["unembedded"], json!(id), "{r}");
    assert!(card_ids(&digests(&kernel, &backend).await).is_empty());
}

#[tokio::test]
async fn embed_rows_never_reach_the_prompt() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "emb_prompt").await;
    let id = id_for(&tmp, "panel");
    panel(&kernel, &id, good_grants()).await;
    let state = crate::state::state_for(&backend);
    let assemble = || async {
        fantastic_kernel::test_support::as_agent(
            backend.clone(),
            crate::assembly::assemble_messages(
                &backend,
                &state,
                "hi",
                &kernel,
                "probe",
                None,
                crate::caching::Caching::Cleanup,
            ),
        )
        .await
        .expect("store read")
    };
    let before = assemble().await;
    call(
        &kernel,
        backend.as_str(),
        json!({"type": "embed", "id": id}),
    )
    .await;
    call(
        &kernel,
        backend.as_str(),
        json!({"type": "unembed", "id": id}),
    )
    .await;
    call(
        &kernel,
        backend.as_str(),
        json!({"type": "embed", "id": id}),
    )
    .await;
    assert_eq!(before, assemble().await);
}
