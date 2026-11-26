//! An inquiry whose recipient's turn fails tells the asker (#722): every failed
//! turn that is not retried closes the inquiry its chain is answering with
//! `{error: "<agent>'s turn failed: <reason>"}`, and the asker reads it as an
//! ordinary `[reply to <pid> · <agent> inquiry]`. A provider failure is retried only
//! by `transport_retries` inside the pass (#795); past them the turn has failed.
//!
//! A child of `tests`, so it drives the same harness: the mock provider, a real
//! `ram_store` and `mk_kernel` backends.

use super::*;

const DONE: &str = r#"<call name="done"></call>"#;

/// The asker's `inquiry` to `target`, in the routing form.
fn ask(target: &AgentId) -> String {
    format!(
        "<call name=\"send\"><routing><target_id>{}</target_id></routing>\
         <payload><type>inquiry</type><text>go</text></payload></call>",
        target.as_str()
    )
}

/// The asker's three passes: ask, done for now, and read the answer.
fn asker_passes(target: &AgentId) -> Vec<Vec<ScriptEvent>> {
    vec![pass(ask(target)), pass(DONE), pass(DONE)]
}

/// Start the asker's run: its first pass sends the inquiry.
async fn start(kernel: &Arc<Kernel>, asker: &AgentId) {
    let _ = crate::tests::settled(kernel, kernel.operator_send(vec![Item::new(
            asker.clone(),
            json!({"type": "send", "text": "ask", "client_id": "probe"}),
        )]))
        .await;
}

/// The asker is handed the error reply naming the failed turn — in whichever of
/// its passes it lands: a fast failure can answer before the asker's own turn ends.
async fn told(fb: &Arc<MockScript>, target: &AgentId, why: &str) {
    let error = format!("{}'s turn failed: {why}", target.as_str());
    told_error(fb, target, &error).await;
}

/// The asker is handed an error reply containing `error`, as the reply to its inquiry.
async fn told_error(fb: &Arc<MockScript>, target: &AgentId, error: &str) {
    let head = format!(" · {} inquiry", target.as_str());
    for _ in 0..400 {
        let seen: Vec<String> = fb
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|m| Value::Array(m.clone()).to_string())
            .collect();
        if let Some(s) = seen.iter().find(|s| s.contains(error)) {
            assert!(s.contains("[reply to ") && s.contains(&head), "{s}");
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "the asker was never told {error}: {:?}",
        fb.requests.lock().unwrap()
    );
}

/// An asker wired to `target`, with its passes scripted.
async fn asker(kernel: &Arc<Kernel>, tmp: &TempDir, target: &AgentId) -> Arc<MockScript> {
    let b = add_peer(kernel, tmp, target).await;
    script(&b, asker_passes(target), Duration::ZERO)
}

fn peer_of(target: &AgentId) -> AgentId {
    AgentId::from(format!("{target}_peer").as_str())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_store_refusal_tells_the_asker() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "iq_store").await;
    let fb = asker(&kernel, &tmp, &a).await;
    // the store lets the agent read, not append: the turn fails fast
    fantastic_kernel::test_support::grant(&kernel, a.as_str(), MEMORY, json!({"call": ["read"]}));
    start(&kernel, &peer_of(&a)).await;
    // the store refuses the message's own row (#795: a message is stored when it
    // arrives), so the inquiry's reply is the store's refusal, naming the failed append
    told_error(&fb, &a, &format!("{a}: memory store 'memory' append failed")).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unusable_prompt_file_tells_the_asker() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "iq_pf").await;
    let fb = asker(&kernel, &tmp, &a).await;
    // a path with no file_bridge_id: the block cannot be read
    let _ = kernel.operator_send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": a.as_str(), "system_prompt_file": "prompt.md"}),
        )])
        .await;
    let s = script(&a, Vec::new(), Duration::ZERO);
    start(&kernel, &peer_of(&a)).await;
    told(
        &fb,
        &a,
        &format!("{a}: system_prompt_file 'prompt.md' is set but the record has no file_bridge_id"),
    )
    .await;
    assert_eq!(
        s.passes_served(),
        0,
        "the model was called on another block"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_record_tells_the_asker() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "iq_value").await;
    let fb = asker(&kernel, &tmp, &a).await;
    // a record the backend cannot run (`ValueError`), written past the gate
    fantastic_kernel::test_support::update(
        &kernel,
        &AgentId::from(a.as_str()),
        serde_json::Map::from_iter([("caching".to_string(), json!("bogus"))]),
    );
    let s = script(&a, Vec::new(), Duration::ZERO);
    start(&kernel, &peer_of(&a)).await;
    told(&fb, &a, "caching='bogus' is not one of the legal values").await;
    assert_eq!(s.passes_served(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_timed_out_turn_tells_the_asker() {
    let _guard = SEND_TIMEOUT_ENV_LOCK.lock().await;
    let _ceiling = Ceiling::set("0.5");
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "iq_timeout").await;
    let fb = asker(&kernel, &tmp, &a).await;
    script(&a, vec![pass("never arrives")], Duration::from_secs(3));
    start(&kernel, &peer_of(&a)).await;
    told(&fb, &a, "send: timeout after 0.5s").await;
}

/// A provider failure fails the turn (#795: no retry turn of its own), and the asker is
/// told it; nothing more is asked of the provider.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_provider_failure_that_gives_up_tells_the_asker() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "iq_provider").await;
    let fb = asker(&kernel, &tmp, &a).await;
    let fail = || vec![ScriptEvent::Fail("provider exploded".into())];
    let s = script(&a, vec![fail(), fail()], Duration::ZERO);
    start(&kernel, &peer_of(&a)).await;
    told(&fb, &a, "provider exploded").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(s.passes_served(), 1, "a failed turn is not re-run");
}

/// A fold-strategy agent over a small window with a goal and six closed chains:
/// its next turn folds.
async fn over_budget(kernel: &Arc<Kernel>, a: &AgentId) {
    let _ = kernel.operator_send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": a.as_str(),
                   "context_strategy": "fold", "context_window": 4000, "output_reserve": 0}),
        )])
        .await;
    put_row(
        kernel,
        a,
        json!({"role": "user", "parent": null, "content": "GOAL"}),
    )
    .await;
    for tag in ["a", "b", "c", "d", "e", "f"] {
        closed_chain(kernel, a, tag).await;
    }
}

/// One closed chain: a task, a call, its large reply and the done.
async fn closed_chain(kernel: &Arc<Kernel>, a: &AgentId, tag: &str) {
    let u = put_row(
        kernel,
        a,
        json!({"role": "user", "parent": null, "content": format!("task {tag}")}),
    )
    .await;
    let call = "<call name=\"send\"><routing><target_id>notes</target_id></routing>\
                <payload><type>read</type></payload></call>";
    let c = put_row(
        kernel,
        a,
        json!({"role": "assistant", "parent": u, "content": format!("{tag} {call}")}),
    )
    .await;
    let pid = kernel.mint_id();
    // DISTINCT bodies: identical ones are stubbed by the repeat-body dedupe
    let words: Vec<String> = (0..500).map(|i| format!("{tag}{i}")).collect();
    let r = put_row(
        kernel,
        a,
        json!({"role": "user", "parent": c,
               "content": format!("[reply to {pid} · notes read · \"{tag}\"]\n{}", words.join(" "))}),
    )
    .await;
    put_row(
        kernel,
        a,
        json!({"role": "assistant", "parent": r, "content": format!("ok {DONE}")}),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fold_summary_that_never_fits_tells_the_asker() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "iq_fsum").await;
    let fb = asker(&kernel, &tmp, &a).await;
    over_budget(&kernel, &a).await;
    let s = script(&a, Vec::new(), Duration::ZERO);
    // every summary the fold asks for is far over its budget
    *s.responder.lock().unwrap() = Some(Box::new(|msgs: &[Value]| {
        msgs[0]["content"]
            .as_str()
            .is_some_and(|c| c.starts_with("Summarize the conversation excerpt"))
            .then(|| format!("TITLE\n\n{}", "far too long ".repeat(4000)))
    }));
    start(&kernel, &peer_of(&a)).await;
    told(&fb, &a, "fold: the summary of rows").await;
}

/// The multisubai case: two askers inquire of one solver whose turns time out —
/// the second inquiry is held behind the first — and each asker is told.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_asker_of_a_solver_whose_turns_fail_is_told() {
    let _guard = SEND_TIMEOUT_ENV_LOCK.lock().await;
    let _ceiling = Ceiling::set("0.5");
    let tmp = TempDir::new().unwrap();
    let (kernel, solver) = mk_kernel(&tmp, "iq_multi").await;
    let mut askers = Vec::new();
    for n in 1..=2 {
        let id = format!("{solver}_asker{n}");
        let ff = format!("ff_{id}");
        add_file_bridge(&kernel, &ff, &tmp.path().join(".fantastic").join(&id)).await;
        askers.push(add_backend(&kernel, &id, &ff).await);
    }
    let peers: Vec<&AgentId> = askers.iter().collect();
    wire(&kernel, &solver, &peers).await;
    for a in &askers {
        wire(&kernel, a, &[&solver]).await;
    }
    script(
        &solver,
        vec![pass("never arrives"), pass("never arrives")],
        Duration::from_secs(3),
    );
    let scripts: Vec<_> = askers
        .iter()
        .map(|a| script(a, asker_passes(&solver), Duration::ZERO))
        .collect();
    for a in &askers {
        start(&kernel, a).await;
    }
    for fb in &scripts {
        told(fb, &solver, "send: timeout after 0.5s").await;
    }
}

/// `holder`'s lines: its parent, `cli`, the store, its file bridge and each of
/// `others` (as [`add_peer`] does for one). Each of `others` writes its own.
async fn wire(kernel: &Arc<Kernel>, holder: &AgentId, others: &[&AgentId]) {
    let all = json!({"call": ["*"]});
    let mut can = json!({"$parent": all, "cli": all, MEMORY: all});
    can[format!("ff_{holder}")] = all.clone();
    for o in others {
        can[o.as_str()] = all.clone();
    }
    let _ = kernel.operator_send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": holder.as_str(), "can": can}),
        )])
        .await;
}
