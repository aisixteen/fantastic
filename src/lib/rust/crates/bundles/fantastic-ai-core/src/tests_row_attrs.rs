//! What a row carries about itself (#618, #619): its `attributes` map: `ts` and
//! `source` on every row; `usage`, `parts` (a delta, a full list on a checkpoint),
//! `totals`, `model`, `provider` and `cost` on the assistant row. Returned whole by
//! `rows` and `history`, never part of a request, whatever keys it holds. The totals
//! are validated against what the provider was sent and what it reported.

use super::*;
use crate::provider::ProviderReport;

const GO: &str = r#"ok <call name="done"></call>"#;

fn passes(n: usize) -> Vec<Vec<ScriptEvent>> {
    (0..n).map(|_| pass(GO)).collect()
}

async fn ask(kernel: &Arc<Kernel>, agent: &AgentId, payload: Value) -> Value {
    kernel
        .send(vec![Item::new(agent.clone(), payload)])
        .await
        .remove(0)
        .result
}

async fn say(kernel: &Arc<Kernel>, agent: &AgentId, text: &str) {
    let r = ask(kernel, agent, json!({"type": "send", "text": text})).await;
    assert!(r.get("error").is_none(), "{r}");
}

async fn rows(kernel: &Arc<Kernel>, agent: &AgentId) -> Vec<Value> {
    let r = ask(kernel, agent, json!({"type": "rows"})).await;
    r["rows"].as_array().cloned().unwrap_or_default()
}

/// The newest assistant row.
async fn last_answer(kernel: &Arc<Kernel>, agent: &AgentId) -> Value {
    rows(kernel, agent)
        .await
        .into_iter()
        .rev()
        .find(|r| r["role"] == "assistant")
        .expect("an assistant row")
}

/// The full parts list of every assistant row that has one, replayed from its deltas:
/// each row's `prev` names the row before it and its digest is the replayed list's.
fn replay(rows: &[Value]) -> Vec<(Value, Vec<Value>)> {
    let mut out: Vec<(Value, Vec<Value>)> = Vec::new();
    for r in rows
        .iter()
        .filter(|r| r["role"] == "assistant" && r["attributes"].get("parts").is_some())
    {
        let parts = &r["attributes"]["parts"];
        let prev = out
            .last()
            .map(|(row, list)| (row["id"].clone(), list.clone()));
        if parts["full"] != json!(true) {
            let (prev_id, _) = prev.clone().expect("a delta needs a row before it");
            assert_eq!(parts["prev"], prev_id, "{parts}");
        }
        let list = crate::row_attrs::apply(&prev.map(|p| p.1).unwrap_or_default(), parts);
        assert_eq!(
            parts["digest"],
            json!(crate::row_attrs::digest(&list)),
            "{parts}"
        );
        out.push((r.clone(), list));
    }
    out
}

fn names(parts: &[Value]) -> Vec<String> {
    parts
        .iter()
        .map(|p| p["part"].as_str().unwrap().to_string())
        .collect()
}

fn part<'a>(parts: &'a [Value], name: &str) -> &'a Value {
    parts
        .iter()
        .find(|p| p["part"] == name)
        .unwrap_or_else(|| panic!("no {name} in {parts:?}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_assistant_row_keeps_usage_and_every_row_a_ts() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "ra_usage").await;
    let s = script(&agent, passes(2), Duration::ZERO);
    *s.report.lock().unwrap() = ProviderReport {
        prompt_tokens: Some(1000),
        completion_tokens: Some(40),
        cache_read: Some(600),
        cache_write: Some(300),
        stop_reason: None,
    };
    say(&kernel, &agent, "go").await;
    let all = rows(&kernel, &agent).await;
    assert!(all.len() >= 2, "{all:?}");
    for r in &all {
        assert!(
            r["attributes"]["ts"].as_f64().is_some_and(|t| t > 0.0),
            "no ts: {r}"
        );
    }
    let answer = last_answer(&kernel, &agent).await;
    assert_eq!(
        answer["attributes"]["usage"],
        json!({"input": 100, "output": 40, "cache_read": 600, "cache_write": 300})
    );
    // `history` returns them too
    let h = ask(&kernel, &agent, json!({"type": "history"})).await;
    let digest = h["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["role"] == "assistant")
        .unwrap();
    assert_eq!(digest["attributes"], answer["attributes"]);
    assert!(h["rows"][0]["attributes"]["ts"].is_number(), "{h}");

    // an endpoint with no cache counts leaves them absent, never 0
    *s.report.lock().unwrap() = ProviderReport {
        prompt_tokens: Some(1000),
        completion_tokens: Some(40),
        ..Default::default()
    };
    say(&kernel, &agent, "again").await;
    let answer = last_answer(&kernel, &agent).await;
    assert_eq!(
        answer["attributes"]["usage"],
        json!({"input": 1000, "output": 40})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_endpoint_that_reports_nothing_leaves_no_usage() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "ra_none").await;
    let _s = script(&agent, passes(1), Duration::ZERO);
    say(&kernel, &agent, "go").await;
    let answer = last_answer(&kernel, &agent).await;
    assert!(answer["attributes"].get("usage").is_none(), "{answer}");
    assert_eq!(
        answer["attributes"]["parts"]["full"],
        json!(true),
        "{answer}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_parts_follow_the_request_and_the_request_is_unchanged() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "ra_parts").await;
    let s = script(&agent, passes(2), Duration::ZERO);
    say(&kernel, &agent, "go").await;
    let first = replay(&rows(&kernel, &agent).await).pop().unwrap().1;
    assert_eq!(
        names(&first),
        ["self_reflect", "verb_docs", "new_input"]
    );
    say(&kernel, &agent, "again").await;
    let second = replay(&rows(&kernel, &agent).await).pop().unwrap().1;
    assert_eq!(
        names(&second),
        [
            "self_reflect",
            "verb_docs",
            "row",
            "row",
            "new_input"
        ]
    );
    let all = rows(&kernel, &agent).await;
    let (user, answer) = (&all[0], &all[1]);
    let held: Vec<&Value> = second.iter().filter(|p| p["part"] == "row").collect();
    assert_eq!(held[0]["ref"], user["id"]);
    assert_eq!(held[0]["form"], "whole");
    assert_eq!(held[1]["ref"], answer["id"]);
    // the same text, the same hash: the first turn's new input is the user row now
    assert_eq!(held[0]["hash"], part(&first, "new_input")["hash"]);
    assert_eq!(held[0]["bytes"], json!(2));
    for p in &second {
        let hash = p["hash"].as_str().unwrap();
        assert!(hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    // THE REQUEST: role and content (and the agent's cache mark), the system block
    // exactly the pieces end to end, the history as the rows hold it
    let sent = s.request_messages(1);
    for m in &sent {
        let keys: Vec<&String> = m.as_object().unwrap().keys().filter(|k| *k != crate::caching::CACHE_MARK).collect();
        assert_eq!(keys, ["role", "content"], "{m}");
    }
    let system = sent[0]["content"].as_str().unwrap();
    let pieces: usize = ["self_reflect", "verb_docs"]
        .iter()
        .map(|n| part(&second, n)["bytes"].as_u64().unwrap() as usize)
        .sum();
    assert_eq!(system.len(), pieces + 2, "one blank line between the pieces");
    assert_eq!(sent[1], json!({"role": "user", "content": "go"}));
    assert_eq!(sent[2]["content"], answer["content"]);
    assert_eq!(sent[3], json!({"role": "user", "content": "again", "cache_mark": true}), "the tail carries the mark");
}

#[tokio::test]
async fn two_identical_builds_give_the_same_parts_and_a_changed_prompt_only_its_own() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "ra_hash").await;
    let state = crate::state::state_for(&agent);
    let build = |prompt: &'static str| {
        let (kernel, agent, state) = (kernel.clone(), agent.clone(), state.clone());
        async move {
            fantastic_kernel::test_support::as_agent(
                agent.clone(),
                crate::assembly::assemble_messages(
                    &agent,
                    &state,
                    "next",
                    &kernel,
                    "c",
                    Some(prompt),
                    crate::caching::Caching::ProviderCache,
                ),
            )
            .await
            .expect("assemble");
            state.request_parts.lock().unwrap().clone()
        }
    };
    let a = build("ROLE-A {howto}").await;
    assert_eq!(a, build("ROLE-A {howto}").await);
    let b = build("ROLE-B {howto}").await;
    assert_eq!(a.len(), b.len());
    assert_ne!(a[0]["hash"], b[0]["hash"], "system_text changed");
    assert_eq!(a[0]["part"], "system_text");
    assert_eq!(a[1..], b[1..], "nothing else changed");
}

/// The prompt FILE's piece is named `prompt_file`, and changing only that file changes
/// only its hash.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_changed_prompt_file_changes_only_the_prompt_file_part() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "ra_file").await;
    let dir = tmp.path().join(".fantastic");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("prompt.md");
    std::fs::write(&path, "FILE-A {howto}").unwrap();
    for (field, v) in [
        ("file_bridge_id", format!("ff_{}", agent.as_str())),
        ("system_prompt_file", "prompt.md".to_string()),
    ] {
        let mut p = json!({"type": "update_agent", "id": agent.as_str()});
        p[field] = json!(v);
        let r = ask(&kernel, &AgentId::from("kernel_state"), p).await;
        assert!(r.get("error").is_none(), "{r}");
    }
    let _s = script(&agent, passes(12), Duration::ZERO);
    say(&kernel, &agent, "go").await;
    let before = replay(&rows(&kernel, &agent).await).pop().unwrap().1;
    assert_eq!(
        names(&before),
        ["prompt_file", "verb_docs", "new_input"],
        "{before:?}"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    std::fs::write(&path, "FILE-B {howto}").unwrap();
    // the watcher marks the file stale; a later turn reads it again
    let mut after = before.clone();
    for _ in 0..10 {
        say(&kernel, &agent, "go").await;
        after = replay(&rows(&kernel, &agent).await).pop().unwrap().1;
        if part(&after, "prompt_file")["hash"] != part(&before, "prompt_file")["hash"] {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_ne!(
        part(&after, "prompt_file")["hash"],
        part(&before, "prompt_file")["hash"],
        "the edited file never reached a request"
    );
    assert_eq!(
        part(&after, "prompt_file")["hash"],
        json!(crate::row_attrs::short_hash("FILE-B ")),
    );
    assert_eq!(part(&after, "verb_docs"), part(&before, "verb_docs"));
    assert!(!names(&after).contains(&"system_text".to_string()));
}

/// The hash is the first 16 hex digits of the text's SHA-256.
#[test]
fn the_hash_is_the_first_sixteen_hex_digits_of_the_sha256() {
    // SHA-256("abc") = ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
    assert_eq!(crate::row_attrs::short_hash("abc"), "ba7816bf8f01cfea");
    // SHA-256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
    assert_eq!(crate::row_attrs::short_hash(""), "e3b0c44298fc1c14");
}

// ---- the delta, the checkpoints, the totals ----

/// A request's parts as `build` takes them: `{part, ref?, form?, bytes, hash}`.
fn entry(part: &str, id: Option<i64>, form: Option<&str>, text: &str) -> Value {
    let mut e = json!({"part": part});
    if let Some(id) = id {
        e["ref"] = json!(id);
    }
    if let Some(f) = form {
        e["form"] = json!(f);
    }
    e["bytes"] = json!(text.len());
    e["hash"] = json!(crate::row_attrs::short_hash(text));
    e
}

#[test]
fn the_parts_are_a_delta_with_a_checkpoint_every_fifty_requests() {
    use crate::row_attrs::{build, CHECKPOINT_EVERY};
    assert_eq!(CHECKPOINT_EVERY, 50);
    let mut prev = None;
    let mut grown: Vec<Value> = vec![entry("system_text", None, None, "sys")];
    for i in 0..120i64 {
        // each request adds one row and the new input changes
        grown.push(entry("row", Some(i), Some("whole"), "a row"));
        let mut full = grown.clone();
        full.push(entry("new_input", None, None, &format!("input {i}")));
        let before = prev.clone();
        let (parts, _, next) = build(prev.as_ref(), None, 1000 + i, full.clone());
        let checkpoint = i % 50 == 0;
        assert_eq!(
            parts["full"] == json!(true),
            checkpoint,
            "request {i}: {parts}"
        );
        if !checkpoint {
            assert_eq!(parts["prev"], json!(999 + i));
            // the delta is small: one row added, the new input swapped
            assert_eq!(parts["add"].as_array().unwrap().len(), 1, "{parts}");
            let before: crate::row_attrs::PrevRequest = before.unwrap();
            assert_eq!(crate::row_attrs::apply(&before.list, &parts), full);
        }
        assert_eq!(parts["digest"], json!(crate::row_attrs::digest(&full)));
        prev = Some(next);
    }
}

#[test]
fn the_totals_count_bytes_per_part_and_the_folds_recalls_and_refolds() {
    use crate::row_attrs::build;
    let mk = |form: &str, input: &str| {
        vec![
            entry("system_text", None, None, "sys"),
            entry("row", Some(7), Some(form), "0123456789"),
            entry("new_input", None, None, input),
        ]
    };
    let (_, t1, p1) = build(None, None, 1, mk("whole", "ab"));
    assert_eq!(t1["requests"], 1);
    assert_eq!(
        t1["bytes"],
        json!({"system_text": 3, "row": 10, "new_input": 2})
    );
    assert_eq!(
        [&t1["folds"], &t1["recalls"], &t1["refolds"]],
        [&json!(0), &json!(0), &json!(0)]
    );
    // whole -> anchor: a fold
    let (_, t2, p2) = build(Some(&p1), None, 2, mk("anchor", "abc"));
    assert_eq!(
        t2["bytes"],
        json!({"system_text": 6, "row": 20, "new_input": 5})
    );
    assert_eq!(
        (
            t2["folds"].clone(),
            t2["recalls"].clone(),
            t2["refolds"].clone()
        ),
        (json!(1), json!(0), json!(0))
    );
    // anchor -> cut: a refold
    let (_, t3, p3) = build(Some(&p2), None, 3, mk("cut", "abc"));
    assert_eq!(
        (
            t3["folds"].clone(),
            t3["recalls"].clone(),
            t3["refolds"].clone()
        ),
        (json!(1), json!(0), json!(1))
    );
    // cut -> whole: a recall
    let (_, t4, _) = build(Some(&p3), None, 4, mk("whole", "abc"));
    assert_eq!(
        (
            t4["folds"].clone(),
            t4["recalls"].clone(),
            t4["refolds"].clone()
        ),
        (json!(1), json!(1), json!(1))
    );
    assert_eq!(t4["requests"], 4);
    // a restart keeps counting from the totals of the newest stored row
    let (_, t5, _) = build(None, Some(t4.clone()), 5, mk("whole", "abc"));
    assert_eq!(t5["requests"], 5);
    assert_eq!(t5["bytes"]["system_text"], json!(15));
}

#[test]
fn a_pass_is_priced_from_the_serving_providers_prices_and_never_as_zero() {
    let meta = |v: Value| v.as_object().unwrap().clone();
    let priced = meta(
        json!({"prices": {"m-a": {"input": 3.0, "output": 15.0, "cache_read": 0.3, "cache_write_5m": 3.75}}}),
    );
    let usage = json!({"input": 200, "output": 50, "cache_read": 600, "cache_write": 200});
    let c = crate::row_attrs::cost(Some(&usage), &priced, "m-a").unwrap();
    assert!((c["usd"].as_f64().unwrap() - 0.00228).abs() < 1e-12, "{c}");
    assert_eq!(c["prices"]["input"], json!(3.0));
    // a dated form of the id takes the entry of the id
    assert!(
        crate::row_attrs::cost(Some(&usage), &priced, "m-a-20261111").unwrap()["usd"].is_number()
    );
    // no entry for the model, or a cache count with no cache price: unpriced
    assert_eq!(
        crate::row_attrs::cost(Some(&usage), &priced, "m-b").unwrap(),
        json!({"unpriced": true})
    );
    let no_cache = meta(json!({"prices": {"m-a": {"input": 3.0, "output": 15.0}}}));
    assert_eq!(
        crate::row_attrs::cost(Some(&usage), &no_cache, "m-a").unwrap(),
        json!({"unpriced": true})
    );
    // the same pass without cache counts is priced
    let plain = json!({"input": 200, "output": 50});
    assert!(crate::row_attrs::cost(Some(&plain), &no_cache, "m-a").unwrap()["usd"].is_number());
    // no usage reported: no cost either
    assert!(crate::row_attrs::cost(None, &priced, "m-a").is_none());
}

#[tokio::test]
async fn an_oversized_attributes_map_is_refused_at_write_but_a_checkpoint_is_not() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "ra_cap").await;
    let write = |attrs: Value| {
        let (kernel, agent) = (kernel.clone(), agent.clone());
        async move {
            fantastic_kernel::test_support::as_agent(
                agent.clone(),
                crate::memory::save_history(
                    &agent,
                    &kernel,
                    &[json!({"role": "user", "content": "x", "attributes": attrs})],
                ),
            )
            .await
        }
    };
    let e = write(json!({"any_future_key": "y".repeat(9000)}))
        .await
        .unwrap_err();
    assert!(
        e.0.contains("attributes") && e.0.contains("ra_cap") && e.0.contains("8192"),
        "{e}"
    );
    assert!(
        !e.0.contains(&"y".repeat(50)),
        "the value is never in the error"
    );
    // the cap is on the map without its parts: a long chat's checkpoint is kept
    let list: Vec<Value> = (0..400)
        .map(|i| entry("row", Some(i), Some("whole"), "text"))
        .collect();
    write(json!({"any_future_key": "small", "parts": {"full": true, "list": list}}))
        .await
        .expect("a checkpoint is not capped");
}

// ---- the total validation: a scripted run, every figure checked two ways ----

/// One scripted pass: who serves it, with which model, and what the endpoint reports.
struct Pass {
    provider: &'static str,
    model: &'static str,
    report: ProviderReport,
}

fn pass_of(
    provider: &'static str,
    model: &'static str,
    prompt: i64,
    completion: i64,
    cache_read: Option<i64>,
    cache_write: Option<i64>,
) -> Pass {
    Pass {
        provider,
        model,
        report: ProviderReport {
            prompt_tokens: Some(prompt),
            completion_tokens: Some(completion),
            cache_read,
            cache_write,
            stop_reason: None,
        },
    }
}

async fn provider_agent(kernel: &Arc<Kernel>, id: &str, prices: Value) {
    let r = ask(
        kernel,
        &AgentId::from("kernel_state"),
        json!({"type": "create_agent", "parent_id": "kernel", "handler_module": HANDLER_MODULE,
               "id": id, "prices": prices}),
    )
    .await;
    assert!(r.get("error").is_none(), "{r}");
}

/// Sums a usage field over rows.
fn sum(rows: &[&Value], field: &str) -> i64 {
    rows.iter()
        .map(|r| r["attributes"]["usage"][field].as_i64().unwrap_or(0))
        .sum()
}

fn usd(rows: &[&Value]) -> f64 {
    rows.iter()
        .filter_map(|r| r["attributes"]["cost"]["usd"].as_f64())
        .sum()
}

fn unpriced(rows: &[&Value]) -> usize {
    rows.iter()
        .filter(|r| r["attributes"]["cost"]["unpriced"] == json!(true))
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_totals_of_a_scripted_run_validate_against_the_request_and_the_provider() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "ra_total").await;
    provider_agent(
        &kernel,
        "prov_a",
        json!({"m-one": {"input": 3.0, "output": 15.0, "cache_read": 0.3, "cache_write_5m": 3.75},
               "m-two": {"input": 1.0, "output": 5.0, "cache_read": 0.1, "cache_write_5m": 1.25}}),
    )
    .await;
    provider_agent(
        &kernel,
        "prov_b",
        json!({"m-three": {"input": 2.0, "output": 10.0}}),
    )
    .await;
    let s = script(&agent, passes(6), Duration::ZERO);
    // two providers, three models; cache figures scripted per pass (absent, zero, set)
    let run = [
        pass_of("prov_a", "m-one", 1000, 50, Some(600), Some(200)),
        pass_of("prov_a", "m-one", 1500, 80, Some(1000), Some(0)),
        pass_of("prov_a", "m-two", 800, 20, None, None),
        pass_of("prov_b", "m-three", 700, 30, None, None),
        // a cache count the provider has no cache price for: unpriced, never 0
        pass_of("prov_b", "m-three", 900, 40, Some(300), None),
        pass_of("prov_a", "m-one", 1200, 60, Some(700), Some(100)),
    ];
    for (i, p) in run.iter().enumerate() {
        let r = ask(
            &kernel,
            &AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": agent.as_str(), "provider_id": p.provider}),
        )
        .await;
        assert!(r.get("error").is_none(), "{r}");
        *s.report.lock().unwrap() = p.report.clone();
        *s.model.lock().unwrap() = p.model.to_string();
        say(&kernel, &agent, &format!("turn {i}")).await;
    }
    let all = rows(&kernel, &agent).await;
    let answers: Vec<&Value> = all.iter().filter(|r| r["role"] == "assistant").collect();
    assert_eq!(answers.len(), 6);

    // 1. THE PARTS vs THE REQUEST THE PROVIDER GOT: replayed from the deltas, the
    //    bytes of the parts (and the blank lines between the system pieces) are the
    //    bytes of the messages, request by request
    let lists = replay(&all);
    assert_eq!(lists.len(), 6);
    for (i, (_, list)) in lists.iter().enumerate() {
        let sent: usize = s
            .request_messages(i)
            .iter()
            .map(|m| m["content"].as_str().unwrap().len())
            .sum();
        let parts: u64 = list.iter().map(|p| p["bytes"].as_u64().unwrap()).sum();
        // the blank line between two system pieces belongs to neither
        let system = list
            .iter()
            .filter(|p| p["part"] != "row" && p["part"] != "new_input")
            .count();
        assert_eq!(parts as usize + 2 * (system - 1), sent, "request {i}");
        assert_eq!(list.last().unwrap()["part"], "new_input");
    }
    assert_eq!(answers[0]["attributes"]["parts"]["full"], json!(true));
    assert!(answers[1..]
        .iter()
        .all(|r| r["attributes"]["parts"].get("full").is_none()));

    // 2. THE RUNNING TOTALS: each row's are the row before's plus its own request
    let mut bytes: std::collections::BTreeMap<String, u64> = Default::default();
    for (i, (row, list)) in lists.iter().enumerate() {
        for p in list {
            *bytes
                .entry(p["part"].as_str().unwrap().to_string())
                .or_default() += p["bytes"].as_u64().unwrap();
        }
        let t = &row["attributes"]["totals"];
        assert_eq!(t["requests"], json!(i + 1));
        assert_eq!(t["bytes"], json!(bytes), "row {i}");
        assert_eq!(
            [&t["folds"], &t["recalls"], &t["refolds"]],
            [&json!(0), &json!(0), &json!(0)]
        );
    }

    // 3. WHAT SERVED EACH ROW, and what it reported
    for (a, p) in answers.iter().zip(&run) {
        assert_eq!(a["attributes"]["provider"], p.provider);
        assert_eq!(a["attributes"]["model"], p.model);
        assert_eq!(a["attributes"]["source"], agent.as_str());
    }
    // a harness notice is the agent's own text: its source is the agent
    for r in all.iter().filter(|r| r["role"] == "user" && r["kind"] != "notice") {
        assert_eq!(r["attributes"]["source"], "cli", "{r}");
    }
    assert_eq!(
        answers[1]["attributes"]["usage"],
        json!({"input": 500, "output": 80, "cache_read": 1000, "cache_write": 0})
    );
    assert_eq!(
        answers[2]["attributes"]["usage"],
        json!({"input": 800, "output": 20})
    );

    // 4. THE TOTAL, and the total grouped by model, by provider, by both: every
    //    group's figures add up to the whole, and to the figures scripted
    let by = |key: &str, value: &str| -> Vec<&Value> {
        answers
            .iter()
            .copied()
            .filter(|r| r["attributes"][key] == value)
            .collect()
    };
    let near = |got: f64, want: f64| assert!((got - want).abs() < 1e-12, "{got} vs {want}");
    for field in ["input", "output", "cache_read", "cache_write"] {
        let total = sum(&answers, field);
        let by_model: i64 = ["m-one", "m-two", "m-three"]
            .iter()
            .map(|m| sum(&by("model", m), field))
            .sum();
        let by_provider: i64 = ["prov_a", "prov_b"]
            .iter()
            .map(|p| sum(&by("provider", p), field))
            .sum();
        assert_eq!((by_model, by_provider), (total, total), "{field}");
    }
    assert_eq!(sum(&answers, "input"), 200 + 500 + 800 + 700 + 600 + 400);
    assert_eq!(sum(&answers, "output"), 50 + 80 + 20 + 30 + 40 + 60);
    assert_eq!(sum(&answers, "cache_read"), 600 + 1000 + 300 + 700);
    assert_eq!(sum(&answers, "cache_write"), 200 + 100);
    assert_eq!(sum(&by("model", "m-one"), "input"), 1100);
    near(usd(&by("model", "m-one")), 0.00228 + 0.003 + 0.002685);
    near(usd(&by("model", "m-two")), 0.0009);
    near(usd(&by("model", "m-three")), 0.0017);
    assert_eq!(unpriced(&by("model", "m-three")), 1);
    near(
        usd(&by("provider", "prov_a")),
        0.00228 + 0.003 + 0.0009 + 0.002685,
    );
    near(usd(&by("provider", "prov_b")), 0.0017);
    assert_eq!(
        (
            unpriced(&by("provider", "prov_a")),
            unpriced(&by("provider", "prov_b"))
        ),
        (0, 1)
    );
    // the unpriced pass is not 0: it carries no usd at all
    assert!(answers[4]["attributes"]["cost"].get("usd").is_none());
    assert_eq!(
        answers[0]["attributes"]["cost"]["prices"]["cache_write_5m"],
        json!(3.75)
    );

    // 5. `history` and `rows` return it whole; the request holds none of it
    let h = ask(&kernel, &agent, json!({"type": "history"})).await;
    let digests: Vec<&Value> = h["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["role"] == "assistant")
        .collect();
    for (d, a) in digests.iter().zip(&answers) {
        assert_eq!(d["attributes"], a["attributes"]);
    }
    for i in 0..6 {
        let body = Value::Array(s.request_messages(i)).to_string();
        for key in ["attributes", "totals", "m-one", "digest"] {
            assert!(!body.contains(key), "request {i} holds {key}");
        }
        // The tree primer names the provider agent, so the bare id `prov_a` may
        // appear; only the ATTRIBUTES carry it as a value of `provider`, and
        // only they carry the usage and cost keys or their exact serialization.
        for frag in ["\"provider\":\"prov_a\"", "\"usage\"", "\"cost\"", "\"model\":"] {
            assert!(!body.contains(frag), "request {i} holds the attribute fragment {frag}");
        }
        for a in &answers {
            let ser = a["attributes"].to_string();
            assert!(!body.contains(&ser), "request {i} holds a row's attributes");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_and_a_compaction_each_start_a_checkpoint_and_the_totals_go_on() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "ra_ckpt").await;
    let _s = script(&agent, passes(5), Duration::ZERO);
    let state = crate::state::state_for(&agent);
    let full_of = |r: &Value| r["attributes"]["parts"]["full"] == json!(true);
    say(&kernel, &agent, "one").await;
    say(&kernel, &agent, "two").await;
    let before = rows(&kernel, &agent).await;
    let answers: Vec<&Value> = before.iter().filter(|r| r["role"] == "assistant").collect();
    assert!(full_of(answers[0]) && !full_of(answers[1]));

    // A RESTART: the chat holds nothing of its previous request in RAM
    *state.prev_request.lock().unwrap() = None;
    say(&kernel, &agent, "three").await;
    let third = last_answer(&kernel, &agent).await;
    assert!(full_of(&third), "{third}");
    assert_eq!(
        third["attributes"]["totals"]["requests"], 3,
        "the count goes on after a restart"
    );
    say(&kernel, &agent, "four").await;
    assert!(!full_of(&last_answer(&kernel, &agent).await));

    // A COMPACTION (the projection that fires on a fat view) clears it too
    let provider: Arc<dyn Provider> = Arc::new(SummaryProvider("SUMMARY"));
    let mut fat = vec![turn("system", "sys")];
    fat.extend(body(60, 8000));
    fantastic_kernel::test_support::as_agent(
        agent.clone(),
        crate::projection::project_context(
            &provider,
            &agent,
            &state,
            &kernel,
            "probe",
            "mock_backend",
            crate::constants::DEFAULT_CONTEXT_WINDOW,
            fat,
            4,
        ),
    )
    .await
    .expect("the projection fires");
    assert!(state.prev_request.lock().unwrap().is_none());
    say(&kernel, &agent, "five").await;
    let fifth = last_answer(&kernel, &agent).await;
    assert!(full_of(&fifth), "{fifth}");
    assert_eq!(fifth["attributes"]["totals"]["requests"], 5);
}
