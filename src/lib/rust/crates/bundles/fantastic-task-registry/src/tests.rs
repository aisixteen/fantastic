//! Unit tests: every one is driven through `kernel.send`, as production is. A
//! `caller.tools` agent stands for any agent that makes calls (a lead, a worker); the
//! store is a real `ram_store`.

use super::*;
use fantastic_kernel::Agent;
use tempfile::TempDir;

/// Every call a `spy.tools` agent received: (agent, verb).
static SPIED: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

struct CallerBundle;

#[async_trait]
impl Bundle for CallerBundle {
    fn name(&self) -> &str {
        "caller"
    }
    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        match verb {
            "boot" | "shutdown" => Ok(None),
            "reflect" => Ok(Some(
                json!({"id": agent_id.as_str(), "sentence": "a caller"}),
            )),
            "call" => {
                let target = payload["target"].as_str().unwrap_or("");
                let out = kernel
                    .send(vec![Item::new(
                        AgentId::from(target),
                        payload["payload"].clone(),
                    )])
                    .await
                    .remove(0);
                Ok(Some(out.result))
            }
            other => Ok(Some(json!({"ok": true, "verb": other}))),
        }
    }
}

/// Records every verb it is sent.
struct SpyBundle;

#[async_trait]
impl Bundle for SpyBundle {
    fn name(&self) -> &str {
        "spy"
    }
    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        if verb != "boot" && verb != "shutdown" {
            SPIED
                .lock()
                .unwrap()
                .push((agent_id.as_str().to_string(), verb.to_string()));
        }
        Ok(Some(json!({"ok": true})))
    }
}

const WRITE_VERBS: [&str; 7] = [
    "add", "assign", "reassign", "status", "report", "list", "get",
];

fn new_kernel() -> Kernel {
    let mut kernel = Kernel::new();
    kernel.bundles.register(
        fantastic_core::HANDLER_MODULE,
        fantastic_core::KernelStateBundle::default(),
    );
    kernel.bundles.register("caller.tools", CallerBundle);
    kernel.bundles.register("spy.tools", SpyBundle);
    kernel.bundles.register(
        fantastic_ram_store::HANDLER_MODULE,
        fantastic_ram_store::RamStoreBundle::default(),
    );
    kernel
        .bundles
        .register(HANDLER_MODULE, TaskRegistryBundle::default());
    kernel
}

async fn send(kernel: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    kernel
        .send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
        .result
}

async fn create(kernel: &Arc<Kernel>, payload: Value) -> Value {
    let mut p = payload;
    p["type"] = json!("create_agent");
    if p.get("parent_id").is_none() {
        p["parent_id"] = json!("kernel");
    }
    let r = send(kernel, "kernel_state", p).await;
    assert!(r.get("error").is_none(), "create: {r}");
    r
}

struct Rig {
    kernel: Arc<Kernel>,
    reg: String,
    store: String,
}

/// `lead` -> `sublead` -> `worker`, a `ram_store`, and a registry on it. The leads may
/// call the registry; the worker holds no grant on it.
async fn rig(tmp: &TempDir) -> Rig {
    let kernel = Arc::new(new_kernel());
    let root = Agent::new(
        AgentId::from("kernel_state"),
        Some(fantastic_core::HANDLER_MODULE.to_string()),
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let store = fantastic_ram_store::test_support::ensure_memory(&kernel)
        .await
        .as_str()
        .to_string();
    let reg = "tr".to_string();
    let grant = json!({reg.clone(): WRITE_VERBS});
    create(
        &kernel,
        json!({"handler_module": "caller.tools", "id": "lead", "can_call": grant.clone()}),
    )
    .await;
    create(
        &kernel,
        json!({"handler_module": "caller.tools", "id": "sublead", "parent_id": "lead",
               "can_call": grant}),
    )
    .await;
    create(
        &kernel,
        json!({"handler_module": "caller.tools", "id": "worker", "parent_id": "sublead"}),
    )
    .await;
    let r = Rig { kernel, reg, store };
    r.make_registry("tr").await;
    r
}

impl Rig {
    /// A registry agent `id` on the rig's store, wired both ends.
    async fn make_registry(&self, id: &str) {
        create(
            &self.kernel,
            json!({"handler_module": HANDLER_MODULE, "id": id,
                   "task_store_id": self.store,
                   "can_call": {self.store.clone(): "*"},
                   "callable_by": {"lead": "*", "sublead": "*"}}),
        )
        .await;
        let reg = id.to_string();
        self.kernel
            .edit_callable_by(&AgentId::from(self.store.as_str()), move |m| {
                m.insert(reg, json!("*"));
                true
            })
            .unwrap();
        // the leads may call every registry made here
        for who in ["lead", "sublead"] {
            let reg = id.to_string();
            self.kernel
                .edit_can_call(&AgentId::from(who), move |m| {
                    m.insert(reg, json!(WRITE_VERBS));
                    true
                })
                .unwrap();
        }
    }

    /// The operator calls the registry.
    async fn op(&self, payload: Value) -> Value {
        send(&self.kernel, &self.reg, payload).await
    }

    /// `who` calls the registry as itself.
    async fn call(&self, who: &str, payload: Value) -> Value {
        self.call_on(who, &self.reg, payload).await
    }

    async fn call_on(&self, who: &str, target: &str, payload: Value) -> Value {
        send(
            &self.kernel,
            who,
            json!({"type": "call", "target": target, "payload": payload}),
        )
        .await
    }

    async fn dump_rows(&self) -> Vec<Value> {
        let d = send(&self.kernel, &self.store, json!({"type": "dump"})).await;
        d["tables"]["rows"].as_array().cloned().unwrap_or_default()
    }

    async fn add(&self, who: &str, id: &str, extra: Value) -> Value {
        let mut p = json!({"type": "add", "id": id, "source": format!("src:{id}")});
        for (k, v) in extra.as_object().cloned().unwrap_or_default() {
            p[k] = v;
        }
        self.call(who, p).await
    }
}

#[test]
fn disabled_by_default() {
    let mut reg = BundleRegistry::new();
    assert!(!register_if_enabled(&mut reg, None));
    assert!(!register_if_enabled(&mut reg, Some("0")));
    assert!(!register_if_enabled(&mut reg, Some("")));
    assert!(reg.get(HANDLER_MODULE).is_none(), "off: no handler");
    assert!(register_if_enabled(&mut reg, Some("1")));
    assert!(reg.get(HANDLER_MODULE).is_some());
    assert!(
        enabled_value(Some("TRUE")) && enabled_value(Some("yes")) && !enabled_value(Some("no"))
    );
}

#[tokio::test]
async fn refuses_every_verb_without_task_store_id() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    create(
        &r.kernel,
        json!({"handler_module": HANDLER_MODULE, "id": "bare"}),
    )
    .await;
    let refl = send(&r.kernel, "bare", json!({"type": "reflect"})).await;
    assert_eq!(refl["enabled"], false, "{refl}");
    for verb in WRITE_VERBS {
        let out = send(&r.kernel, "bare", json!({"type": verb, "id": "t"})).await;
        assert_eq!(out["reason"], "disabled", "{verb}: {out}");
    }
}

#[tokio::test]
async fn add_appends_one_row_to_the_store_agent() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    let added = r
        .add(
            "lead",
            "t1",
            json!({"source": "gh:o/r#12", "holder": "sublead"}),
        )
        .await;
    assert_eq!(added["id"], "t1", "{added}");
    let rows = r.dump_rows().await;
    let tasks: Vec<&Value> = rows.iter().filter(|x| x["row"]["kind"] == "task").collect();
    assert_eq!(tasks.len(), 1, "{rows:?}");
    assert!(
        tasks[0]["owner"].as_str().unwrap().ends_with("|tr"),
        "owned by the registry: {}",
        tasks[0]["owner"]
    );
    assert_eq!(tasks[0]["row"]["task_id"], "t1");
    assert_eq!(tasks[0]["row"]["content"], added);
    assert_eq!(added["source"], "gh:o/r#12");
    assert_eq!(added["status"], "waiting");
    assert_eq!(added["holder"], "sublead");
    let got = r.call("lead", json!({"type": "get", "id": "t1"})).await;
    let mut want = added.clone();
    want["children"] = json!([]);
    assert_eq!(got, want);
}

#[tokio::test]
async fn add_refuses_duplicate_id_and_unknown_parent() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    assert_eq!(r.add("lead", "t1", json!({})).await["id"], "t1");
    let dup = r.add("lead", "t1", json!({})).await;
    assert_eq!(dup["reason"], "exists", "{dup}");
    let orphan = r.add("lead", "t2", json!({"parent": "nope"})).await;
    assert_eq!(orphan["reason"], "not_found", "{orphan}");
    let none = r.call("lead", json!({"type": "list"})).await;
    assert_eq!(none["tasks"].as_array().unwrap().len(), 1, "{none}");
}

#[tokio::test]
async fn get_unknown_is_not_found() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    let out = r.call("lead", json!({"type": "get", "id": "ghost"})).await;
    assert_eq!(out["reason"], "not_found", "{out}");
}

#[tokio::test]
async fn source_is_opaque_and_nothing_is_pushed() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    // a spy the registry could call if it wanted to: it is wired both ends
    create(
        &r.kernel,
        json!({"handler_module": "spy.tools", "id": "spy", "callable_by": {"tr": "*"}}),
    )
    .await;
    r.kernel
        .edit_can_call(&AgentId::from("tr"), |m| {
            m.insert("spy".to_string(), json!("*"));
            true
        })
        .unwrap();
    for (i, src) in ["gh:o/r#12", "jira:KEY-5", "9f3 !? random \u{1F600}"]
        .iter()
        .enumerate()
    {
        let id = format!("t{i}");
        let out = r.add("lead", &id, json!({"source": src})).await;
        assert_eq!(out["source"], *src, "verbatim: {out}");
        assert_eq!(out["update_to"], *src, "update_to defaults to source");
    }
    let own = r
        .add(
            "lead",
            "t9",
            json!({"source": "gh:o/r#1", "update_to": "jira:KEY-9"}),
        )
        .await;
    assert_eq!(own["update_to"], "jira:KEY-9");
    r.call(
        "lead",
        json!({"type": "status", "id": "t0", "status": "done"}),
    )
    .await;
    r.call(
        "lead",
        json!({"type": "assign", "id": "t0", "holder": "worker"}),
    )
    .await;
    assert!(
        SPIED.lock().unwrap().iter().all(|(a, _)| a != "spy"),
        "the registry called a source agent"
    );
    // all its traffic went to the store: one row per write
    let rows = r.dump_rows().await;
    assert_eq!(rows.len(), 6, "4 adds + status + assign: {rows:?}");
}

#[tokio::test]
async fn assign_sets_holder_and_history() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    r.add("lead", "t1", json!({})).await;
    let a = r
        .call(
            "lead",
            json!({"type": "assign", "id": "t1", "holder": "sublead"}),
        )
        .await;
    assert_eq!(a["holder"], "sublead", "{a}");
    let h = a["history"].as_array().unwrap();
    assert_eq!(h.len(), 1);
    assert_eq!(
        (h[0]["from"].clone(), h[0]["to"].clone(), h[0]["by"].clone()),
        (Value::Null, json!("sublead"), json!("lead"))
    );
    assert!(h[0]["at"].as_f64().unwrap() > 0.0);
    let again = r
        .call(
            "lead",
            json!({"type": "assign", "id": "t1", "holder": "worker"}),
        )
        .await;
    assert_eq!(again["reason"], "already_held", "{again}");
    let got = r.call("lead", json!({"type": "get", "id": "t1"})).await;
    assert_eq!(got["holder"], "sublead");
    assert_eq!(got["history"].as_array().unwrap().len(), 1);
    // the operator assigns as null
    r.add("lead", "t2", json!({})).await;
    let op = r
        .op(json!({"type": "assign", "id": "t2", "holder": "worker"}))
        .await;
    assert_eq!(op["history"][0]["by"], Value::Null, "{op}");
}

#[tokio::test]
async fn reassign_requires_a_holder() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    r.add("lead", "t1", json!({})).await;
    let out = r
        .call(
            "lead",
            json!({"type": "reassign", "id": "t1", "holder": "worker"}),
        )
        .await;
    assert_eq!(out["reason"], "not_held", "{out}");
    let got = r.call("lead", json!({"type": "get", "id": "t1"})).await;
    assert_eq!(got["holder"], Value::Null);
    assert_eq!(got["history"], json!([]));
}

#[tokio::test]
async fn reassign_agent_to_agent_and_to_subtree_and_back_keeps_history() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    r.add("lead", "t1", json!({})).await;
    r.call(
        "lead",
        json!({"type": "assign", "id": "t1", "holder": "sublead"}),
    )
    .await;
    let two = r
        .call(
            "sublead",
            json!({"type": "reassign", "id": "t1", "holder": {"subtree": "lead"}}),
        )
        .await;
    assert_eq!(two["holder"], json!({"subtree": "lead"}), "{two}");
    let three = r
        .call(
            "lead",
            json!({"type": "reassign", "id": "t1", "holder": "worker"}),
        )
        .await;
    assert_eq!(three["holder"], "worker", "{three}");
    let h = three["history"].as_array().unwrap();
    assert_eq!(h.len(), 3);
    let steps: Vec<(Value, Value, Value)> = h
        .iter()
        .map(|x| (x["from"].clone(), x["to"].clone(), x["by"].clone()))
        .collect();
    assert_eq!(
        steps,
        vec![
            (Value::Null, json!("sublead"), json!("lead")),
            (
                json!("sublead"),
                json!({"subtree": "lead"}),
                json!("sublead")
            ),
            (json!({"subtree": "lead"}), json!("worker"), json!("lead")),
        ]
    );
}

#[tokio::test]
async fn holder_must_exist() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    let add = r.add("lead", "t0", json!({"holder": "nobody"})).await;
    assert_eq!(add["reason"], "not_found", "{add}");
    r.add("lead", "t1", json!({})).await;
    let a = r
        .call(
            "lead",
            json!({"type": "assign", "id": "t1", "holder": "nobody"}),
        )
        .await;
    assert_eq!(a["reason"], "not_found", "{a}");
    let sub = r
        .call(
            "lead",
            json!({"type": "assign", "id": "t1", "holder": {"subtree": "nobody"}}),
        )
        .await;
    assert_eq!(sub["reason"], "not_found", "{sub}");
    r.call(
        "lead",
        json!({"type": "assign", "id": "t1", "holder": "worker"}),
    )
    .await;
    let re = r
        .call(
            "lead",
            json!({"type": "reassign", "id": "t1", "holder": "nobody"}),
        )
        .await;
    assert_eq!(re["reason"], "not_found", "{re}");
    let got = r.call("lead", json!({"type": "get", "id": "t1"})).await;
    assert_eq!(got["holder"], "worker", "unchanged");
}

#[tokio::test]
async fn status_sets_and_rejects_unknown() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    r.add("lead", "t1", json!({})).await;
    for s in STATES {
        let out = r
            .call("lead", json!({"type": "status", "id": "t1", "status": s}))
            .await;
        assert_eq!(out["status"], s, "{out}");
    }
    let bad = r
        .call(
            "lead",
            json!({"type": "status", "id": "t1", "status": "finished"}),
        )
        .await;
    assert_eq!(bad["reason"], "bad_arg", "{bad}");
    let got = r.call("lead", json!({"type": "get", "id": "t1"})).await;
    assert_eq!(got["status"], "paused", "unchanged by the bad one");
    let bad_add = r.add("lead", "t2", json!({"status": "nope"})).await;
    assert_eq!(bad_add["reason"], "bad_arg", "{bad_add}");
    let ghost = r
        .call(
            "lead",
            json!({"type": "status", "id": "ghost", "status": "done"}),
        )
        .await;
    assert_eq!(ghost["reason"], "not_found");
}

#[tokio::test]
async fn report_records_last_report_from_own_subtree() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    r.add("lead", "t1", json!({"holder": "worker"})).await;
    let one = r
        .call(
            "lead",
            json!({"type": "report", "id": "t1", "from": "worker", "state": "running",
                   "result": {"pct": 40}}),
        )
        .await;
    let rep = &one["report"];
    assert_eq!(
        (
            rep["from"].clone(),
            rep["state"].clone(),
            rep["result"].clone()
        ),
        (json!("worker"), json!("running"), json!({"pct": 40})),
        "{one}"
    );
    assert!(rep["at"].as_f64().unwrap() > 0.0);
    // the default `from` is the caller; a second report replaces the first
    let two = r
        .call(
            "sublead",
            json!({"type": "report", "id": "t1", "state": "done", "result": "ok"}),
        )
        .await;
    assert_eq!(two["report"]["from"], "sublead", "{two}");
    let got = r.call("lead", json!({"type": "get", "id": "t1"})).await;
    assert_eq!(got["report"], two["report"]);
    assert_eq!(got["report"]["state"], "done");
    let bad = r
        .call(
            "lead",
            json!({"type": "report", "id": "t1", "state": "finished"}),
        )
        .await;
    assert_eq!(bad["reason"], "bad_arg", "{bad}");
}

#[tokio::test]
async fn report_for_an_agent_outside_the_callers_subtree_is_refused() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    r.add("lead", "t1", json!({})).await;
    // sublead reports for its parent: not its subtree
    let up = r
        .call(
            "sublead",
            json!({"type": "report", "id": "t1", "from": "lead", "state": "done"}),
        )
        .await;
    assert_eq!(up["reason"], "not_in_subtree", "{up}");
    // worker is below sublead but a sibling-free check: lead -> sublead is fine, the
    // reverse is not
    let ghost = r
        .call(
            "lead",
            json!({"type": "report", "id": "t1", "from": "nobody", "state": "done"}),
        )
        .await;
    assert_eq!(ghost["reason"], "not_found", "{ghost}");
    let got = r.call("lead", json!({"type": "get", "id": "t1"})).await;
    assert_eq!(got["report"], Value::Null, "the entry is unchanged");
    let rows = r.dump_rows().await;
    assert_eq!(rows.len(), 1, "no row was written: {rows:?}");
}

#[tokio::test]
async fn list_filters_by_parent_holder_and_status() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    r.add("lead", "b", json!({"holder": "sublead"})).await;
    r.add(
        "lead",
        "a",
        json!({"parent": "b", "holder": {"subtree": "sublead"}}),
    )
    .await;
    r.add("lead", "c", json!({"parent": "b", "status": "running"}))
        .await;
    let ids = |v: &Value| -> Vec<String> {
        v["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_str().unwrap().to_string())
            .collect()
    };
    let all = r.call("lead", json!({"type": "list"})).await;
    assert_eq!(ids(&all), ["a", "b", "c"], "sorted by id");
    let by_parent = r.call("lead", json!({"type": "list", "parent": "b"})).await;
    assert_eq!(ids(&by_parent), ["a", "c"]);
    let by_holder = r
        .call("lead", json!({"type": "list", "holder": "sublead"}))
        .await;
    assert_eq!(ids(&by_holder), ["b"]);
    let by_subtree = r
        .call(
            "lead",
            json!({"type": "list", "holder": {"subtree": "sublead"}}),
        )
        .await;
    assert_eq!(ids(&by_subtree), ["a"]);
    let by_status = r
        .call("lead", json!({"type": "list", "status": "running"}))
        .await;
    assert_eq!(ids(&by_status), ["c"]);
    let both = r
        .call(
            "lead",
            json!({"type": "list", "parent": "b", "status": "waiting"}),
        )
        .await;
    assert_eq!(ids(&both), ["a"]);
}

#[tokio::test]
async fn get_lists_children_and_history() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    r.add("lead", "top", json!({})).await;
    r.add("lead", "mid", json!({"parent": "top"})).await;
    r.add("lead", "leaf", json!({"parent": "mid"})).await;
    r.add("lead", "mid2", json!({"parent": "top"})).await;
    r.call(
        "lead",
        json!({"type": "assign", "id": "mid", "holder": "sublead"}),
    )
    .await;
    r.call(
        "lead",
        json!({"type": "reassign", "id": "mid", "holder": "worker"}),
    )
    .await;
    let top = r.call("lead", json!({"type": "get", "id": "top"})).await;
    assert_eq!(top["children"], json!(["mid", "mid2"]), "{top}");
    let mid = r.call("lead", json!({"type": "get", "id": "mid"})).await;
    assert_eq!(mid["children"], json!(["leaf"]));
    assert_eq!(mid["parent"], "top");
    assert_eq!(mid["history"].as_array().unwrap().len(), 2);
    let leaf = r.call("lead", json!({"type": "get", "id": "leaf"})).await;
    assert_eq!(leaf["children"], json!([]));
    assert_eq!(leaf["parent"], "mid");
}

#[tokio::test]
async fn worker_without_a_grant_cannot_write() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    r.add("lead", "t1", json!({})).await;
    for p in [
        json!({"type": "add", "id": "w", "source": "x"}),
        json!({"type": "assign", "id": "t1", "holder": "worker"}),
        json!({"type": "reassign", "id": "t1", "holder": "worker"}),
        json!({"type": "status", "id": "t1", "status": "done"}),
        json!({"type": "report", "id": "t1", "state": "done"}),
    ] {
        let out = r.call("worker", p.clone()).await;
        assert_eq!(out["reason"], "unauthorized", "{p}: {out}");
    }
    let got = r.call("lead", json!({"type": "get", "id": "t1"})).await;
    assert_eq!(got["holder"], Value::Null);
    assert_eq!(got["status"], "waiting");
    assert_eq!(r.dump_rows().await.len(), 1, "only the lead's add");
    // the lead's own calls went through
    assert!(r
        .call(
            "lead",
            json!({"type": "status", "id": "t1", "status": "done"})
        )
        .await
        .get("error")
        .is_none());
}

#[tokio::test]
async fn state_survives_a_registry_restart() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    r.add("lead", "t1", json!({})).await;
    r.add("lead", "t2", json!({"parent": "t1"})).await;
    r.call(
        "lead",
        json!({"type": "assign", "id": "t2", "holder": "worker"}),
    )
    .await;
    let before = r.call("lead", json!({"type": "list"})).await;
    let gone = send(
        &r.kernel,
        "kernel_state",
        json!({"type": "delete_agent", "id": "tr"}),
    )
    .await;
    assert_eq!(gone["deleted"], true, "{gone}");
    r.make_registry("tr").await;
    let after = r.call("lead", json!({"type": "list"})).await;
    assert_eq!(after, before);
    assert_eq!(after["tasks"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn two_registries_on_one_store_do_not_mix() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    r.make_registry("tr2").await;
    r.add("lead", "mine", json!({})).await;
    let other = r
        .call_on(
            "lead",
            "tr2",
            json!({"type": "add", "id": "theirs", "source": "s"}),
        )
        .await;
    assert_eq!(other["id"], "theirs", "{other}");
    let ids = |v: &Value| -> Vec<String> {
        v["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_str().unwrap().to_string())
            .collect()
    };
    let one = r.call("lead", json!({"type": "list"})).await;
    let two = r.call_on("lead", "tr2", json!({"type": "list"})).await;
    assert_eq!(ids(&one), ["mine"]);
    assert_eq!(ids(&two), ["theirs"]);
    // the same id may exist in both
    assert_eq!(
        r.call_on(
            "lead",
            "tr2",
            json!({"type": "add", "id": "mine", "source": "s"})
        )
        .await["id"],
        "mine"
    );
    assert_eq!(r.dump_rows().await.len(), 3);
}

#[tokio::test]
async fn store_refusal_passes_through() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    r.add("lead", "t1", json!({})).await;
    r.kernel
        .edit_callable_by(&AgentId::from(r.store.as_str()), |m| {
            m.remove("tr").is_some()
        })
        .unwrap();
    let out = r.add("lead", "t2", json!({})).await;
    assert_eq!(out["reason"], "store_refused", "{out}");
    assert!(
        out["store_error"].as_str().is_some_and(|e| !e.is_empty()),
        "carries the store's own error: {out}"
    );
    assert_eq!(out["store_reason"], "unauthorized", "{out}");
    let read = r.call("lead", json!({"type": "list"})).await;
    assert_eq!(
        read["reason"], "store_refused",
        "reads pass it through too: {read}"
    );
    // reopen the store: nothing was created by the refused add
    r.kernel
        .edit_callable_by(&AgentId::from(r.store.as_str()), |m| {
            m.insert("tr".to_string(), json!("*"));
            true
        })
        .unwrap();
    let list = r.call("lead", json!({"type": "list"})).await;
    assert_eq!(list["tasks"].as_array().unwrap().len(), 1, "{list}");
}

fn vers(n: u32) -> Value {
    json!([{"repo": format!("o/r{n}"), "branch": format!("b{n}"), "commit": format!("c{n}")}])
}

fn fork() -> Value {
    json!({"agent": "ghost_agent", "mark": "m1"})
}

fn usage(i: u64, cr: u64, cw: u64, o: u64) -> Value {
    json!({"input_tokens": i, "cache_read_tokens": cr, "cache_write_tokens": cw, "output_tokens": o})
}

async fn task_rows(r: &Rig) -> usize {
    r.dump_rows()
        .await
        .iter()
        .filter(|x| x["row"]["kind"] == "task")
        .count()
}

#[tokio::test]
async fn add_records_versions_source_version_and_fork_from() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    let added = r
        .add(
            "lead",
            "t1",
            json!({"versions": vers(1), "source_version": "2026-10-01T00:00:00Z", "fork_from": fork(),
                   "holder": "sublead"}),
        )
        .await;
    assert_eq!(added["versions"], vers(1), "{added}");
    assert_eq!(added["source_version"], "2026-10-01T00:00:00Z");
    assert_eq!(added["fork_from"], fork());
    assert_eq!(
        added["history"][0]["versions"],
        vers(1),
        "first history item"
    );
    let got = r.call("lead", json!({"type": "get", "id": "t1"})).await;
    assert_eq!(got["versions"], vers(1));
    assert_eq!(got["fork_from"], fork());
    let rows = r.dump_rows().await;
    let row = rows.iter().find(|x| x["row"]["kind"] == "task").unwrap();
    assert_eq!(row["row"]["content"]["versions"], vers(1));
    assert_eq!(
        row["row"]["content"]["source_version"],
        "2026-10-01T00:00:00Z"
    );
    assert_eq!(row["row"]["content"]["fork_from"], fork());
}

#[tokio::test]
async fn add_without_the_new_facts_has_empty_defaults() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    let added = r.add("lead", "t1", json!({"holder": "sublead"})).await;
    assert_eq!(added["versions"], json!([]), "{added}");
    assert_eq!(added["source_version"], Value::Null);
    assert_eq!(added["fork_from"], Value::Null);
    assert_eq!(added["usage"], Value::Null);
    let keys: Vec<&String> = added["history"][0].as_object().unwrap().keys().collect();
    assert_eq!(keys.len(), 4, "{keys:?}");
}

#[tokio::test]
async fn versions_must_be_a_list_of_repo_branch_commit() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    for bad in [
        json!("o/r@main"),
        json!({"repo": "a", "branch": "b", "commit": "c"}),
        json!([{"repo": "a", "branch": "b"}]),
        json!([{"repo": "a", "branch": "b", "commit": "c", "extra": "d"}]),
        json!([{"repo": "a", "branch": "", "commit": "c"}]),
        json!([{"repo": "a", "branch": "b", "commit": 7}]),
        json!(["a"]),
    ] {
        let out = r.add("lead", "t1", json!({"versions": bad})).await;
        assert_eq!(out["reason"], "bad_arg", "{bad}: {out}");
        assert_eq!(out["arg"], "versions", "{out}");
    }
    assert_eq!(task_rows(&r).await, 0, "nothing was appended");
    r.add("lead", "t1", json!({})).await;
    let out = r
        .call(
            "lead",
            json!({"type": "assign", "id": "t1", "holder": "worker", "versions": "x"}),
        )
        .await;
    assert_eq!(out["reason"], "bad_arg", "{out}");
    assert_eq!(task_rows(&r).await, 1, "only the add");
    // an empty list is allowed
    let ok = r.add("lead", "t2", json!({"versions": []})).await;
    assert_eq!(ok["versions"], json!([]), "{ok}");
}

#[tokio::test]
async fn fork_from_needs_agent_and_mark() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    for bad in [
        json!({"agent": "a"}),
        json!("a:m"),
        json!({"agent": "a", "mark": ""}),
        json!({"agent": "a", "mark": "m", "x": 1}),
    ] {
        let out = r.add("lead", "t1", json!({"fork_from": bad})).await;
        assert_eq!(out["reason"], "bad_arg", "{bad}: {out}");
        assert_eq!(out["arg"], "fork_from", "{out}");
    }
    assert_eq!(task_rows(&r).await, 0);
    let ok = r.add("lead", "t1", json!({"fork_from": fork()})).await;
    assert_eq!(
        ok["fork_from"],
        fork(),
        "an opaque agent that is not in the kernel: {ok}"
    );
}

#[tokio::test]
async fn assign_sets_the_facts_and_reassign_keeps_them_in_history() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    r.add("lead", "t1", json!({})).await;
    let a = r
        .call(
            "lead",
            json!({"type": "assign", "id": "t1", "holder": "sublead", "versions": vers(1),
                   "source_version": "v1", "fork_from": fork()}),
        )
        .await;
    assert_eq!(a["versions"], vers(1), "{a}");
    let b = r
        .call(
            "lead",
            json!({"type": "reassign", "id": "t1", "holder": "worker", "versions": vers(2),
                   "source_version": "v2"}),
        )
        .await;
    assert_eq!(b["versions"], vers(2), "{b}");
    assert_eq!(b["source_version"], "v2");
    assert_eq!(b["fork_from"], fork(), "absent arg leaves it");
    let h = b["history"].as_array().unwrap();
    assert_eq!(h[0]["versions"], vers(1));
    assert_eq!(h[0]["source_version"], "v1");
    assert_eq!(h[0]["fork_from"], fork());
    assert_eq!(h[1]["versions"], vers(2));
    assert_eq!(h[1]["source_version"], "v2");
    // an explicit [] clears the list
    let c = r
        .call(
            "lead",
            json!({"type": "reassign", "id": "t1", "holder": "sublead", "versions": []}),
        )
        .await;
    assert_eq!(c["versions"], json!([]), "{c}");
    assert!(c["history"][2].get("versions").is_none(), "{c}");
}

#[tokio::test]
async fn history_item_omits_facts_when_none_were_given() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    r.add("lead", "t1", json!({})).await;
    r.call(
        "lead",
        json!({"type": "assign", "id": "t1", "holder": "sublead"}),
    )
    .await;
    let b = r
        .call(
            "lead",
            json!({"type": "reassign", "id": "t1", "holder": "worker"}),
        )
        .await;
    for item in b["history"].as_array().unwrap() {
        let mut keys: Vec<&str> = item
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["at", "by", "from", "to"], "{item}");
    }
}

#[tokio::test]
async fn lead_updates_the_facts_through_status() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    r.add("lead", "t1", json!({"status": "running"})).await;
    let out = r
        .call(
            "lead",
            json!({"type": "status", "id": "t1", "versions": vers(3), "source_version": "v9",
                   "fork_from": fork()}),
        )
        .await;
    assert!(out.get("error").is_none(), "{out}");
    assert_eq!(out["status"], "running", "unchanged");
    assert_eq!(out["versions"], vers(3));
    assert_eq!(out["source_version"], "v9");
    assert_eq!(out["fork_from"], fork());
    let got = r.call("lead", json!({"type": "get", "id": "t1"})).await;
    assert_eq!(got["versions"], vers(3));
    let both = r
        .call(
            "lead",
            json!({"type": "status", "id": "t1", "status": "done", "versions": []}),
        )
        .await;
    assert_eq!(
        (both["status"].clone(), both["versions"].clone()),
        (json!("done"), json!([]))
    );
}

#[tokio::test]
async fn status_needs_status_or_a_fact() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    r.add("lead", "t1", json!({})).await;
    let out = r.call("lead", json!({"type": "status", "id": "t1"})).await;
    assert_eq!(out["reason"], "missing_arg", "{out}");
    assert_eq!(out["arg"], "status", "{out}");
    let bad = r
        .call(
            "lead",
            json!({"type": "status", "id": "t1", "status": "nope", "versions": vers(1)}),
        )
        .await;
    assert_eq!(bad["reason"], "bad_arg", "{bad}");
    assert_eq!(task_rows(&r).await, 1);
}

#[tokio::test]
async fn worker_without_a_grant_cannot_update_the_facts() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    r.add("lead", "t1", json!({"versions": vers(1)})).await;
    let out = r
        .call(
            "worker",
            json!({"type": "status", "id": "t1", "versions": vers(2)}),
        )
        .await;
    assert_eq!(out["reason"], "unauthorized", "{out}");
    let got = r.call("lead", json!({"type": "get", "id": "t1"})).await;
    assert_eq!(got["versions"], vers(1), "unchanged");
    assert_eq!(task_rows(&r).await, 1);
}

#[tokio::test]
async fn report_records_usage_and_keeps_the_last() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    r.add("lead", "t1", json!({})).await;
    let one = r
        .call(
            "lead",
            json!({"type": "report", "id": "t1", "state": "running",
                   "usage": {"input_tokens": 10, "cache_read_tokens": 20,
                             "cache_write_tokens": 30, "output_tokens": 40}}),
        )
        .await;
    assert_eq!(one["usage"], usage(10, 20, 30, 40), "{one}");
    assert_eq!(one["report"]["usage"], usage(10, 20, 30, 40));
    let two = r
        .call(
            "lead",
            json!({"type": "report", "id": "t1", "state": "done"}),
        )
        .await;
    assert_eq!(two["usage"], usage(10, 20, 30, 40), "kept: {two}");
    assert_eq!(two["report"]["usage"], Value::Null);
    let three = r
        .call(
            "lead",
            json!({"type": "report", "id": "t1", "usage": usage(1, 2, 3, 4)}),
        )
        .await;
    assert_eq!(three["usage"], usage(1, 2, 3, 4), "{three}");
}

#[tokio::test]
async fn report_usage_fills_missing_keys_with_zero() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    r.add("lead", "t1", json!({})).await;
    let out = r
        .call(
            "lead",
            json!({"type": "report", "id": "t1", "usage": {"input_tokens": 5}}),
        )
        .await;
    assert_eq!(out["usage"], usage(5, 0, 0, 0), "{out}");
    assert_eq!(out["report"]["usage"], usage(5, 0, 0, 0));
}

#[tokio::test]
async fn report_usage_rejects_unknown_keys_and_negatives_and_floats() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    r.add("lead", "t1", json!({})).await;
    for bad in [
        json!({"total_tokens": 5}),
        json!({"input_tokens": -1}),
        json!({"input_tokens": 1.5}),
        json!({"input_tokens": "5"}),
        json!(5),
        json!([1]),
    ] {
        let out = r
            .call("lead", json!({"type": "report", "id": "t1", "usage": bad}))
            .await;
        assert_eq!(out["reason"], "bad_arg", "{bad}: {out}");
        assert_eq!(out["arg"], "usage", "{out}");
    }
    let got = r.call("lead", json!({"type": "get", "id": "t1"})).await;
    assert_eq!(got["usage"], Value::Null);
    assert_eq!(got["report"], Value::Null);
    assert_eq!(task_rows(&r).await, 1);
}

async fn report_usage(r: &Rig, id: &str, u: Value) {
    let out = r
        .call("lead", json!({"type": "report", "id": id, "usage": u}))
        .await;
    assert!(out.get("error").is_none(), "{out}");
}

#[tokio::test]
async fn list_sums_usage_over_the_given_task_ids() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    for id in ["a", "b", "c"] {
        r.add("lead", id, json!({})).await;
    }
    report_usage(&r, "a", usage(1, 2, 3, 4)).await;
    report_usage(&r, "b", usage(10, 20, 30, 40)).await;
    report_usage(&r, "c", usage(100, 200, 300, 400)).await;
    let out = r
        .call("lead", json!({"type": "list", "ids": ["a", "b"]}))
        .await;
    let ids: Vec<&str> = out["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["a", "b"], "{out}");
    assert_eq!(out["usage"], usage(11, 22, 33, 44), "{out}");
}

#[tokio::test]
async fn list_usage_counts_tasks_without_usage_as_zero() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    r.add("lead", "a", json!({})).await;
    r.add("lead", "b", json!({})).await;
    report_usage(&r, "a", usage(1, 2, 3, 4)).await;
    let out = r
        .call("lead", json!({"type": "list", "ids": ["a", "b"]}))
        .await;
    assert_eq!(out["usage"], usage(1, 2, 3, 4), "{out}");
    let none = r.call("lead", json!({"type": "list", "ids": ["b"]})).await;
    assert_eq!(none["usage"], usage(0, 0, 0, 0), "{none}");
}

#[tokio::test]
async fn list_ids_unknown_is_not_found() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    r.add("lead", "a", json!({})).await;
    let out = r
        .call("lead", json!({"type": "list", "ids": ["a", "ghost"]}))
        .await;
    assert_eq!(out["reason"], "not_found", "{out}");
    for bad in [json!("a"), json!([1]), json!([""])] {
        let out = r.call("lead", json!({"type": "list", "ids": bad})).await;
        assert_eq!(out["reason"], "bad_arg", "{bad}: {out}");
    }
}

#[tokio::test]
async fn list_usage_totals_all_returned_tasks_without_ids() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    r.add("lead", "a", json!({"holder": "worker"})).await;
    r.add("lead", "b", json!({"holder": "sublead"})).await;
    report_usage(&r, "a", usage(1, 1, 1, 1)).await;
    report_usage(&r, "b", usage(5, 5, 5, 5)).await;
    let all = r.call("lead", json!({"type": "list"})).await;
    assert_eq!(all["usage"], usage(6, 6, 6, 6), "{all}");
    let one = r
        .call("lead", json!({"type": "list", "holder": "worker"}))
        .await;
    assert_eq!(one["usage"], usage(1, 1, 1, 1), "{one}");
    let empty = r
        .call("lead", json!({"type": "list", "status": "done"}))
        .await;
    assert_eq!(empty["usage"], usage(0, 0, 0, 0), "{empty}");
}

#[tokio::test]
async fn facts_survive_a_registry_restart() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    r.add(
        "lead",
        "t1",
        json!({"versions": vers(1), "fork_from": fork()}),
    )
    .await;
    report_usage(&r, "t1", usage(1, 2, 3, 4)).await;
    send(
        &r.kernel,
        "kernel_state",
        json!({"type": "delete_agent", "id": "tr"}),
    )
    .await;
    r.make_registry("tr").await;
    let got = r.call("lead", json!({"type": "get", "id": "t1"})).await;
    assert_eq!(got["versions"], vers(1), "{got}");
    assert_eq!(got["fork_from"], fork());
    assert_eq!(got["usage"], usage(1, 2, 3, 4));
}

#[tokio::test]
async fn entries_from_before_the_facts_still_load() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    // a snapshot written before the facts existed: strip the keys from a real row
    r.add("lead", "old", json!({"source": "gh:o/r#1"})).await;
    let mut tables = send(&r.kernel, &r.store, json!({"type": "dump"})).await["tables"].clone();
    for row in tables["rows"].as_array_mut().unwrap() {
        let c = row["row"]["content"].as_object_mut().unwrap();
        for k in ["versions", "source_version", "fork_from", "usage"] {
            c.remove(k);
        }
    }
    let inj = send(
        &r.kernel,
        &r.store,
        json!({"type": "inject", "tables": tables}),
    )
    .await;
    assert!(inj.get("error").is_none(), "{inj}");
    let got = r.call("lead", json!({"type": "get", "id": "old"})).await;
    assert_eq!(got["versions"], json!([]), "{got}");
    assert_eq!(got["source_version"], Value::Null);
    assert_eq!(got["usage"], Value::Null);
    let list = r.call("lead", json!({"type": "list"})).await;
    assert_eq!(list["tasks"][0]["fork_from"], Value::Null, "{list}");
    let a = r
        .call(
            "lead",
            json!({"type": "assign", "id": "old", "holder": "worker"}),
        )
        .await;
    assert_eq!(a["versions"], json!([]), "{a}");
    assert_eq!(a["history"][0].as_object().unwrap().len(), 4);
    let rep = r
        .call(
            "lead",
            json!({"type": "report", "id": "old", "state": "done"}),
        )
        .await;
    assert_eq!(rep["usage"], Value::Null, "{rep}");
    let rows = r.dump_rows().await;
    let last = rows
        .iter()
        .rev()
        .find(|x| x["row"]["task_id"] == "old")
        .unwrap();
    assert_eq!(
        last["row"]["content"]["versions"],
        json!([]),
        "rewritten with defaults"
    );
}

#[test]
fn readme_is_python_verbatim() {
    let py = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../python/bundled_agents/task_registry/src/task_registry/readme.md");
    let src = std::fs::read_to_string(&py).unwrap_or_else(|e| panic!("{}: {e}", py.display()));
    assert_eq!(src, README, "readme has drifted from {}", py.display());
}

// ── the refusal shape: `error` is a sentence, `reason` is the code ──────────

fn assert_shape(out: &Value, code: &str) {
    assert_eq!(out["reason"], code, "{out}");
    let sentence = out["error"].as_str().unwrap_or_else(|| panic!("{out}"));
    assert!(sentence.contains(' ') && sentence != code, "{out}");
}

#[tokio::test]
async fn disabled_refusal_reason_is_code_error_is_sentence() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    create(
        &r.kernel,
        json!({"handler_module": HANDLER_MODULE, "id": "bare"}),
    )
    .await;
    for verb in WRITE_VERBS {
        let out = send(&r.kernel, "bare", json!({"type": verb, "id": "t"})).await;
        assert_shape(&out, "disabled");
        assert!(
            out["error"]
                .as_str()
                .unwrap()
                .contains("task_registry is disabled"),
            "{out}"
        );
        assert!(out["hint"].is_string(), "{out}");
    }
}

#[tokio::test]
async fn store_refused_reason_is_code_keeps_store_extras() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    r.kernel
        .edit_callable_by(&AgentId::from(r.store.as_str()), |m| {
            m.remove("tr").is_some()
        })
        .unwrap();
    let out = r.add("lead", "t2", json!({})).await;
    assert_shape(&out, "store_refused");
    assert!(out["error"].as_str().unwrap().starts_with("store"), "{out}");
    assert!(
        out["store_error"].as_str().is_some_and(|e| !e.is_empty()),
        "{out}"
    );
    assert_eq!(out["store_reason"], "unauthorized", "{out}");
}

#[tokio::test]
async fn already_held_refusal_reason_is_code() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    r.add("lead", "t1", json!({})).await;
    let first = json!({"type": "assign", "id": "t1", "holder": "sublead"});
    r.call("lead", first).await;
    let again = r
        .call(
            "lead",
            json!({"type": "assign", "id": "t1", "holder": "worker"}),
        )
        .await;
    assert_shape(&again, "already_held");
    assert!(
        again["error"].as_str().unwrap().contains("reassign"),
        "{again}"
    );
}

#[tokio::test]
async fn not_held_refusal_reason_is_code() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    r.add("lead", "t1", json!({})).await;
    let out = r
        .call(
            "lead",
            json!({"type": "reassign", "id": "t1", "holder": "worker"}),
        )
        .await;
    assert_shape(&out, "not_held");
    assert!(out["error"].as_str().unwrap().contains("assign"), "{out}");
}

#[tokio::test]
async fn not_in_subtree_refusal_reason_is_code() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp).await;
    r.add("lead", "t1", json!({})).await;
    let up = r
        .call(
            "sublead",
            json!({"type": "report", "id": "t1", "from": "lead", "state": "done"}),
        )
        .await;
    assert_shape(&up, "not_in_subtree");
    assert!(
        up["error"].as_str().unwrap().contains("descendants"),
        "{up}"
    );
}
