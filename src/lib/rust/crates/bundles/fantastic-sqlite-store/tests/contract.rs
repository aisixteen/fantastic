//! THE SHARED MEMORY-STORE CASES (`contract/memory_store/cases.json`), replayed
//! against both rust stores: every step must answer as the case says, and the two
//! stores' transcripts must be identical. The python stores replay the same file
//! (`bundled_agents/memory/sqlite_store/tests/test_memory_store_cases.py`).

use std::sync::{Arc, Once};

use fantastic_kernel::agent::Agent;
use fantastic_kernel::test_support::as_agent;
use fantastic_kernel::{AgentId, Item, Kernel};
use serde_json::{json, Value};

const RAM: &str = fantastic_ram_store::HANDLER_MODULE;
const SQL: &str = fantastic_sqlite_store::HANDLER_MODULE;
const CASES: &str = include_str!("../../../../../../../contract/memory_store/cases.json");

/// `path` is relative to the kernel's workdir: one temp dir for the binary.
fn work() -> std::path::PathBuf {
    static ONCE: Once = Once::new();
    let d = std::env::temp_dir().join(format!("fc-sqlite-contract-{}", std::process::id()));
    ONCE.call_once(|| {
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
    });
    d
}

fn agent(id: &str, hm: Option<&str>, meta: Value) -> Arc<Agent> {
    let parent = (id != "kernel_state").then(|| AgentId::from("kernel_state"));
    Agent::new(
        AgentId::from(id),
        hm.map(str::to_string),
        parent,
        meta.as_object().cloned().unwrap_or_default(),
        std::path::PathBuf::new(),
        false,
    )
}

/// The cases' fixture: guid `g1`, `alice` and `bob` granted onto the store `mem`.
fn store(handler: &str, tag: &str) -> Arc<Kernel> {
    let dir = work();
    let mut k = Kernel::with_storage(fantastic_kernel::StorageMode::Disk {
        state_dir: dir.clone().join(".fantastic"),
        work_dir: dir.clone(),
    });
    k.bundles
        .register(RAM, fantastic_ram_store::RamStoreBundle::default());
    k.bundles
        .register(SQL, fantastic_sqlite_store::SqliteStoreBundle::default());
    let k = Arc::new(k);
    let root = agent("kernel_state", None, json!({"guid": "g1"}));
    k.register(Arc::clone(&root));
    k.set_root(root);
    for a in ["alice", "bob"] {
        k.register(agent(a, None, json!({"can_call": {"mem": "*"}})));
    }
    let mut meta = json!({"callable_by": {"alice": "*", "bob": "*"}});
    if handler == SQL {
        let path = format!("stores/{tag}.db");
        let _ = std::fs::remove_file(dir.join(&path));
        meta["path"] = json!(path);
    }
    k.register(agent("mem", Some(handler), meta));
    k
}

async fn replay(handler: &str, scenario: &Value) -> Vec<Value> {
    let name = scenario["name"].as_str().unwrap();
    let k = store(handler, name);
    let mut out = Vec::new();
    for (n, step) in scenario["steps"].as_array().unwrap().iter().enumerate() {
        let item = Item::new(AgentId::from("mem"), step["send"].clone());
        let reply = match step["as"].as_str() {
            None => k.send(vec![item]).await.remove(0).result,
            Some(who) => as_agent(AgentId::from(who), k.send(vec![item])).await.remove(0).result,
        };
        for (key, want) in step["expect"].as_object().unwrap() {
            assert_eq!(
                reply.get(key),
                Some(want),
                "{handler} {name} step {n} {}: {key} — {reply}",
                step["send"]
            );
        }
        out.push(reply);
    }
    out
}

/// The shared file is also the python stores' contract, and python still coerces
/// text into numbers and accepts the old `keep` string/dict forms; these stores do
/// not (alpha: a wrongly typed argument is refused, never coerced). So the steps
/// that rely on coercion are left out here, and every refusal the declared
/// verbs now give is asserted in its refusal form instead.
fn alpha(cases: &mut Value) {
    let scenarios = cases["scenarios"].as_array_mut().unwrap();
    // scenarios that only test coercion
    scenarios.retain(|s| {
        !matches!(
            s["name"].as_str().unwrap(),
            "a-superscript-digit-is-no-id" | "a-zero-limit-is-no-limit"
        )
    });
    for s in scenarios.iter_mut() {
        let name = s["name"].as_str().unwrap().to_string();
        let list = s["steps"].as_array_mut().unwrap();
        // a step is dropped when its send carries a text where a number goes
        list.retain(|st| {
            let send = &st["send"];
            let text_id = |k: &str| send.get(k).is_some_and(Value::is_string);
            let text_in_ids = send["ids"]
                .as_array()
                .is_some_and(|a| a.iter().any(Value::is_string));
            !(text_id("after") || text_id("limit") || text_in_ids)
        });
        for st in list.iter_mut() {
            let send = st["send"].clone();
            let t = send["type"].as_str().unwrap_or("");
            let refused = |arg: &str| json!({"reason": "bad_arg", "arg": arg});
            match (name.as_str(), t) {
                // an `owner` is no argument of any verb: refused, never read
                ("owner-is-never-a-writable-field", "append" | "read") if send.get("owner").is_some() => {
                    st["expect"] = refused("owner");
                }
                ("refusals", "append") if send["rows"] == json!([]) => {
                    // an empty list is below the declared `minItems`: the engine's missing_arg
                    st["expect"] = json!({"reason": "missing_arg", "arg": "rows"});
                }
                ("digit-string-refs-are-not-found", "append") if send["rows"][0]["refs"] == json!(["4"]) => {
                    // a text ref is no row id: refused, not looked up
                    st["expect"] = refused("rows");
                }
                (_, "inject")
                    if send["tables"] == json!([1]) || send["tables"]["rows"] == json!({"owner": "x"}) =>
                {
                    st["expect"] = json!({"reason": "bad_arg"});
                }
                _ => {}
            }
        }
        if name == "prune-keep-is-python-in" {
            // `keep` is a list of kinds
            for st in list.iter_mut() {
                let keep = st["send"].get("keep").cloned();
                if let Some(k) = keep {
                    st["send"]["keep"] = match k {
                        Value::String(_) => json!(["fold", "old"]),
                        Value::Object(m) => json!(m.keys().collect::<Vec<_>>()),
                        other => other,
                    };
                }
            }
        }
    }
    // zero means no limit, as numbers
    scenarios.push(json!({
        "name": "a-zero-limit-is-no-limit-as-numbers",
        "steps": [
            {"as": "alice", "send": {"type": "append", "rows": [{"id": 1, "kind": "msg", "content": "x one"}, {"id": 2, "kind": "msg", "content": "x two"}]}, "expect": {"ids": [1, 2]}},
            {"as": "alice", "send": {"type": "read", "limit": 0}, "expect": {"total": 2, "next": null}},
            {"as": "alice", "send": {"type": "search", "query": "x", "limit": 0, "width": 0}, "expect": {"total": 2, "truncated": false}},
            {"as": "alice", "send": {"type": "read", "limit": "0"}, "expect": {"reason": "bad_arg", "arg": "limit"}},
            {"as": "alice", "send": {"type": "read", "ids": ["2"]}, "expect": {"reason": "bad_arg"}}
        ]
    }));
}

/// What the two stores may say differently: each names itself in an unknown-verb
/// error and in its reflect; everything else is the same.
fn comparable(v: &Value) -> Value {
    let mut v = v.clone();
    if v.get("sentence").is_some() {
        return json!({"owners": v["owners"], "rows": v["rows"]});
    }
    if v.get("reason") == Some(&json!("unhandled")) {
        v.as_object_mut().unwrap().remove("error");
    }
    v
}

#[tokio::test]
async fn every_store_answers_the_shared_cases() {
    let mut cases: Value = serde_json::from_str(CASES).expect("cases.json parses");
    alpha(&mut cases);
    let scenarios = cases["scenarios"].as_array().unwrap();
    assert!(!scenarios.is_empty());
    for scenario in scenarios {
        let ram = replay(RAM, scenario).await;
        let sql = replay(SQL, scenario).await;
        for (n, (a, b)) in ram.iter().zip(&sql).enumerate() {
            assert_eq!(
                comparable(a),
                comparable(b),
                "{} step {n}: ram_store and sqlite_store differ",
                scenario["name"]
            );
        }
    }
}
