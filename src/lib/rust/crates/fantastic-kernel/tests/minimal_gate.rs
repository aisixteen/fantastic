//! THE GRANT MODEL (v9), pinned.
//!
//! - A grant on an agent reaches that agent and everything below it; agents on the
//!   same plane are granted one by one.
//! - A grant is a line in the CALLER's own record, in its one `can` field: a
//!   target and one level (`call`, `update` or `create`). The target keeps no list
//!   of callers, and nothing is stamped or inherited.
//! - Without a line an agent reaches only its own tree, by position; anything
//!   outside it needs a line, the parent included.
//! - A call passes only when the caller's lines allow it AND every ancestor whose
//!   tree does not hold the target allows it too (the cap).
//! - Every denial is the unknown answer, byte-identical to a missing id.

use async_trait::async_trait;
use fantastic_kernel::test_support::{grant, register, v9_tree};
use fantastic_kernel::{Agent, AgentId, BundleRegistry, Handler, Item, Kernel, Outcome, Reply};
use serde_json::{json, Map, Value};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

const MOD: &str = "minimal_gate_probe.tools";

/// Every boot a probe saw: (agent id, its origin).
static BOOTS: Mutex<Vec<(String, Option<String>)>> = Mutex::new(Vec::new());

/// Answers `{ok: <id>, verb}` for any of its verbs. A leg's `inbound_call` is sent
/// onward as the leg. At `boot`, a record carrying `boot_create` sends that
/// `create_agent` for its asker ([`Kernel::call`]: at a boot, the agent
/// itself); a record carrying `hydrate` creates `<id>_hydrated` under itself, as
/// itself (the static loader's roster pass).
struct Probe;

fantastic_kernel::stand_in_verbs!(probe_verbs {
    BootV "boot" {  },
    PingV "ping" {  },
    SendV "send" {  },
    GetV "get" {  },
    CompleteV "complete" {  },
    IssuesV "issues" {  },
    ReadV "read" {  },
    AppendV "append" {  },
    ShutdownV "shutdown" {  },
    InboundCallV "inbound_call" { item: serde_json::Value, sender: Option<String> },
    WriteOwnV "write_own" { fields: serde_json::Value },
});
#[async_trait]
impl Handler for Probe {
    type Call = probe_verbs::Call;
    fn man(&self) -> &'static str {
        "A test stand-in."
    }
    fn name(&self) -> &str {
        "minimal_gate_probe"
    }
    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        probe_verbs::VERBS
    }
    fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
        probe_verbs::dynamic_verbs(agent)
    }
    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        fantastic_kernel::fixture_record!()
    }
    async fn handle(
        &self,
        id: &AgentId,
        call: Self::Call,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, Box<dyn std::error::Error + Send + Sync>> {
        let payload = &call.payload();
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        Ok(match verb {
            "boot" => {
                let origin = fantastic_kernel::current_origin();
                BOOTS
                    .lock()
                    .unwrap()
                    .push((id.0.clone(), origin.as_ref().map(|o| o.0.clone())));
                let meta = kernel
                    .resolve_target(id.as_str())
                    .map(|a| a.record().meta)
                    .unwrap_or_default();
                if meta.contains_key("hydrate") {
                    let kid = format!("{}_hydrated", id.0);
                    let _ = fantastic_kernel::test_support::settled(kernel, vec![Item::new(
                            "kernel",
                            json!({"type": "create_agent", "parent_id": id.0,
                                   "handler_module": MOD, "id": kid}),
                        )])
                        .await;
                }
                if let Some(c) = meta.get("boot_create") {
                    let _ = kernel
                        .call(Item::new("kernel", c.clone()), |_| async {})
                        .await;
                }
                None
            }
            "shutdown" => None,
            // the bundle's own code writing its own record: an `update_agent` it
            // sends as itself, exactly what a model's or a page's call would send
            "write_own" => {
                let mut p = json!({"type": "update_agent", "id": id.as_str()});
                for (k, v) in payload["fields"].as_object().cloned().unwrap_or_default() {
                    p[k] = v;
                }
                Some(fantastic_kernel::test_support::settled(kernel, vec![Item::new("kernel", p)]).await.remove(0).result)
            }
            "inbound_call" => {
                // the frame's `sender` is never an identity: the leg sends as itself
                let item = &payload["item"];
                let target = item["target"].as_str().unwrap_or("");
                let out = fantastic_kernel::test_support::settled(kernel, vec![Item::new(target, item["payload"].clone())])
                    .await
                    .remove(0);
                Some(out.result)
            }
            _ => Some(json!({"ok": id.as_str(), "verb": verb})),
        })
    }
}

/// The bundle names the v9 tree uses, and the extra ones these tests create.
const BUNDLES: [&str; 26] = [
    MOD,
    "kernel",
    "ledger",
    "user",
    "store",
    "file",
    "memory",
    "credentials",
    "exec",
    "home",
    "term",
    "web",
    "ws",
    "page",
    "mcp",
    "key_user",
    "canvas",
    "loader",
    "chat",
    "anthropic_backend",
    "exec_bridge",
    "file_bridge",
    "team",
    "tasks",
    "lead",
    "worker",
];

fn registry() -> BundleRegistry {
    let mut reg = BundleRegistry::new();
    for b in BUNDLES {
        reg.register(b, Probe);
    }
    reg.register("static_probe", Probe);
    reg.register("boot_maker", Probe);
    reg
}

/// A kernel whose root `kernel_state` holds no lines.
fn mk() -> (TempDir, Arc<Kernel>) {
    let tmp = TempDir::new().unwrap();
    let mut kernel = Kernel::new();
    kernel.bundles = registry();
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    register(&kernel, Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    (tmp, kernel)
}

/// A kernel holding the v9 initial tree (`test_support::v9_tree`).
fn mk_v9() -> Arc<Kernel> {
    let mut kernel = Kernel::new();
    kernel.bundles = registry();
    let kernel = Arc::new(kernel);
    v9_tree(&kernel);
    kernel
}

async fn op(kernel: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    fantastic_kernel::test_support::send_settled(kernel, vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
        .result
}

async fn as_agent(kernel: &Arc<Kernel>, sender: &str, target: &str, payload: Value) -> Outcome {
    let k = Arc::clone(kernel);
    let t = target.to_string();
    fantastic_kernel::test_support::as_agent(AgentId::from(sender), async move {
        fantastic_kernel::test_support::send_settled(&k, vec![Item::new(AgentId::from(t.as_str()), payload)])
            .await
            .remove(0)
    })
    .await
}

/// `create_agent` of `bundle` sent by `sender` to the kernel.
async fn create_as(kernel: &Arc<Kernel>, sender: &str, parent: &str, id: &str, bundle: &str) -> Value {
    as_agent(
        kernel,
        sender,
        "kernel",
        json!({"type": "create_agent", "parent_id": parent, "handler_module": bundle, "id": id}),
    )
    .await
    .result
}

async fn create(kernel: &Arc<Kernel>, parent: &str, id: &str, meta: Value) {
    let mut p =
        json!({"type": "create_agent", "parent_id": parent, "handler_module": MOD, "id": id});
    for (k, v) in meta.as_object().cloned().unwrap_or_default() {
        p[k] = v;
    }
    let r = op(kernel, "kernel", p).await;
    assert_eq!(r["id"], json!(id), "{r}");
}

/// The unknown answer for `id`.
fn unknown(id: &str) -> Value {
    fantastic_kernel::unknown_agent(id)
}

/// The refusal of an act on a target the caller sees: `not_allowed` naming
/// `arg`, with a hint and no side or ids (#783).
fn names(r: &Value, arg: &str) -> bool {
    r["reason"] == "not_allowed"
        && r["arg"] == arg
        && r["hint"].is_string()
        && r.get("side").is_none()
        && r.get("ids").is_none()
}

/// The call reached the target (its handler answered).
fn reached(out: &Outcome, id: &str) -> bool {
    out.result.get("ok").and_then(Value::as_str) == Some(id)
}

/// Drop `holder`'s line on `target`, as the operator.
fn drop_line(kernel: &Arc<Kernel>, holder: &str, target: &str) {
    fantastic_kernel::test_support::revoke(kernel, holder, target);
}

#[tokio::test]
async fn a_new_agent_gets_no_stamped_line_and_calls_its_own_tree() {
    let (_t, k) = mk();
    create(&k, "kernel_state", "a", json!({})).await;
    create(&k, "a", "a_kid", json!({})).await;
    create(&k, "kernel_state", "b", json!({})).await;
    let rec = k.resolve_target("a").unwrap().record().meta;
    assert!(rec.get("can").is_none(), "nothing is stamped: {rec:?}");
    // its own tree, itself included, by position
    assert!(reached(&as_agent(&k, "a", "a", json!({"type": "ping"})).await, "a"));
    assert!(reached(&as_agent(&k, "a", "a_kid", json!({"type": "ping"})).await, "a_kid"));
    // anything outside it needs a line, the parent included
    assert_eq!(as_agent(&k, "a", "b", json!({"type": "ping"})).await.result, unknown("b"));
    assert_eq!(
        as_agent(&k, "a_kid", "a", json!({"type": "ping"})).await.result,
        unknown("a")
    );
}

#[tokio::test]
async fn one_line_in_the_callers_can_is_the_whole_edge() {
    let (_t, k) = mk();
    create(&k, "kernel_state", "a", json!({"can": {"b": {"call": ["ping"]}}})).await;
    create(&k, "kernel_state", "b", json!({})).await;
    // the caller's line alone: b keeps no list of callers
    let out = as_agent(&k, "a", "b", json!({"type": "ping"})).await;
    assert!(reached(&out, "b"), "{}", out.result);
    assert!(k.resolve_target("b").unwrap().record().meta.get("can").is_none());
    // a verb the line does not name: a sees b, so the verb is named, no `side`
    let out = as_agent(&k, "a", "b", json!({"type": "send"})).await;
    assert!(names(&out.result, "send"), "{}", out.result);
    // the other way: b holds no line on a
    let out = as_agent(&k, "b", "a", json!({"type": "ping"})).await;
    assert_eq!(out.result, unknown("a"));
}

#[tokio::test]
async fn a_send_with_no_origin_scope_is_refused() {
    let (_t, k) = mk();
    create(&k, "kernel_state", "a", json!({})).await;
    let k2 = Arc::clone(&k);
    // a bare spawn carries no identity at all, and is never the operator
    let out = tokio::spawn(async move {
        fantastic_kernel::test_support::settled(&k2, vec![Item::new("a", json!({"type": "ping"}))])
            .await
            .remove(0)
    })
    .await
    .unwrap();
    assert_eq!(out.result, unknown("a"));
}

#[tokio::test]
async fn operator_send_is_ungated() {
    let (_t, k) = mk();
    create(&k, "kernel_state", "a", json!({})).await;
    let out = fantastic_kernel::test_support::send_settled(&k, vec![Item::new("a", json!({"type": "ping"}))])
        .await
        .remove(0);
    assert!(reached(&out, "a"), "{}", out.result);
}

#[tokio::test]
async fn a_frame_handed_to_a_leg_runs_with_the_legs_lines_only() {
    let (_t, k) = mk();
    create(&k, "kernel_state", "leg", json!({"can": {"b": {"call": ["ping"]}}})).await;
    create(&k, "kernel_state", "a", json!({"can": {"c": {"call": ["ping"]}}})).await;
    create(&k, "kernel_state", "b", json!({})).await;
    create(&k, "kernel_state", "c", json!({})).await;
    let hand_in = |target: &str, sender: Option<&str>| {
        let mut p = json!({"type": "inbound_call",
                           "item": {"target": target, "payload": {"type": "ping"}}});
        if let Some(s) = sender {
            p["sender"] = json!(s);
        }
        p
    };
    // the operator hands a frame to its leg: the leg sends onward as itself
    let r = op(&k, "leg", hand_in("b", None)).await;
    assert_eq!(r["ok"], "b", "{r}");
    let r = op(&k, "leg", hand_in("c", None)).await;
    assert_eq!(r, unknown("c"));
    // a frame naming another sender gets no more than the leg's lines
    let r = op(&k, "leg", hand_in("c", Some("a"))).await;
    assert_eq!(r, unknown("c"));
}

#[tokio::test]
async fn a_kernel_verb_sent_to_another_agent_is_not_here_only_after_the_gate() {
    let (_t, k) = mk();
    create(&k, "kernel_state", "a", json!({})).await;
    create(&k, "kernel_state", "b", json!({})).await;
    let ps = json!({"type": "ps"});
    // no line on b: the unknown answer, as for a missing id
    assert_eq!(as_agent(&k, "a", "b", ps.clone()).await.result, unknown("b"));
    // holding the verb on b, the gate passes and the kernel verb is not answered there
    grant(&k, "a", "b", json!({"call": ["ps"]}));
    let out = as_agent(&k, "a", "b", ps.clone()).await;
    assert_eq!(out.result["reason"], "not_here", "{}", out.result);
    // the operator is ungated, and still told the same
    assert_eq!(op(&k, "b", ps).await["reason"], "not_here");
}

#[tokio::test]
async fn only_the_originator_joins_an_execution() {
    let (_t, k) = mk();
    create(&k, "kernel_state", "a", json!({})).await;
    create(&k, "kernel_state", "b", json!({"can": {"a": {"call": ["ping"]}}})).await;
    let named = |k: Arc<Kernel>, sender: &'static str| async move {
        fantastic_kernel::test_support::as_agent(AgentId::from(sender), async move {
            fantastic_kernel::test_support::send_settled(&k, vec![Item::new("a", json!({"type": "ping"})).with_id("job")])
                .await
                .remove(0)
        })
        .await
    };
    let first = named(Arc::clone(&k), "a").await;
    assert!(reached(&first, "a"), "{}", first.result);
    // another caller naming the same execution, though it holds the verb on a
    let other = named(Arc::clone(&k), "b").await;
    assert_eq!(other.result, unknown("a"));
    // its originator joins it, and so does the operator
    let again = named(Arc::clone(&k), "a").await;
    assert_eq!(again.result, first.result);
    let by_op = fantastic_kernel::test_support::send_settled(&k, vec![Item::new("a", json!({"type": "ping"})).with_id("job")])
        .await
        .remove(0);
    assert_eq!(by_op.result, first.result);
}

#[tokio::test]
async fn ps_needs_no_line_and_answers_own_tasks_and_those_below() {
    let (_t, k) = mk();
    create(&k, "kernel_state", "a", json!({})).await;
    let out = as_agent(&k, "a", "kernel", json!({"type": "ps"})).await;
    let rows = out.result["tasks"].as_array().cloned().unwrap_or_else(|| {
        panic!("ps answers without a line: {}", out.result);
    });
    // its own ps dispatch runs on the root, outside its tree: not shown
    assert!(rows.iter().all(|r| r["agent"] == "a"), "{rows:?}");
}

#[tokio::test]
async fn a_create_line_on_itself_creates_under_itself() {
    let (_t, k) = mk();
    create(
        &k,
        "kernel_state",
        "a",
        json!({"can": {"a": {"create": {"who": ["*"], "what": {MOD: {}}}}}}),
    )
    .await;
    create(&k, "kernel_state", "b", json!({"can": {"kernel_state": {"call": ["*"]}}})).await;
    let r = create_as(&k, "a", "a", "kid", MOD).await;
    assert_eq!(r["id"], "kid", "{r}");
    assert_eq!(k.resolve_target("kid").unwrap().parent_id, Some(AgentId::from("a")));
    // `call *` on the root creates nothing (b sees the root: the bundle is named)
    let r = create_as(&k, "b", "kernel_state", "nope", MOD).await;
    assert!(names(&r, "handler_module"), "{r}");
    assert!(!k.exists("nope"));
}

#[tokio::test]
async fn create_star_on_the_root_never_reaches_self_or_ancestors() {
    let (_t, k) = mk();
    create(
        &k,
        "kernel_state",
        "a",
        json!({"can": {"kernel_state": {"create": {"who": ["*"], "what": "*"}}}}),
    )
    .await;
    create(&k, "kernel_state", "other", json!({})).await;
    // anywhere in the root's tree
    let r = create_as(&k, "a", "other", "x", MOD).await;
    assert_eq!(r["id"], "x", "{r}");
    let upd = |id: &str, field: &str| json!({"type": "update_agent", "id": id, field: {"x": {"call": ["ping"]}}});
    // never upward: its own `can`, the root, its own delete
    let r = as_agent(&k, "a", "kernel", upd("a", "can")).await.result;
    assert!(names(&r, "can"), "{r}");
    let r = as_agent(&k, "a", "kernel", json!({"type": "update_agent", "id": "kernel_state", "description": "x"}))
        .await
        .result;
    assert!(names(&r, "description"), "{r}");
    for id in ["a", "kernel_state"] {
        let r = as_agent(&k, "a", "kernel", json!({"type": "delete_agent", "id": id})).await.result;
        assert!(names(&r, "delete"), "{id}: {r}");
        assert_eq!(r["deleted"], false, "{id}: {r}");
    }
    // a sibling it selects goes
    let r = as_agent(&k, "a", "kernel", json!({"type": "delete_agent", "id": "other"})).await.result;
    assert_eq!(r["deleted"], true, "{r}");
}

#[tokio::test]
async fn a_missing_operand_or_an_unknown_id_gets_the_verbs_own_refusal() {
    let (_t, k) = mk();
    create(
        &k,
        "kernel_state",
        "a",
        json!({"can": {"kernel_state": {"create": {"who": ["*"], "what": "*"}}}}),
    )
    .await;
    // a missing operand is `missing_arg`, a non-string one `bad_arg`: the kernel's
    // check of the verb's declaration, by argument name, before the verb runs
    for (payload, verb, arg, reason) in [
        (
            json!({"type": "create_agent", "handler_module": MOD}),
            "create_agent",
            "parent_id",
            "missing_arg",
        ),
        (
            json!({"type": "create_agent", "parent_id": 7, "handler_module": MOD}),
            "create_agent",
            "parent_id",
            "bad_arg",
        ),
        (
            json!({"type": "update_agent", "description": "x"}),
            "update_agent",
            "id",
            "missing_arg",
        ),
        (
            json!({"type": "update_agent", "id": 7}),
            "update_agent",
            "id",
            "bad_arg",
        ),
        (
            json!({"type": "delete_agent"}),
            "delete_agent",
            "id",
            "missing_arg",
        ),
        (
            json!({"type": "delete_agent", "id": ""}),
            "delete_agent",
            "id",
            "bad_arg",
        ),
    ] {
        let out = as_agent(&k, "a", "kernel", payload.clone()).await;
        assert_eq!(out.result["reason"], reason, "{payload}: {}", out.result);
        assert_eq!(out.result["arg"], arg, "{payload}: {}", out.result);
        assert!(
            out.result["error"]
                .as_str()
                .is_some_and(|e| e.starts_with(&format!("{verb}: "))),
            "{payload}: {}",
            out.result
        );
    }
    // an id naming no agent: the verb's own answer, never an authorization refusal
    for verb in ["update_agent", "delete_agent"] {
        let out = as_agent(&k, "a", "kernel", json!({"type": verb, "id": "ghost"})).await;
        assert!(
            out.result["reason"] != "unauthorized" && out.result.get("side").is_none(),
            "{verb}: {}",
            out.result
        );
    }
}

#[tokio::test]
async fn a_peer_target_in_can_is_refused_at_the_write() {
    // peers are parked: no line names a `<guid>|…` target
    let (_t, k) = mk();
    for key in ["g1|ai", "g1|*"] {
        let r = op(
            &k,
            "kernel",
            json!({"type": "create_agent", "parent_id": "kernel_state", "handler_module": MOD,
                   "id": "z", "can": {key: {"call": ["*"]}}}),
        )
        .await;
        assert_eq!(r["reason"], "invalid_meta", "{key}: {r}");
    }
    assert!(!k.exists("z"));
}

#[tokio::test]
async fn a_can_line_outside_the_legal_shape_is_refused_at_the_write() {
    let (_t, k) = mk();
    for can in [
        json!({"*": {"call": ["x"]}}),
        json!({"$self": {"call": ["x"]}}),
        json!({"$subtree": {"call": ["x"]}}),
        json!({"a": {"call": ["x"], "update": ["y"]}}),
        json!({"a": {"grant": ["x"]}}),
        json!({"a": ["x"]}),
        json!({"a": "*"}),
        json!({"$parent": {"update": ["x"]}}),
        json!({"a": {"create": {"who": ["a*b*"], "what": "*"}}}),
        json!({"a": {"create": {"who": ["*"]}}}),
        json!(["a"]),
    ] {
        let r = op(
            &k,
            "kernel",
            json!({"type": "create_agent", "parent_id": "kernel_state", "handler_module": MOD,
                   "id": "z", "can": can.clone()}),
        )
        .await;
        assert!(
            r["reason"] == "invalid_meta" || r["reason"] == "bad_arg",
            "{can}: {r}"
        );
        assert!(!k.exists("z"), "{can}");
    }
    // and the same at an update
    create(&k, "kernel_state", "a", json!({})).await;
    let r = op(
        &k,
        "kernel",
        json!({"type": "update_agent", "id": "a", "can": {"$self": {"call": ["x"]}}}),
    )
    .await;
    assert_eq!(r["reason"], "invalid_meta", "{r}");
}

#[tokio::test]
async fn chat_never_updates_its_own_can() {
    let k = mk_v9();
    let r = as_agent(
        &k,
        "chat_a",
        "kernel",
        json!({"type": "update_agent", "id": "chat_a", "can": {"credentials": {"call": ["get"]}}}),
    )
    .await
    .result;
    assert!(names(&r, "can"), "{r}");
    assert!(k.resolve_target("chat_a").unwrap().record().meta["can"]
        .get("credentials")
        .is_none());
}

#[tokio::test]
async fn chat_never_updates_or_deletes_canvas_root() {
    let k = mk_v9();
    let r = as_agent(
        &k,
        "chat_a",
        "kernel",
        json!({"type": "update_agent", "id": "canvas_root", "description": "x"}),
    )
    .await
    .result;
    assert!(names(&r, "description"), "{r}");
    let r = as_agent(&k, "chat_a", "kernel", json!({"type": "delete_agent", "id": "canvas_root"}))
        .await
        .result;
    assert!(names(&r, "delete"), "{r}");
    assert_eq!(r["deleted"], false);
    assert!(k.exists("canvas_root"));
}

#[tokio::test]
async fn siblings_reach_each_other_only_by_a_line() {
    let k = mk_v9();
    let ping = json!({"type": "ping"});
    assert_eq!(as_agent(&k, "t1_w1", "t1_w2", ping.clone()).await.result, unknown("t1_w2"));
    grant(&k, "t1_w1", "t1_w2", json!({"call": ["ping"]}));
    assert!(reached(&as_agent(&k, "t1_w1", "t1_w2", ping.clone()).await, "t1_w2"));
    // the line is one way
    assert_eq!(as_agent(&k, "t1_w2", "t1_w1", ping).await.result, unknown("t1_w1"));
}

#[tokio::test]
async fn parent_line_gives_send_on_the_parent_and_no_create() {
    let k = mk_v9();
    assert!(reached(
        &as_agent(&k, "t1_w1", "t1_lead", json!({"type": "send"})).await,
        "t1_lead"
    ));
    let r = as_agent(&k, "t1_w1", "t1_lead", json!({"type": "ping"})).await.result;
    assert!(names(&r, "ping"), "{r}");
    // nothing on the grandparent
    assert_eq!(
        as_agent(&k, "t1_w1", "team_1", json!({"type": "send"})).await.result,
        unknown("team_1")
    );
    let r = create_as(&k, "t1_w1", "t1_lead", "t1_w9", "worker").await;
    assert!(names(&r, "handler_module"), "{r}");
    assert!(!k.exists("t1_w9"));
}

#[tokio::test]
async fn a_create_line_selects_by_who_and_what() {
    let k = mk_v9();
    let r = create_as(&k, "t1_lead", "t1_lead", "t1_w3", "worker").await;
    assert_eq!(r["id"], "t1_w3", "{r}");
    // an id outside `who`
    let r = create_as(&k, "t1_lead", "t1_lead", "x_w3", "worker").await;
    assert!(names(&r, "id"), "{r}");
    // a bundle outside `what`
    let r = create_as(&k, "t1_lead", "t1_lead", "t1_w4", "exec_bridge").await;
    assert!(names(&r, "handler_module"), "{r}");
    assert!(!k.exists("x_w3") && !k.exists("t1_w4"));
    // allowed once the line's `what` names it
    grant(
        &k,
        "t1_lead",
        "t1_lead",
        json!({"create": {"who": ["t1_w*"], "what": {"worker": {}, "exec_bridge": {}}}}),
    );
    let r = create_as(&k, "t1_lead", "t1_lead", "t1_w4", "exec_bridge").await;
    assert_eq!(r["id"], "t1_w4", "{r}");
}

#[tokio::test]
async fn a_line_outside_the_tree_is_capped_by_every_ancestor() {
    let k = mk_v9();
    let issues = json!({"type": "issues"});
    // the lead's line on github, with team_1's and canvas_root's
    assert!(reached(&as_agent(&k, "t1_lead", "github", issues.clone()).await, "github"));
    // a worker with no line of its own
    assert_eq!(as_agent(&k, "t1_w1", "github", issues).await.result, unknown("github"));
    // a worker line its ancestors do not hold: capped
    grant(&k, "t1_w1", "credentials", json!({"call": ["get"]}));
    assert_eq!(
        as_agent(&k, "t1_w1", "credentials", json!({"type": "get"})).await.result,
        unknown("credentials")
    );
    assert!(!k.may_call(&AgentId::from("t1_w1"), "get", &AgentId::from("credentials")));
}

#[tokio::test]
async fn an_ancestor_losing_a_line_cuts_every_descendant() {
    let k = mk_v9();
    let lead = AgentId::from("t1_lead");
    let github = AgentId::from("github");
    assert!(k.may_call(&lead, "issues", &github));
    drop_line(&k, "canvas_root", "github");
    // the lead's own line is unchanged; the cap cuts it
    assert!(!k.may_call(&lead, "issues", &github));
    assert_eq!(
        as_agent(&k, "t1_lead", "github", json!({"type": "issues"})).await.result,
        unknown("github")
    );
    assert!(!k.may_call(&AgentId::from("team_1"), "issues", &github));
}

#[tokio::test]
async fn providers_use_the_key_and_chats_never_get_it() {
    let k = mk_v9();
    let get = json!({"type": "get"});
    assert!(reached(&as_agent(&k, "prov_1", "credentials", get.clone()).await, "credentials"));
    assert_eq!(as_agent(&k, "chat_a", "credentials", get.clone()).await.result, unknown("credentials"));
    // a provider made in the garage gets its key line from the garage's create line
    // (forced `can`), whatever its creator asks for
    let r = as_agent(
        &k,
        "canvas_garage",
        "kernel",
        json!({"type": "create_agent", "parent_id": "canvas_garage",
               "handler_module": "anthropic_backend", "id": "prov_2", "model": "m",
               "can": {"credentials": {"call": ["get", "set"]}}}),
    )
    .await
    .result;
    assert_eq!(r["id"], "prov_2", "{r}");
    assert_eq!(
        k.resolve_target("prov_2").unwrap().record().meta["can"],
        json!({"credentials": {"call": ["get"]}})
    );
    assert!(reached(&as_agent(&k, "prov_2", "credentials", get.clone()).await, "credentials"));
    // a chat made in the same garage gets no key line
    let r = create_as(&k, "canvas_garage", "canvas_garage", "chat_g", "chat").await;
    assert_eq!(r["id"], "chat_g", "{r}");
    assert_eq!(as_agent(&k, "chat_g", "credentials", get).await.result, unknown("credentials"));
}

#[tokio::test]
async fn the_page_leg_creates_in_the_user_space_under_webs_cap() {
    let k = mk_v9();
    // h: the page leg creates a cell under canvas_root, and in the garage
    let r = create_as(&k, "canvas_ws", "canvas_root", "cell_1", "chat").await;
    assert_eq!(r["id"], "cell_1", "{r}");
    let r = create_as(&k, "canvas_ws", "canvas_garage", "cell_2", "chat").await;
    assert_eq!(r["id"], "cell_2", "{r}");
    // never outside the user space
    assert_eq!(create_as(&k, "canvas_ws", "user", "cell_3", "chat").await, unknown("user"));
    // h': web loses its line; the leg's own line is unchanged and the cap cuts it
    drop_line(&k, "web", "canvas_root");
    assert_eq!(
        create_as(&k, "canvas_ws", "canvas_root", "cell_4", "chat").await,
        unknown("canvas_root")
    );
    assert!(!k.exists("cell_4"));
}

/// Review of #761, H1, in the shipped tree: a page leg (`canvas_ws`, `mcp`) holds
/// `create *` on `canvas_root`, which writes the `can` of what it creates there.
/// An `update` line includes calling every verb, so an empty `update` on a system
/// helper would hand out `credentials.get` or `memory.read` the leg never holds.
#[tokio::test]
async fn a_leg_cannot_hand_out_call_all_through_an_empty_update_line() {
    let k = mk_v9();
    for leg in ["canvas_ws", "mcp"] {
        for helper in ["credentials", "memory"] {
            let line = json!({helper: {"update": []}});
            // at create
            let id = format!("x_{leg}_{helper}");
            let r = as_agent(&k, leg, "kernel", json!({"type": "create_agent",
                "parent_id": "canvas_root", "handler_module": "chat", "id": id,
                "can": line.clone()}))
            .await
            .result;
            assert_eq!(r, unknown(helper), "{leg} creates with {line}");
            assert!(!k.exists(&id), "{id}");
            // on an agent it may already write the `can` of
            let cell = format!("cell_{leg}_{helper}");
            let r = create_as(&k, leg, "canvas_root", &cell, "chat").await;
            assert_eq!(r["id"], json!(cell), "{r}");
            let r = as_agent(&k, leg, "kernel",
                json!({"type": "update_agent", "id": cell, "can": line.clone()}))
            .await
            .result;
            assert_eq!(r, unknown(helper), "{leg} gives {cell} {line}");
            let rec = k.resolve_target(&cell).unwrap().record().meta;
            assert!(rec.get("can").is_none(), "{cell}: {rec:?}");
            for verb in ["get", "read", "append"] {
                assert!(!k.may_call(&AgentId::from(cell.as_str()), verb, &AgentId::from(helper)));
            }
        }
    }
}

#[tokio::test]
async fn a_create_line_deletes_only_selected_agents() {
    let k = mk_v9();
    let del = |id: &str| json!({"type": "delete_agent", "id": id});
    // not selected: bundle outside `what`, id outside `who`, the holder, an
    // ancestor. One it sees is named back (`delete`); one it does not is unknown.
    let lead = AgentId::from("t1_lead");
    let mut seen = 0;
    for id in ["t1_tasks", "t1_exec", "t1_lead", "team_1"] {
        let r = as_agent(&k, "t1_lead", "kernel", del(id)).await.result;
        assert_eq!(r["deleted"], false, "{id}: {r}");
        if k.holds(Some(&lead), fantastic_kernel::Held::Call, &AgentId::from(id)) {
            seen += 1;
            assert!(names(&r, "delete"), "{id}: {r}");
        } else {
            let mut u = unknown(id);
            u["deleted"] = json!(false);
            assert_eq!(r, u, "{id}");
        }
        assert!(k.exists(id), "{id}");
    }
    assert!(seen > 0, "the holder sees itself");
    // a sibling worker holds no create line
    let r = as_agent(&k, "t1_w1", "kernel", del("t1_w2")).await.result;
    assert_eq!(r["deleted"], false, "{r}");
    // selected
    let r = as_agent(&k, "t1_lead", "kernel", del("t1_w2")).await.result;
    assert_eq!(r["deleted"], true, "{r}");
    assert!(!k.exists("t1_w2"));
}

#[tokio::test]
async fn delete_needs_the_right_on_the_target_only_and_a_locked_descendant_refuses() {
    let k = mk_v9();
    // a child of a bundle the lead's line does not select goes with its parent
    create(&k, "t1_w1", "w1_tool", json!({})).await;
    let r = as_agent(&k, "t1_lead", "kernel", json!({"type": "delete_agent", "id": "t1_w1"}))
        .await
        .result;
    assert_eq!(r["deleted"], true, "{r}");
    assert!(!k.exists("w1_tool"));
    // a locked descendant refuses the whole delete: a lock, not a right
    create(&k, "t1_w2", "w2_tool", json!({"delete_lock": true})).await;
    let r = as_agent(&k, "t1_lead", "kernel", json!({"type": "delete_agent", "id": "t1_w2"}))
        .await
        .result;
    assert_eq!(r["reason"], "locked", "{r}");
    assert!(k.exists("t1_w2") && k.exists("w2_tool"));
}

#[tokio::test]
async fn a_bare_agent_is_created_and_deleted_by_the_operator_only() {
    let k = mk_v9();
    // no handler: refused by the declaration, for every sender
    let bare = json!({"type": "create_agent", "parent_id": "t1_lead", "handler_module": "",
                      "id": "t1_wbare"});
    let r = as_agent(&k, "t1_lead", "kernel", bare.clone()).await.result;
    assert!(r.get("error").is_some(), "{r}");
    assert!(!k.exists("t1_wbare"));
    // a bare agent the operator placed: the lead's line never covers it
    let a = Agent::new(
        AgentId::from("t1_wbare"),
        None,
        Some(AgentId::from("t1_lead")),
        Map::new(),
        std::path::PathBuf::new(),
        true,
    );
    fantastic_kernel::test_support::adopt(&k, a);
    let r = as_agent(&k, "t1_lead", "kernel", json!({"type": "delete_agent", "id": "t1_wbare"}))
        .await
        .result;
    assert_eq!(r["deleted"], false, "{r}");
    let r = op(&k, "kernel", json!({"type": "delete_agent", "id": "t1_wbare"})).await;
    assert_eq!(r["deleted"], true, "{r}");
}

/// ONCE LOADED, A BUNDLE IS AN AGENT LIKE ANY OTHER (owner, 2026-11-24): every boot
/// runs with the agent ITSELF as its origin — inside `create_agent`, whoever the
/// creator, and after a restart ([`Kernel::announce`], as `kernel_start` and the
/// shell boot) — so a hook has the same authority whenever it runs: the agent's own
/// lines. A loader with no create line on itself builds nothing, even when the
/// operator made it; one whose record holds the line builds its roster at create
/// and again after load.
#[tokio::test]
async fn every_boot_runs_as_the_agent_itself() {
    let k = mk_v9();
    grant(
        &k,
        "canvas_root",
        "canvas_root",
        json!({"create": {"who": ["*"], "what": {"static_probe": {"hydrate": "*"}}}}),
    );
    let r = as_agent(
        &k,
        "canvas_root",
        "kernel",
        json!({"type": "create_agent", "parent_id": "canvas_root",
               "handler_module": "static_probe", "id": "sl_1", "hydrate": true}),
    )
    .await
    .result;
    assert_eq!(r["id"], "sl_1", "{r}");
    // the operator's create: no more power to its boot than its record holds
    let r = op(
        &k,
        "kernel",
        json!({"type": "create_agent", "parent_id": "canvas_root",
               "handler_module": "static_probe", "id": "sl_2", "hydrate": true}),
    )
    .await;
    assert_eq!(r["id"], "sl_2", "{r}");
    // a record holding a create line on itself
    let r = op(
        &k,
        "kernel",
        json!({"type": "create_agent", "parent_id": "canvas_root",
               "handler_module": "static_probe", "id": "sl_3", "hydrate": true,
               "can": {"sl_3": {"create": {"who": ["*"], "what": "*"}}}}),
    )
    .await;
    assert_eq!(r["id"], "sl_3", "{r}");
    for id in ["sl_1", "sl_2", "sl_3"] {
        assert!(BOOTS.lock().unwrap().contains(&(id.to_string(), Some(id.to_string()))), "{id}");
    }
    assert!(!k.exists("sl_1_hydrated"));
    assert!(!k.exists("sl_2_hydrated"));
    assert!(k.exists("sl_3_hydrated"));
    // after load: the same pass
    let r = op(&k, "kernel", json!({"type": "delete_agent", "id": "sl_3_hydrated"})).await;
    assert_eq!(r["deleted"], true, "{r}");
    for id in ["sl_1", "sl_2", "sl_3"] {
        let a = k.resolve_target(id).expect(id);
        k.announce(&a, json!({"type": "boot"})).await.expect("boot declared");
    }
    assert!(!k.exists("sl_1_hydrated"));
    assert!(!k.exists("sl_2_hydrated"));
    assert!(k.exists("sl_3_hydrated"));
}

#[tokio::test]
async fn a_create_inside_a_boot_hook_is_checked_as_the_agent_itself() {
    let k = mk_v9();
    grant(
        &k,
        "canvas_root",
        "canvas_root",
        json!({"create": {"who": ["*"], "what": {"boot_maker": {"boot_create": "*"}, "worker": {}}}}),
    );
    let inner = |parent: &str, id: &str| {
        json!({"type": "create_agent", "parent_id": parent, "handler_module": "worker", "id": id})
    };
    // canvas_root may create a worker there, yet the boot's create is bm_1's own
    let r = as_agent(
        &k,
        "canvas_root",
        "kernel",
        json!({"type": "create_agent", "parent_id": "canvas_root", "handler_module": "boot_maker",
               "id": "bm_1", "boot_create": inner("bm_1", "bm_1_kid")}),
    )
    .await
    .result;
    assert_eq!(r["id"], "bm_1", "{r}");
    assert!(!k.exists("bm_1_kid"), "the boot hook created with its creator's lines");
    // the operator's create: the same
    let r = op(
        &k,
        "kernel",
        json!({"type": "create_agent", "parent_id": "canvas_root", "handler_module": "boot_maker",
               "id": "bm_2", "boot_create": inner("bm_2", "bm_2_kid")}),
    )
    .await;
    assert_eq!(r["id"], "bm_2", "{r}");
    assert!(!k.exists("bm_2_kid"), "the boot hook created as the operator");
    // its own line: created
    let r = op(
        &k,
        "kernel",
        json!({"type": "create_agent", "parent_id": "canvas_root", "handler_module": "boot_maker",
               "id": "bm_3", "boot_create": inner("bm_3", "bm_3_kid"),
               "can": {"bm_3": {"create": {"who": ["*"], "what": {"worker": {}}}}}}),
    )
    .await;
    assert_eq!(r["id"], "bm_3", "{r}");
    assert!(k.exists("bm_3_kid"));
}

#[tokio::test]
async fn only_key_users_reach_credentials_get() {
    let k = mk_v9();
    let creds = AgentId::from("credentials");
    let reach: Vec<String> = k
        .agent_ids()
        .into_iter()
        .filter(|a| a != &creds && k.may_call(a, "get", &creds))
        .map(|a| a.0)
        .collect();
    // the root and `user` by position; the key users and their caps by their lines
    let mut want = vec!["kernel_state", "user", "github", "canvas_root", "canvas_garage", "prov_1"];
    want.sort_unstable();
    let mut got: Vec<&str> = reach.iter().map(String::as_str).collect();
    got.sort_unstable();
    assert_eq!(got, want);
}

#[tokio::test]
async fn no_leg_line_reaches_the_system_helpers() {
    let k = mk_v9();
    for leg in ["web", "canvas_ws", "canvas_page", "mcp"] {
        for helper in ["credentials", "memory", "store", "exec", "home", "term", "kernel_memory"] {
            for verb in ["get", "set", "read", "write", "append", "run"] {
                assert!(
                    !k.may_call(&AgentId::from(leg), verb, &AgentId::from(helper)),
                    "{leg} -> {helper}.{verb}"
                );
            }
            let r = as_agent(&k, leg, helper, json!({"type": "reflect"})).await.result;
            assert_eq!(r, unknown(helper), "{leg} reflects {helper}");
        }
    }
}

#[tokio::test]
async fn nothing_is_inherited_from_a_parents_can() {
    let k = mk_v9();
    let r = op(
        &k,
        "kernel",
        json!({"type": "create_agent", "parent_id": "canvas_root", "handler_module": "chat",
               "id": "kid"}),
    )
    .await;
    assert_eq!(r["id"], "kid", "{r}");
    assert!(k.resolve_target("kid").unwrap().record().meta.get("can").is_none());
    // canvas_root holds github and memory lines; its child holds none
    assert_eq!(
        as_agent(&k, "kid", "github", json!({"type": "issues"})).await.result,
        unknown("github")
    );
    assert_eq!(
        as_agent(&k, "kid", "memory", json!({"type": "append"})).await.result,
        unknown("memory")
    );
}

/// SELF-WRITES ARE UNIFORM (owner, 2026-11-24): an agent's own code and an
/// `update_agent` call it sends meet one rule. Its ordinary fields pass; `can`,
/// a forced value, `handler_module` and `parent_id` are refused, both ways alike.
#[tokio::test]
async fn self_writes_are_one_rule_for_code_and_verb() {
    let (_t, k) = mk();
    // a live create line on the root forces `mode` on every agent of MOD below it
    create(
        &k,
        "kernel_state",
        "forcer",
        json!({"can": {"kernel_state": {"create": {"who": ["*"], "what": {MOD: {"mode": "fixed", "*": "*"}}}}}}),
    )
    .await;
    create(&k, "kernel_state", "a", json!({"description": "d0"})).await;
    let cases = [
        (json!({"description": "d1"}), true),
        (json!({"note": "n1"}), true),
        (json!({"can": {"kernel_state": {"call": ["ping"]}}}), false),
        (json!({"mode": "other"}), false),
        (json!({"handler_module": "chat"}), false),
        (json!({"parent_id": "forcer"}), false),
    ];
    for (fields, ok) in cases {
        let before = k.resolve_target("a").unwrap().record();
        // by verb: the agent sends update_agent on itself
        let mut p = json!({"type": "update_agent", "id": "a"});
        for (key, v) in fields.as_object().cloned().unwrap() {
            p[key] = v;
        }
        let by_verb = as_agent(&k, "a", "kernel", p).await.result;
        // by code: its handler sends the same write as itself
        let by_code = op(&k, "a", json!({"type": "write_own", "fields": fields.clone()})).await;
        if ok {
            assert_eq!(by_verb["updated"], true, "{fields}: {by_verb}");
            assert_eq!(by_code["updated"], true, "{fields}: {by_code}");
            let meta = k.resolve_target("a").unwrap().record().meta;
            for (key, v) in fields.as_object().unwrap() {
                assert_eq!(&meta[key], v, "{fields}");
            }
        } else {
            assert!(fantastic_kernel::is_refused(&by_verb), "{fields}: {by_verb}");
            // its own record: the refused field is named
            let key = fields.as_object().unwrap().keys().next().unwrap().clone();
            assert!(names(&by_verb, &key), "{fields}: {by_verb}");
            assert_eq!(by_code, by_verb, "{fields}: one rule both ways");
            assert_eq!(k.resolve_target("a").unwrap().record(), before, "{fields}: nothing written");
        }
    }
}

/// Review of #761, M4: the `update_agent` reply carries the record cut to what the
/// caller holds on it (rule 9). A caller holding any line (here `update: [description]`)
/// reads every key; the operator and the agent itself read it whole.
#[tokio::test]
async fn an_update_reply_carries_every_key_to_a_holder() {
    let (_t, k) = mk();
    create(
        &k,
        "kernel_state",
        "t",
        json!({"description": "d", "note": "hidden", "can": {"kernel_state": {"call": ["ping"]}}}),
    )
    .await;
    create(&k, "kernel_state", "w", json!({})).await;
    grant(&k, "w", "t", json!({"update": ["description"]}));
    let upd = |d: &str| json!({"type": "update_agent", "id": "t", "description": d});
    let r = as_agent(&k, "w", "kernel", upd("d2")).await.result;
    assert_eq!(r["updated"], true, "{r}");
    let seen = r["agent"].as_object().unwrap();
    assert_eq!(seen["id"], "t", "{r}");
    assert_eq!(seen["description"], "d2", "{r}");
    assert_eq!(seen["note"], "hidden", "{r}");
    assert!(seen.get("can").is_some(), "{r}");
    // the operator reads it whole
    let r = op(&k, "kernel", upd("d3")).await;
    assert_eq!(r["agent"]["note"], "hidden", "{r}");
    assert!(r["agent"].get("can").is_some(), "{r}");
    // the agent itself reads its own record whole
    let r = as_agent(&k, "t", "kernel", upd("d4")).await.result;
    assert_eq!(r["agent"]["note"], "hidden", "{r}");
    assert!(r["agent"].get("can").is_some(), "{r}");
}

/// The payload's `agent` of the first `agent_created` in `rx`.
fn created_record(rx: &mut fantastic_kernel::inbox::InboxReceiver) -> Option<Value> {
    while let Some(item) = rx.try_recv() {
        if item.payload["type"] == "agent_created" {
            return Some(item.payload["agent"].clone());
        }
    }
    None
}

/// Review of #761, M4: `agent_created` reaches each receiver cut to what it holds
/// on the new agent (the reflect cut). A receiver holding any line on it (the parent,
/// a watcher with `call: [watch]`, one with `update: [note]`) reads every key; a
/// watcher that is no agent the identity keys only.
#[tokio::test]
async fn agent_created_carries_every_key_to_each_holder() {
    let (_t, k) = mk();
    create(&k, "kernel_state", "p", json!({})).await;
    create(&k, "kernel_state", "w", json!({"can": {"p": {"call": ["watch"]}}})).await;
    create(&k, "kernel_state", "u", json!({"can": {"p": {"update": ["note"]}}})).await;
    let id = |s: &str| AgentId::from(s);
    let mut p_rx = k.ensure_inbox(&id("p"));
    let mut w_rx = k.ensure_inbox(&id("w"));
    let mut u_rx = k.ensure_inbox(&id("u"));
    let mut b_rx = k.ensure_inbox(&id("browser_1"));
    for who in ["w", "u"] {
        let kk = Arc::clone(&k);
        let r = fantastic_kernel::test_support::as_agent(id(who), async move {
            kk.watch(&AgentId::from("p"), AgentId::from(who)).await
        })
        .await;
        assert!(r.is_none(), "{who}: {r:?}");
    }
    let r = fantastic_kernel::send::as_operator(k.watch(&id("p"), id("browser_1"))).await;
    assert!(r.is_none(), "{r:?}");
    create(&k, "p", "c", json!({"description": "d", "note": "n", "secret": "s"})).await;
    let whole = created_record(&mut p_rx).expect("the parent gets it");
    assert_eq!((whole["note"].clone(), whole["secret"].clone()), (json!("n"), json!("s")), "{whole}");
    let w = created_record(&mut w_rx).expect("a watcher with a line gets it");
    assert_eq!((w["id"].clone(), w["description"].clone()), (json!("c"), json!("d")), "{w}");
    assert_eq!((w["note"].clone(), w["secret"].clone()), (json!("n"), json!("s")), "{w}");
    let u = created_record(&mut u_rx).expect("an update watcher gets it");
    assert_eq!(u["note"], "n", "{u}");
    assert_eq!(u["secret"], "s", "{u}");
    let b = created_record(&mut b_rx).expect("a watcher that is no agent gets it");
    assert_eq!(b["description"], "d", "{b}");
    assert!(b.get("note").is_none() && b.get("secret").is_none(), "{b}");
}
