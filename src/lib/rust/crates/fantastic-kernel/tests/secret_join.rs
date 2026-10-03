//! JOINING a secret-marked execution (audit 2026-10-03, F1) — the twin of python
//! `tests/kernel/test_secret_join.py`.
//!
//! A join hands back the execution's outcome without running anything, re-gated
//! on the ORIGINAL verb. Measured before the fix: `eve`, granted the same verb on
//! a credential_store but never a reader of the ref, joined `alice`'s marked
//! `get` by its int pid (and by its name) and received the real key — the store's
//! per-ref `readers` check runs in its handler, which a joiner never reaches, and
//! the joiner marked nothing.
//!
//! The rule: an execution whose item carried `secret` marks hands its REAL
//! outcome to its ORIGINAL caller only — the identity that opened it: the sending
//! agent, or the operator (no sender), or, when a bridge leg relays a foreign
//! caller, that caller's qualified `<guid>|<agent>`. Every other joiner gets the
//! outcome AS SHOWN — the result marks and every secret value replaced by
//! `<secret>`, exactly what the ledger reads back. A join of an unmarked
//! execution is unchanged.

use async_trait::async_trait;
use fantastic_kernel::secret::PLACEHOLDER;
use fantastic_kernel::{Agent, AgentId, Bundle, BundleRegistry, Item, Kernel, Outcome, Reply};
use serde_json::{json, Map, Value};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::Notify;

const KEY: &str = "sk-join-0123456789abcdef";
const MOD: &str = "secret_join_probe.tools";

struct Probe {
    release: Arc<Notify>,
}

#[async_trait]
impl Bundle for Probe {
    fn name(&self) -> &str {
        "secret_join_probe"
    }
    async fn handle(
        &self,
        id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, Box<dyn std::error::Error + Send + Sync>> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        Ok(Some(match verb {
            "boot" | "shutdown" | "reflect" => {
                json!({"id": id.as_str(), "sentence": "join probe", "verbs": {}})
            }
            "issue" => json!({"key": KEY, "note": "issued"}),
            "slow_issue" => {
                self.release.notified().await;
                json!({"key": KEY, "note": "issued"})
            }
            "echo" => json!({"echo": payload.get("text").cloned().unwrap_or(Value::Null)}),
            other => json!({"error": format!("unknown verb {other:?}")}),
        }))
    }
}

fn mk() -> (TempDir, Arc<Kernel>, Arc<Notify>) {
    let tmp = TempDir::new().unwrap();
    let release = Arc::new(Notify::new());
    let mut kernel = Kernel::new();
    let mut reg = BundleRegistry::new();
    reg.register(
        MOD,
        Probe {
            release: Arc::clone(&release),
        },
    );
    kernel.bundles = reg;
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    (tmp, kernel, release)
}

async fn create(k: &Arc<Kernel>, payload: Value) {
    let r = k
        .send(vec![Item::new(AgentId::from("kernel"), payload)])
        .await
        .remove(0)
        .result;
    assert!(r.get("id").is_some(), "{r}");
}

async fn seed(k: &Arc<Kernel>) {
    create(
        k,
        json!({"type": "create_agent", "parent_id": "kernel_state", "id": "vault",
               "handler_module": MOD,
               "callable_by": {"alice": "*", "eve": "*", "leg": "*", "P-g|*": "*"}}),
    )
    .await;
    for aid in ["alice", "eve", "leg"] {
        create(
            k,
            json!({"type": "create_agent", "parent_id": "kernel_state", "id": aid,
                   "handler_module": MOD, "can_call": {"vault": "*"}}),
        )
        .await;
    }
}

async fn send(k: &Arc<Kernel>, item: Item, sender: Option<&str>, remote: Option<&str>) -> Outcome {
    let run = async { k.send(vec![item]).await.remove(0) };
    match (sender, remote) {
        (None, _) => run.await,
        (Some(s), None) => fantastic_kernel::send::with_sender(AgentId::from(s), run).await,
        (Some(s), Some(r)) => {
            fantastic_kernel::send::with_sender(
                AgentId::from(s),
                fantastic_kernel::send::with_remote_caller(AgentId::from(s), r.to_string(), run),
            )
            .await
        }
    }
}

fn issue() -> Item {
    Item::new(AgentId::from("vault"), json!({"type": "issue"})).with_secret(json!(["result.key"]))
}

/// A JOIN of `id` (an int pid or a name), sending whatever payload.
fn join(id: Value, payload: Value) -> Item {
    Item::new(AgentId::from("vault"), payload).with_raw_id(id)
}

fn shown(out: &Outcome) -> bool {
    out.status.as_str() == "completed"
        && out.result == json!({"key": PLACEHOLDER, "note": "issued"})
}

fn real(out: &Outcome) -> bool {
    out.status.as_str() == "completed" && out.result == json!({"key": KEY, "note": "issued"})
}

fn pid(out: &Outcome) -> i64 {
    out.pid.expect("a pid").get()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_non_originating_joiner_gets_the_shown_outcome_by_pid() {
    let (_t, k, _r) = mk();
    seed(&k).await;
    let own = send(&k, issue(), Some("alice"), None).await;
    assert!(real(&own), "{:?}", own.result);
    let joined = send(
        &k,
        join(json!(pid(&own)), json!({"type": "issue"})),
        Some("eve"),
        None,
    )
    .await;
    assert!(shown(&joined), "{:?}", joined.result);
    assert!(!format!("{:?}", joined.result).contains(KEY));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_non_originating_joiner_gets_the_shown_outcome_by_name() {
    let (_t, k, _r) = mk();
    seed(&k).await;
    let own = send(&k, issue().with_id("keyread"), Some("alice"), None).await;
    assert!(real(&own), "{:?}", own.result);
    let joined = send(
        &k,
        join(json!("keyread"), json!({"type": "issue"})),
        Some("eve"),
        None,
    )
    .await;
    assert!(shown(&joined), "{:?}", joined.result);
    assert_eq!(pid(&joined), pid(&own));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_original_caller_joining_again_gets_the_real_outcome() {
    let (_t, k, _r) = mk();
    seed(&k).await;
    let own = send(&k, issue().with_id("mine"), Some("alice"), None).await;
    let again = send(
        &k,
        join(json!("mine"), json!({"type": "issue"})),
        Some("alice"),
        None,
    )
    .await;
    assert!(real(&again), "{:?}", again.result);
    assert_eq!(pid(&again), pid(&own));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_operator_is_an_identity_like_any_other() {
    // The operator joining an AGENT's secret execution is not its caller (shown);
    // an agent joining the OPERATOR's is not either (shown); the operator joining
    // its own gets the real outcome.
    let (_t, k, _r) = mk();
    seed(&k).await;
    let alices = send(&k, issue().with_id("a1"), Some("alice"), None).await;
    let op_join = send(
        &k,
        join(json!(pid(&alices)), json!({"type": "issue"})),
        None,
        None,
    )
    .await;
    assert!(shown(&op_join), "{:?}", op_join.result);
    let ops = send(&k, issue().with_id("o1"), None, None).await;
    assert!(real(&ops), "{:?}", ops.result);
    let eve_join = send(
        &k,
        join(json!("o1"), json!({"type": "issue"})),
        Some("eve"),
        None,
    )
    .await;
    assert!(shown(&eve_join), "{:?}", eve_join.result);
    let op_again = send(&k, join(json!("o1"), json!({"type": "x"})), None, None).await;
    assert!(real(&op_again), "{:?}", op_again.result);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_foreign_callers_through_one_leg_are_two_identities() {
    // A bridge leg relays many callers of ONE peer kernel; each is its qualified
    // `<guid>|<agent>`, not the leg.
    let (_t, k, _r) = mk();
    seed(&k).await;
    let first = send(&k, issue().with_id("peer-read"), Some("leg"), Some("P-g|a")).await;
    assert!(real(&first), "{:?}", first.result);
    let other = send(
        &k,
        join(json!("peer-read"), json!({"type": "issue"})),
        Some("leg"),
        Some("P-g|b"),
    )
    .await;
    assert!(shown(&other), "{:?}", other.result);
    let same = send(
        &k,
        join(json!("peer-read"), json!({"type": "issue"})),
        Some("leg"),
        Some("P-g|a"),
    )
    .await;
    assert!(real(&same), "{:?}", same.result);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_joiner_waiting_on_an_execution_in_flight_gets_it_shown() {
    let (_t, k, release) = mk();
    seed(&k).await;
    let ko = Arc::clone(&k);
    let owner = tokio::spawn(async move {
        send(
            &ko,
            Item::new(AgentId::from("vault"), json!({"type": "slow_issue"}))
                .with_id("slow")
                .with_secret(json!(["result.key"])),
            Some("alice"),
            None,
        )
        .await
    });
    for _ in 0..500 {
        if k.ledger.resolve_name("slow").is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert!(k.ledger.resolve_name("slow").is_some(), "the owner opened");
    let kj = Arc::clone(&k);
    let joiner = tokio::spawn(async move {
        send(
            &kj,
            join(json!("slow"), json!({"type": "x"})),
            Some("eve"),
            None,
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !k.ledger.is_done(k.ledger.resolve_name("slow").unwrap()),
        "still in flight"
    );
    release.notify_one();
    let own = owner.await.unwrap();
    let joined = joiner.await.unwrap();
    assert!(real(&own), "{:?}", own.result);
    assert!(shown(&joined), "{:?}", joined.result);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_join_of_an_unmarked_execution_is_unchanged() {
    let (_t, k, _r) = mk();
    seed(&k).await;
    let own = send(
        &k,
        Item::new(
            AgentId::from("vault"),
            json!({"type": "echo", "text": "hi"}),
        )
        .with_id("plain"),
        Some("alice"),
        None,
    )
    .await;
    let joined = send(
        &k,
        join(json!("plain"), json!({"type": "echo"})),
        Some("eve"),
        None,
    )
    .await;
    assert_eq!(joined.result, own.result);
    assert_eq!(joined.result, json!({"echo": "hi"}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn carried_secret_resolves_an_item_id_as_send_does() {
    // The question ai_core asks before a model's `<exec_id>` joins anything: an
    // int is a message id, a string a caller's name; anything else, an id naming
    // no held execution, and an unmarked execution are all false.
    let (_t, k, _r) = mk();
    seed(&k).await;
    let marked = send(&k, issue().with_id("m1"), Some("alice"), None).await;
    let plain = send(
        &k,
        Item::new(AgentId::from("vault"), json!({"type": "echo", "text": "x"})).with_id("p1"),
        Some("alice"),
        None,
    )
    .await;
    assert!(k.carried_secret(&json!(pid(&marked))));
    assert!(k.carried_secret(&json!("m1")));
    assert!(!k.carried_secret(&json!(pid(&plain))));
    assert!(!k.carried_secret(&json!("p1")));
    assert!(
        !k.carried_secret(&json!(pid(&marked).to_string())),
        "a string is a name"
    );
    for nothing in [
        json!(true),
        Value::Null,
        json!(10_000),
        json!("nobody"),
        json!(["m1"]),
    ] {
        assert!(!k.carried_secret(&nothing), "{nothing}");
    }
}
