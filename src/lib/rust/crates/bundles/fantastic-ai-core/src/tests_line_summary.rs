//! THE LINE SUMMARY (#709) over the harness: the model's `line_summary` call is parsed,
//! trimmed to one capped line, kept as the chat's state, shown by `reflect`, published
//! on the chat's inbox and read back from the history at boot; offered only when the
//! record sets `line_summaries`; and a stretch's latest line is its fold's title.

use super::*;
use crate::constants::MAX_TITLE;
use crate::line_summary::{clean, last_in, of_span, EVENT, FIELD, HOWTO};
use fantastic_kernel::test_support::as_agent;

const DONE: &str = "<call name=\"done\"></call>";

fn line(text: &str) -> String {
    format!("<call name=\"line_summary\"><text>{text}</text></call>")
}

/// A read of `notes`, in the routing form.
fn read_call() -> String {
    "<call name=\"send\"><routing><target_id>notes</target_id></routing><payload><type>read</type><path>a.md</path></payload></call>".to_string()
}

async fn say(kernel: &Arc<Kernel>, agent: &AgentId, text: &str) {
    let _ = kernel
        .send(vec![Item::new(
            agent.clone(),
            json!({"type": "send", "text": text, "client_id": "probe"}),
        )])
        .await;
}

async fn verb(kernel: &Arc<Kernel>, agent: &AgentId, payload: Value) -> Value {
    let out = kernel
        .send(vec![Item::new(agent.clone(), payload)])
        .await
        .remove(0);
    assert_eq!(out.status.as_str(), "completed", "{:?}", out.result);
    out.result
}

async fn set(kernel: &Arc<Kernel>, agent: &AgentId, fields: Value) {
    let mut p = json!({"type": "update_agent", "id": agent.as_str()});
    for (k, v) in fields.as_object().unwrap() {
        p[k] = v.clone();
    }
    let r = verb(kernel, &AgentId::from("kernel_state"), p).await;
    assert!(r.get("error").is_none(), "{r}");
}

async fn offer(kernel: &Arc<Kernel>, agent: &AgentId) {
    set(kernel, agent, json!({FIELD: true})).await;
}

/// Every row of the agent in its store.
async fn store(kernel: &Arc<Kernel>, agent: &AgentId) -> Vec<Value> {
    let res = as_agent(
        agent.clone(),
        crate::memory::store_call(agent, kernel, json!({"type": "read"})),
    )
    .await
    .expect("store read");
    res["rows"].as_array().cloned().unwrap_or_default()
}

/// The `line_summary_changed` events among `evs`.
fn lines_published(evs: &[Value]) -> Vec<Value> {
    evs.iter().filter(|e| e["type"] == EVENT).cloned().collect()
}

// ── the call ─────────────────────────────────────────────────────────────────

#[test]
fn the_call_parses_in_our_grammar_and_its_line_is_trimmed_to_one_capped_line() {
    let calls = crate::tool_parse::extract_tool_calls(&line("  reading\n   the   spec  "));
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "line_summary");
    assert_eq!(calls[0].arguments["text"], "reading\n   the   spec");
    assert_eq!(
        last_in(&line("  reading\n   the   spec  ")).as_deref(),
        Some("reading the spec")
    );
    // the last line of an answer is its line
    let two = format!("{} words {}", line("first"), line("second"));
    assert_eq!(last_in(&two).as_deref(), Some("second"));
    // capped like a fold title, and cleaned like one: the view and the fold agree
    let long = "w".repeat(MAX_TITLE + 60);
    assert_eq!(clean(&long).unwrap().chars().count(), MAX_TITLE);
    assert_eq!(clean("a \"b\" [c]").as_deref(), Some("a 'b' [c)"));
    assert_eq!(clean(" \n\t "), None, "nothing left: no line");
    assert_eq!(last_in(&line("   ")), None);
    // the call's own name is no verb: a send is still a send
    assert_eq!(last_in(&read_call()), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_line_is_the_chats_state_its_reflect_and_an_event_on_its_inbox() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "ls_state").await;
    offer(&kernel, &agent).await;
    let r = verb(&kernel, &agent, json!({"type": "reflect"})).await;
    assert_eq!(r["line_summary"], Value::Null, "offered, no line yet: {r}");
    let mut rx = rebind_backend_inbox(&kernel, &agent);
    let s = script(
        &agent,
        vec![pass(format!(
            "{}\nworking. {DONE}",
            line("  reading   the spec ")
        ))],
        Duration::ZERO,
    );
    let before = crate::helpers::now_secs();
    say(&kernel, &agent, "go").await;
    assert!(until_passes(&s, 1).await);
    let evs = drain(&mut rx).await;
    let published = lines_published(&evs);
    assert_eq!(published.len(), 1, "one event per new line: {evs:?}");
    assert_eq!(published[0]["text"], "reading the spec");
    assert_eq!(published[0]["source"], agent.as_str());
    let ts = published[0]["ts"].as_f64().expect("a ts");
    assert!(ts >= before, "{ts} < {before}");
    let r = verb(&kernel, &agent, json!({"type": "reflect"})).await;
    assert_eq!(
        r["line_summary"],
        json!({"text": "reading the spec", "ts": ts}),
        "{r}"
    );
    // the line stays in the history like any call
    let stored = store(&kernel, &agent).await;
    assert!(
        stored.iter().any(|x| x["role"] == "assistant"
            && x["content"]
                .as_str()
                .is_some_and(|c| c.contains("<call name=\"line_summary\">"))),
        "{stored:?}"
    );
    assert!(
        !stored
            .iter()
            .any(|x| x.get("kind").and_then(Value::as_str) == Some("line_summary")),
        "no row of its own"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_of_a_line_alone_is_a_turn_with_no_call() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "ls_alone").await;
    offer(&kernel, &agent).await;
    let s = script(
        &agent,
        vec![pass(line("thinking it over")), pass(DONE)],
        Duration::ZERO,
    );
    say(&kernel, &agent, "go").await;
    assert!(until_passes(&s, 2).await);
    assert!(
        s.request(1).contains("[no call was dispatched]"),
        "{}",
        s.request(1)
    );
    let r = verb(&kernel, &agent, json!({"type": "reflect"})).await;
    assert_eq!(r["line_summary"]["text"], "thinking it over", "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn not_offered_the_call_is_refused_reflect_has_no_line_and_the_prompt_no_line() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "ls_off").await;
    let block = crate::assembly::default_system_content(&agent, &kernel).await;
    assert!(!block.contains(HOWTO), "off: the prompt line is absent");
    let s = script(
        &agent,
        vec![pass(line("reading")), pass(DONE)],
        Duration::ZERO,
    );
    let mut rx = rebind_backend_inbox(&kernel, &agent);
    say(&kernel, &agent, "go").await;
    assert!(until_passes(&s, 2).await);
    assert!(
        s.request(1)
            .contains("line_summary: this chat does not offer it"),
        "{}",
        s.request(1)
    );
    let evs = drain(&mut rx).await;
    assert!(lines_published(&evs).is_empty(), "{evs:?}");
    let r = verb(&kernel, &agent, json!({"type": "reflect"})).await;
    assert!(r.get("line_summary").is_none(), "{r}");
    // offered: the one line is in the block
    offer(&kernel, &agent).await;
    let block = crate::assembly::default_system_content(&agent, &kernel).await;
    assert_eq!(block.matches(HOWTO).count(), 1, "on: the one line");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_with_no_line_is_told_so() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "ls_empty").await;
    offer(&kernel, &agent).await;
    let s = script(&agent, vec![pass(line("  ")), pass(DONE)], Duration::ZERO);
    say(&kernel, &agent, "go").await;
    assert!(until_passes(&s, 2).await);
    assert!(
        s.request(1).contains("line_summary: it takes `<text>`"),
        "{}",
        s.request(1)
    );
    let r = verb(&kernel, &agent, json!({"type": "reflect"})).await;
    assert_eq!(r["line_summary"], Value::Null, "{r}");
}

// ── boot ─────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn at_boot_the_chat_restores_its_last_line_from_its_history() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "ls_boot").await;
    offer(&kernel, &agent).await;
    let u = put_row(
        &kernel,
        &agent,
        json!({"role": "user", "parent": null, "content": "go"}),
    )
    .await;
    put_row(&kernel, &agent, json!({"role": "assistant", "parent": u, "content": format!("{} {DONE}", line("the old line"))})).await;
    let newest = put_row(
        &kernel,
        &agent,
        json!({"role": "assistant", "parent": u, "content": format!("{} {DONE}", line("the newest line")),
               "attributes": {"ts": 1234.5}}),
    )
    .await;
    // more answers after it than one page of the boot's read, none with a line
    for k in 0..70 {
        put_row(
            &kernel,
            &agent,
            json!({"role": "assistant", "parent": u, "content": format!("step {k} {DONE}")}),
        )
        .await;
    }
    assert!(newest > u);
    crate::state::drop_state(&agent);
    let mut rx = rebind_backend_inbox(&kernel, &agent);
    let r = verb(&kernel, &agent, json!({"type": "boot"})).await;
    assert!(r.is_null(), "{r}");
    let r = verb(&kernel, &agent, json!({"type": "reflect"})).await;
    assert_eq!(
        r["line_summary"],
        json!({"text": "the newest line", "ts": 1234.5}),
        "{r}"
    );
    let mut evs = Vec::new();
    while let Some(v) = rx.try_recv() {
        evs.push(v.payload);
    }
    assert_eq!(
        lines_published(&evs),
        vec![
            json!({"type": EVENT, "source": agent.as_str(), "text": "the newest line", "ts": 1234.5})
        ],
        "a follower that missed the restart has it"
    );
    // a record that does not offer the call restores nothing
    set(&kernel, &agent, json!({FIELD: false})).await;
    crate::state::drop_state(&agent);
    verb(&kernel, &agent, json!({"type": "boot"})).await;
    assert!(crate::line_summary::current(&agent).is_null());
}

// ── the fold ─────────────────────────────────────────────────────────────────

const SUMMARY: &str = "SUMMARISER TITLE\n\nwhat was read and how it ended.";

/// A closed chain: the task, an answer writing two lines and a read, its reply, done.
/// Returns the answer's row id.
async fn closed(kernel: &Arc<Kernel>, agent: &AgentId, tag: &str) -> i64 {
    let u = put_row(
        kernel,
        agent,
        json!({"role": "user", "parent": null, "content": format!("task {tag}")}),
    )
    .await;
    let a = put_row(
        kernel,
        agent,
        json!({"role": "assistant", "parent": u,
               "content": format!("{}{}{}", line(&format!("first {tag}")), line(&format!("LINE {tag}")), read_call())}),
    )
    .await;
    let pid = kernel.mint_id();
    let words: Vec<String> = (0..500).map(|i| format!("{tag}{i}")).collect();
    let r = put_row(
        kernel,
        agent,
        json!({"role": "user", "parent": a,
               "content": format!("[reply to {pid} · notes read · \"{tag}\"]\n{}", words.join(" "))}),
    )
    .await;
    put_row(
        kernel,
        agent,
        json!({"role": "assistant", "parent": r, "content": format!("ok {DONE}")}),
    )
    .await;
    a
}

/// One projection, as a turn runs it, every summariser ask answered with [`SUMMARY`].
async fn project(kernel: &Arc<Kernel>, agent: &AgentId) -> Arc<MockScript> {
    let s = script(
        agent,
        (0..40).map(|_| pass(SUMMARY)).collect(),
        Duration::ZERO,
    );
    let provider: Arc<dyn Provider> = Arc::new(MockProvider {
        script: Arc::clone(&s),
    });
    let state = crate::state::state_for(agent);
    let (messages, store_len) = as_agent(
        agent.clone(),
        crate::assembly::assemble_messages(
            agent,
            &state,
            "next",
            kernel,
            "cli",
            crate::caching::Caching::ProviderCache,
        ),
    )
    .await
    .expect("assemble");
    as_agent(
        agent.clone(),
        crate::projection::project_context(
            &provider,
            agent,
            &state,
            kernel,
            "cli",
            "mock",
            crate::constants::DEFAULT_CONTEXT_WINDOW,
            messages,
            store_len,
        ),
    )
    .await
    .expect("projection");
    s
}

/// A chat on the fold strategy over a small window: a goal, six closed chains, one
/// projection. Returns each chain's answer row id with its tag, and the summariser log.
async fn folded(
    kernel: &Arc<Kernel>,
    agent: &AgentId,
) -> (Vec<(i64, &'static str)>, Arc<MockScript>) {
    set(
        kernel,
        agent,
        json!({"context_strategy": "fold", "context_window": 4000, "output_reserve": 0}),
    )
    .await;
    put_row(
        kernel,
        agent,
        json!({"role": "user", "parent": null, "content": "GOAL"}),
    )
    .await;
    let mut answers = Vec::new();
    for t in ["a", "b", "c", "d", "e", "f"] {
        answers.push((closed(kernel, agent, t).await, t));
    }
    let s = project(kernel, agent).await;
    (answers, s)
}

fn folds(rows: &[Value]) -> Vec<&Value> {
    rows.iter().filter(|r| r["kind"] == "fold").collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stretchs_latest_line_is_its_folds_title_and_the_summariser_writes_the_body() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "ls_fold").await;
    offer(&kernel, &agent).await;
    let (answers, s) = folded(&kernel, &agent).await;
    let rows = store(&kernel, &agent).await;
    let fs = folds(&rows);
    // a fold over the chains' rows is titled by its stretch's latest line; a fold of
    // folds holds no answer, so no line: the summariser's title
    let mut leaves = 0;
    for f in &fs {
        let refs: Vec<i64> = f["refs"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_i64)
            .collect();
        let Some(last) = answers.iter().filter(|(id, _)| refs.contains(id)).last() else {
            assert_eq!(f["title"], "SUMMARISER TITLE", "{f}");
            continue;
        };
        leaves += 1;
        assert_eq!(f["title"], format!("LINE {}", last.1), "{f}");
        assert!(
            f["content"]
                .as_str()
                .unwrap()
                .contains(&format!("\"LINE {}\"", last.1)),
            "{f}"
        );
        assert!(
            f["body"]
                .as_str()
                .unwrap()
                .starts_with("SUMMARISER TITLE\nwhat was read"),
            "the body is the summariser's whole text: {f}"
        );
    }
    assert!(leaves > 0, "the projection folded the chains: {fs:?}");
    let asks: Vec<String> = (0..s.passes_served()).map(|n| s.request(n)).collect();
    let bodies = asks
        .iter()
        .filter(|a| a.contains("Its title is written already"))
        .count();
    let titled = asks
        .iter()
        .filter(|a| a.contains("First line: a one-sentence title"))
        .count();
    assert_eq!((bodies, titled), (leaves, fs.len() - leaves), "{asks:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn not_offered_a_fold_keeps_the_summarisers_title() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "ls_fold_off").await;
    let (_, s) = folded(&kernel, &agent).await;
    let rows = store(&kernel, &agent).await;
    let fs = folds(&rows);
    assert!(!fs.is_empty(), "the projection folded");
    assert!(
        fs.iter().all(|f| f["title"] == "SUMMARISER TITLE"),
        "{fs:?}"
    );
    assert!(
        (0..s.passes_served()).all(|n| s.request(n).contains("First line: a one-sentence title"))
    );
}

#[tokio::test]
async fn a_stretch_with_no_line_has_none() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "ls_span").await;
    offer(&kernel, &agent).await;
    let rows = vec![
        json!({"role": "user", "id": 1, "content": line("a person's text is no line")}),
        json!({"role": "assistant", "id": 2, "content": format!("ok {DONE}")}),
        json!({"role": "assistant", "id": 3, "content": format!("{} {DONE}", line("mine"))}),
    ];
    assert_eq!(of_span(&agent, &kernel, &rows, &[0, 1]), None);
    assert_eq!(
        of_span(&agent, &kernel, &rows, &[0, 1, 2]).as_deref(),
        Some("mine")
    );
}
