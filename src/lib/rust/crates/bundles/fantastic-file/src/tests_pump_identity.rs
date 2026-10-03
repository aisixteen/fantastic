//! The PUMP runs both ends AS ITS CALLER — the bridge is no deputy. The twin of
//! python `file_bridge/tests/test_pump_identity.py`.
//!
//! Audit AV-7 (2026-10-03): `pump` sent its `read_stream` and `write_stream` from
//! inside its own dispatch, so the far ends judged THE BRIDGE, never the caller.
//! A client allowed only `pump` on F copied a private G's file into F and read it
//! there, and the reverse pump overwrote G. Now both ends run as the pump's
//! caller: the caller needs its own grants on the source and on the sink (the
//! operator, as ever, is ungated).

use super::*;
use fantastic_kernel::{Agent, Item};
use serde_json::json;

const PRIVATE: &[u8] = b"PRIVATE-G-DATA";

async fn op(k: &Arc<Kernel>, target: &str, payload: Value, as_: Option<&str>) -> Value {
    let item = Item::new(AgentId::from(target), payload);
    match as_ {
        None => k.send(vec![item]).await.remove(0).result,
        Some(who) => {
            fantastic_kernel::send::with_sender(AgentId::from(who), k.send(vec![item]))
                .await
                .remove(0)
                .result
        }
    }
}

/// A raw chunk to `target` as the operator.
async fn put(k: &Arc<Kernel>, target: &str, path: &str, data: &[u8]) -> Value {
    k.send(vec![Item::new(
        AgentId::from(target),
        json!({"type": "write_stream", "path": path, "bytes": null, "truncate": true}),
    )
    .with_binary_key("bytes", data.to_vec())])
        .await
        .remove(0)
        .result
}

async fn bridge(k: &Arc<Kernel>, tmp: &tempfile::TempDir, id: &str, meta: Value) {
    let dir = tmp.path().join(id);
    std::fs::create_dir_all(&dir).unwrap();
    let mut p = json!({"type": "create_agent", "parent_id": "kernel",
                       "handler_module": HANDLER_MODULE, "id": id,
                       "root": dir.display().to_string(), "allow": ["."],
                       "ingress_rule": "allow_all_ingress"});
    for (key, v) in meta.as_object().cloned().unwrap() {
        p[key] = v;
    }
    let r = op(k, "kernel_state", p, None).await;
    assert_eq!(r["id"], id, "{r}");
}

/// G is private to F; F answers C's `pump` and `read`; C reaches nothing else.
async fn wire(tmp: &tempfile::TempDir) -> Arc<Kernel> {
    let mut kernel = Kernel::new();
    kernel
        .bundles
        .register(HANDLER_MODULE, FileBundle::default());
    let k = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    k.register(Arc::clone(&root));
    k.set_root(Arc::clone(&root));
    bridge(
        &k,
        tmp,
        "g",
        json!({"callable_by": {"f": ["read_stream", "write_stream"]}}),
    )
    .await;
    bridge(
        &k,
        tmp,
        "f",
        json!({"can_call": {"g": ["read_stream", "write_stream"]},
               "callable_by": {"c": ["pump", "read", "read_stream", "write_stream"]}}),
    )
    .await;
    k.register(Agent::new(
        AgentId::from("c"),
        None,
        Some(AgentId::from("kernel_state")),
        json!({"can_call": {"f": "*"}})
            .as_object()
            .cloned()
            .unwrap(),
        PathBuf::new(),
        false,
    ));
    let w = put(&k, "g", "secret.txt", PRIVATE).await;
    assert_eq!(w["written"], PRIVATE.len(), "{w}");
    k
}

async fn pump(k: &Arc<Kernel>, as_: &str, args: Value) -> Value {
    let mut p = json!({"type": "pump"});
    for (key, v) in args.as_object().cloned().unwrap() {
        p[key] = v;
    }
    op(k, "f", p, Some(as_)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pump_caller_cannot_read_an_end_it_cannot_reach() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = wire(&tmp).await;
    let direct = op(
        &k,
        "g",
        json!({"type": "read_stream", "path": "secret.txt"}),
        Some("c"),
    )
    .await;
    assert_eq!(direct["reason"], "unauthorized", "{direct}");
    let out = pump(
        &k,
        "c",
        json!({"source": "g", "source_path": "secret.txt", "sink_path": "loot.txt"}),
    )
    .await;
    assert_eq!(out["reason"], "transport_error", "{out}");
    let err = out["error"].as_str().unwrap();
    assert!(err.starts_with("pump: read from 'g' failed: {"), "{out}");
    assert!(err.contains("c may not call 'read_stream' on g"), "{out}");
    assert!(
        !tmp.path().join("f").join("loot.txt").exists(),
        "nothing was copied"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pump_caller_cannot_write_an_end_it_cannot_reach() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = wire(&tmp).await;
    let w = put(&k, "f", "mine.txt", b"OVERWRITTEN").await;
    assert!(w.get("error").is_none(), "{w}");
    let out = pump(
        &k,
        "c",
        json!({"source_path": "mine.txt", "sink": "g", "sink_path": "secret.txt"}),
    )
    .await;
    assert_eq!(out["reason"], "transport_error", "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .starts_with("pump: write to 'g' failed: {"),
        "{out}"
    );
    assert_eq!(
        std::fs::read(tmp.path().join("g").join("secret.txt")).unwrap(),
        PRIVATE
    );
}

/// Granted on both ends ITSELF, the caller pumps — the bridge's own reach is not
/// what lets it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pump_runs_with_the_callers_own_grants() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = wire(&tmp).await;
    let mut patch = Map::new();
    patch.insert("can_call".into(), json!({"f": "*", "g": ["read_stream"]}));
    k.update(&AgentId::from("c"), patch);
    let mut cb = k
        .agents
        .get(&AgentId::from("g"))
        .unwrap()
        .meta
        .read()
        .unwrap()
        .get("callable_by")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    cb.insert("c".into(), json!(["read_stream"]));
    let mut patch = Map::new();
    patch.insert("callable_by".into(), Value::Object(cb));
    k.update(&AgentId::from("g"), patch);
    let out = pump(
        &k,
        "c",
        json!({"source": "g", "source_path": "secret.txt", "sink_path": "copy.txt"}),
    )
    .await;
    assert_eq!(
        out,
        json!({"source": "secret.txt", "sink": "copy.txt", "bytes": PRIVATE.len(), "chunks": 1})
    );
    assert_eq!(
        std::fs::read(tmp.path().join("f").join("copy.txt")).unwrap(),
        PRIVATE
    );
}
