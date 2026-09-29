//! A SESSION loader (a `kernel_state` with a `root`, under a leg) persists a
//! federated kernel's records — a canvas and the thin agents nested under it.
//! Those records are never live on the host, so the record-owner walk and the path
//! resolution climb THIS LOADER'S persisted records (disk position) past the live
//! tree: nested 2+ levels is allowed for the owning leg and refused for a sibling,
//! and an existing record is placed where it is — never by the payload's parent.
//! The twin of python `kernel_state/tests/test_nested_records.py`.

use super::*;
use fantastic_kernel::with_sender;

/// A stand-in for the legs (python uses `file_bridge` agents): it only has to
/// exist and boot.
struct Leg;

#[async_trait]
impl Bundle for Leg {
    fn name(&self) -> &str {
        "leg"
    }
    async fn handle(
        &self,
        _id: &AgentId,
        _payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        Ok(None)
    }
}

const LEG: &str = "leg.tools";

fn booted_with_legs(tmp: &TempDir) -> Arc<Kernel> {
    let mut reg = BundleRegistry::new();
    reg.register(HANDLER_MODULE, KernelStateBundle::default());
    reg.register(LEG, Leg);
    bootstrap(reg, BootstrapOptions::daemon(tmp.path()))
        .expect("boot")
        .kernel
}

/// The session loader's root (python's `.fantastic/web`).
fn web_root(tmp: &TempDir) -> std::path::PathBuf {
    tmp.path().join(".fantastic/web")
}

async fn call(k: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    k.send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
        .result
}

async fn call_as(k: &Arc<Kernel>, who: &str, target: &str, payload: Value) -> Value {
    with_sender(
        AgentId::from(who),
        k.send(vec![Item::new(AgentId::from(target), payload)]),
    )
    .await
    .remove(0)
    .result
}

/// `wws` (the owning leg) with the session loader `web_loader` under it, and a
/// sibling leg `rest_leg` granted to call the loader.
async fn setup(k: &Arc<Kernel>, tmp: &TempDir) {
    let root = k.root().expect("root").id.0.clone();
    for rec in [
        // the loader's record verbs are privileged: the owning leg names them
        json!({"id": "wws", "handler_module": LEG, "parent_id": root,
               "can_call": {"web_loader": ["persist_record", "forget_record", "load_tree"]}}),
        json!({"id": "rest_leg", "handler_module": LEG, "parent_id": root,
               "can_call": {"web_loader": "*"}}),
        json!({"id": "web_loader", "handler_module": HANDLER_MODULE, "parent_id": "wws",
               "root": web_root(tmp).to_string_lossy(), "watch": false,
               "callable_by": {"rest_leg": "*"}}),
    ] {
        let mut p = json!({"type": "create_agent"});
        for (key, v) in rec.as_object().unwrap() {
            p[key] = v.clone();
        }
        let out = call(k, &root, p).await;
        assert_eq!(out["id"], rec["id"], "{out}");
    }
}

async fn persist(k: &Arc<Kernel>, who: &str, rid: &str, parent: &str) -> Value {
    call_as(
        k,
        who,
        "web_loader",
        json!({"type": "persist_record", "record": {"id": rid, "parent_id": parent}}),
    )
    .await
}

#[tokio::test]
async fn the_owning_leg_persists_records_nested_under_a_federated_record() {
    let tmp = TempDir::new().unwrap();
    let k = booted_with_legs(&tmp);
    setup(&k, &tmp).await;
    assert_eq!(
        persist(&k, "wws", "canvas", "web_loader").await,
        json!({"ok": true})
    );
    assert_eq!(
        persist(&k, "wws", "terminal_view", "canvas").await,
        json!({"ok": true})
    );
    assert_eq!(
        persist(&k, "wws", "deep", "terminal_view").await,
        json!({"ok": true})
    );
    assert!(
        web_root(&tmp)
            .join("agents/canvas/agents/terminal_view/agents/deep/agent.yaml")
            .is_file(),
        "a 2-level nested record lands under its real parents"
    );
    let tree = call(&k, "web_loader", json!({"type": "load_tree"})).await;
    let parents: HashMap<String, Value> = tree["records"]
        .as_array()
        .expect("records")
        .iter()
        .map(|r| {
            (
                r["id"].as_str().unwrap().to_string(),
                r["parent_id"].clone(),
            )
        })
        .collect();
    assert_eq!(parents["terminal_view"], json!("canvas"), "{tree}");
    assert_eq!(parents["deep"], json!("terminal_view"), "{tree}");
}

#[tokio::test]
async fn a_sibling_may_not_touch_the_nested_records() {
    let tmp = TempDir::new().unwrap();
    let k = booted_with_legs(&tmp);
    setup(&k, &tmp).await;
    persist(&k, "wws", "canvas", "web_loader").await;
    persist(&k, "wws", "terminal_view", "canvas").await;
    let r = persist(&k, "rest_leg", "sneak", "terminal_view").await;
    assert_eq!(r["reason"], json!("unauthorized"), "{r}");
    let r = call_as(
        &k,
        "rest_leg",
        "web_loader",
        json!({"type": "forget_record", "id": "terminal_view", "parent_id": "canvas"}),
    )
    .await;
    assert_eq!(r["reason"], json!("unauthorized"), "{r}");
    assert!(web_root(&tmp)
        .join("agents/canvas/agents/terminal_view/agent.yaml")
        .is_file());
}

/// Naming a record that exists with `parent_id` = yourself used to pass the guard
/// (the payload's parent was walked first) and overwrite it.
#[tokio::test]
async fn an_existing_record_is_placed_by_its_position_not_the_payload() {
    let tmp = TempDir::new().unwrap();
    let k = booted_with_legs(&tmp);
    setup(&k, &tmp).await;
    persist(&k, "wws", "canvas", "web_loader").await;
    persist(&k, "wws", "terminal_view", "canvas").await;
    let r = persist(&k, "rest_leg", "terminal_view", "rest_leg").await;
    assert_eq!(r["reason"], json!("unauthorized"), "{r}");
    assert!(!web_root(&tmp).join("agents/terminal_view").exists());
}

#[tokio::test]
async fn forget_record_removes_a_nested_record_where_it_is() {
    let tmp = TempDir::new().unwrap();
    let k = booted_with_legs(&tmp);
    setup(&k, &tmp).await;
    persist(&k, "wws", "canvas", "web_loader").await;
    persist(&k, "wws", "terminal_view", "canvas").await;
    persist(&k, "wws", "deep", "terminal_view").await;
    let r = call_as(
        &k,
        "wws",
        "web_loader",
        json!({"type": "forget_record", "id": "deep"}),
    )
    .await;
    assert_eq!(r, json!({"ok": true}));
    assert!(!web_root(&tmp)
        .join("agents/canvas/agents/terminal_view/agents/deep")
        .exists());
    assert!(web_root(&tmp)
        .join("agents/canvas/agents/terminal_view/agent.yaml")
        .is_file());
}

/// No fallback: a brand-new record's placement comes from the payload, and the
/// walk from there must still reach the caller.
#[tokio::test]
async fn an_id_in_neither_tree_nor_disk_is_placed_by_the_payload_and_guarded() {
    let tmp = TempDir::new().unwrap();
    let k = booted_with_legs(&tmp);
    setup(&k, &tmp).await;
    let r = persist(&k, "wws", "orphan", "nowhere").await;
    assert_eq!(r["reason"], json!("unauthorized"), "{r}");
}
