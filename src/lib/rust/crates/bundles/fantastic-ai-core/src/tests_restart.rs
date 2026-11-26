//! #726: a chain in flight across a kernel restart. Everything is records: the
//! first kernel dies with a chain mid-flight (the lead's call to a slow NON-AI peer out
//! (a send to an AI is acked at once and never cut, #795), the
//! lead's pass over an asker's message held at its request); a second kernel boots on
//! what the first one flushed. The calls that died are dead letters, every agent they concern
//! is told once at its boot, and the lead's chain goes on from there.

use super::*;

const DONE: &str = "<call name=\"done\"></call>";
const RECALL: &str = "You are rebuilding your own working context";

/// A call to `target`, its reply back to self.
fn send_call(target: &str, text: &str, ms: Option<u64>) -> String {
    let ms = ms.map(|m| format!("<ms>{m}</ms>")).unwrap_or_default();
    format!(
        "<call name=\"send\"><routing><target_id>{target}</target_id><reply_to>self</reply_to></routing><payload><type>send</type><text>{text}</text>{ms}</payload></call>"
    )
}

/// The ids of one world: the lead, its slow peer, the asker.
struct Ids {
    lead: AgentId,
    slow: AgentId,
    asker: AgentId,
}

/// One kernel on `tmp`: the lead, `slow` and `asker`, each with its leg, each may
/// send to the others, the root naming the memory store. Not started. `next_id`:
/// where the id counter goes on (the root record's bound, in a real reboot).
async fn world(tmp: &TempDir, next_id: Option<i64>) -> (Arc<Kernel>, Ids) {
    let kernel = base_kernel(tmp, |k| k.bundles.register("sleeper.tools", Sleeper)).await;
    if let Some(n) = next_id {
        let mut meta = Map::new();
        meta.insert("next_id".to_string(), json!(n));
        kernel.restore_counter(&meta);
    }
    let lead = id_for(tmp, "rs");
    let ids = Ids {
        slow: AgentId::from(format!("{lead}_slow")),
        asker: AgentId::from(format!("{lead}_asker")),
        lead: AgentId::from(lead),
    };
    let all = [ids.lead.clone(), ids.slow.clone(), ids.asker.clone()];
    // the slow peer is the slow non-AI stand-in
    let r = kernel
        .operator_send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "create_agent", "parent_id": "kernel", "handler_module": "sleeper.tools", "id": ids.slow.as_str()}),
        )])
        .await
        .remove(0)
        .result;
    assert!(r.get("error").is_none(), "{r}");
    for a in all.iter().filter(|a| **a != ids.slow) {
        let leg = format!("ff_{a}");
        add_file_bridge(
            &kernel,
            &leg,
            &tmp.path().join(".fantastic").join(a.as_str()),
    )
        .await;
        add_backend(&kernel, a.as_str(), &leg).await;
    }
    for a in all.iter().filter(|a| **a != ids.slow) {
        let line = json!({"call": ["*"]});
        let mut can = json!({"$parent": line, "cli": line, MEMORY: line});
        can[format!("ff_{a}")] = line.clone();
        for o in all.iter().filter(|o| *o != a) {
            can[o.as_str()] = line.clone();
        }
        let r = kernel.operator_send(vec![Item::new(
                AgentId::from("kernel_state"),
                json!({"type": "update_agent", "id": a.as_str(), "can": can}),
            )])
            .await
            .remove(0)
            .result;
        assert!(r.get("error").is_none(), "{r}");
    }
    let mut patch = Map::new();
    patch.insert("memory_store_id".to_string(), json!(MEMORY));
    fantastic_kernel::test_support::update(&kernel, &kernel.root().unwrap().id, patch);
    (kernel, ids)
}

/// The text of the last message of a request.
fn last(msgs: &[Value]) -> String {
    msgs.last()
        .and_then(|m| m["content"].as_str())
        .unwrap_or("")
        .to_string()
}

/// The first request whose last message holds `needle`, waited for.
async fn asked(s: &Arc<MockScript>, needle: &str) -> String {
    for _ in 0..500 {
        let hit = s
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|m| last(m))
            .find(|t| t.contains(needle));
        if let Some(t) = hit {
            return t;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "no request carried {needle:?}: {:?}",
        s.requests
            .lock()
            .unwrap()
            .iter()
            .map(|m| last(m))
            .collect::<Vec<_>>()
    );
}

/// The first life: the lead's call to `slow` is out (slow is mid-pass), and the
/// asker's message is stored at the lead (acked at once, #795), its pass held at the
/// request. Returns what the store holds then,
/// and where the id counter goes on.
async fn first_life(tmp: &TempDir) -> (Value, i64) {
    let (kernel, ids) = world(tmp, None).await;
    assert_eq!(kernel.start(false).await["started"], json!(true));
    // the lead's call to the slow peer (an hour of sleep) outlives this kernel
    let sl = script(
        &ids.lead,
        vec![pass(send_call(ids.slow.as_str(), "work", Some(3_600_000)))],
        Duration::ZERO,
    );
    let _sa = script(
        &ids.asker,
        vec![pass(send_call(ids.lead.as_str(), "question", None))],
        Duration::ZERO,
    );
    // no settle: the lead's call stays open
    let r = kernel.operator_send(vec![Item::new(
            ids.lead.clone(),
            json!({"type": "send", "text": "start the work", "client_id": "o"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(r.get("error").is_none(), "{r}");
    until_true("the lead's call to be out", || {
        crate::state::state_for(&ids.lead).life.lock().unwrap().open > 0
    })
    .await;
    // the lead's next request is held: the asker's message wakes a pass that never ends
    let _held = sl.hold_next();
    let r = kernel.operator_send(vec![Item::new(
            ids.asker.clone(),
            json!({"type": "send", "text": "ask the lead", "client_id": "o"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(r.get("error").is_none(), "{r}");
    assert!(
        until_passes(&sl, 2).await,
        "the asker's message woke no pass of the lead"
    );
    // what reached the store is all the next life has
    kernel.flush_executions().await;
    let tables = kernel.operator_send(vec![Item::new(
            AgentId::from(MEMORY),
            json!({"type": "dump"}),
        )])
        .await
        .remove(0)
        .result["tables"]
        .clone();
    (tables, kernel.mint_id() + 1)
}

#[test]
fn a_chain_cut_by_a_restart_is_told_as_dead_letters_and_goes_on() {
    let tmp = TempDir::new().unwrap();
    // THE FIRST PROCESS: its runtime is dropped with the chain in flight, so none of
    // its tasks runs again and no hook runs (a kill, not a stop).
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let (tables, next_id) = rt.block_on(first_life(&tmp));
    drop(rt);
    // ...and its RAM is gone with it.
    let lead_id = AgentId::from(id_for(&tmp, "rs"));
    for a in [
        lead_id.clone(),
        AgentId::from(format!("{lead_id}_slow")),
        AgentId::from(format!("{lead_id}_asker")),
    ] {
        crate::state::drop_state(&a);
        crate::route::forget(a.as_str());
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        // THE SECOND PROCESS, on the same records
        let (kernel, ids) = world(&tmp, Some(next_id)).await;
        let r = kernel.operator_send(vec![Item::new(
                AgentId::from(MEMORY),
                json!({"type": "inject", "tables": tables}),
            )])
            .await
            .remove(0)
            .result;
        assert!(r.get("error").is_none(), "{r}");
        let answer = |on_notice: String| {
            move |msgs: &[Value]| -> Option<String> {
                let t = last(msgs);
                if t.trim_start().starts_with(RECALL) {
                    Some(r#"{"expand": [], "why": "fine"}"#.to_string())
                } else if t.contains("[restart]") {
                    Some(on_notice.clone())
                } else {
                    Some(format!("ANSWERED {DONE}"))
                }
            }
        };
        let sl = script(&ids.lead, Vec::new(), Duration::ZERO);
        let sa = script(&ids.asker, Vec::new(), Duration::ZERO);
        // the lead resends what it still needs; the others are done
        *sl.responder.lock().unwrap() = Some(Box::new({
            let resend = send_call(ids.slow.as_str(), "work again", Some(10));
            let on = answer(resend);
            move |msgs: &[Value]| {
                if last(msgs).contains("ANSWERED") {
                    Some(DONE.to_string())
                } else {
                    on(msgs)
                }
            }
        }));
        *sa.responder.lock().unwrap() = Some(Box::new(answer(DONE.to_string())));
        assert_eq!(kernel.start(true).await["started"], json!(true));

        // THE LEDGER: the call that died is a dead letter, nothing is in flight; the
        // asker's message to the lead was acked at once, so it is no dead letter
        let entries = kernel.operator_send(vec![Item::new(
                AgentId::from("kernel_state"),
                json!({"type": "ledger", "limit": 1000}),
            )])
            .await
            .remove(0)
            .result["executions"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let cut: Vec<&Value> = entries
            .iter()
            .filter(|e| e["status"] == json!("cancelled"))
            .collect();
        let work = cut
            .iter()
            .find(|e| e["target"] == json!(ids.slow.as_str()))
            .and_then(|e| e["id"].as_i64())
            .unwrap_or_else(|| panic!("a cut call to {}: {entries:?}", ids.slow));
        assert!(
            !cut.iter().any(|e| e["target"] == json!(ids.lead.as_str())),
            "the asker's message was acked, never cut: {cut:?}"
        );
        assert!(
            cut.iter().all(|e| e["outcome"]["error"]
                == json!(fantastic_kernel::executions::CUT_BY_RESTART)),
            "{cut:?}"
        );

        // THE LEAD is told its reply will not come
        let told = asked(&sl, "[restart]").await;
        assert!(
            told.contains(&format!(
                "[reply to {work} \u{b7} {} send] no reply will come \u{2192} dead letter {work}; resend or drop",
                ids.slow
            )),
            "{told}"
        );
        // THE ASKER's message is not lost: stored and acked, the boot's pass reads it
        until_true("the lead to read the asker's message", || {
            sl.requests
                .lock()
                .unwrap()
                .iter()
                .any(|m| Value::Array(m.clone()).to_string().contains("question"))
        })
        .await;

        // THE CHAIN GOES ON: the lead's resend is answered and reaches it
        let back = asked(&sl, "slept").await;
        assert!(back.contains(&format!("{} send", ids.slow)), "{back}");
        // nothing of the asker was cut, so nothing tells it
        assert!(settle(&kernel).await, "the agents never went quiet");
        assert_eq!(sa.passes_served(), 0, "the asker is told nothing");

        // TOLD ONCE: a reboot of the lead tells it nothing again
        tokio::time::sleep(Duration::from_millis(300)).await;
        let before = sl.passes_served();
        let r = kernel.operator_send(vec![Item::new(ids.lead.clone(), json!({"type": "boot"}))])
            .await
            .remove(0);
        assert!(r.result.get("error").is_none(), "{}", r.result);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(sl.passes_served(), before, "no second notice");
    });
}
