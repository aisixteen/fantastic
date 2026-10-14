//! The PUMP runs both ends AS THE FILE_BRIDGE itself — the
//! twin of python `file_bridge/tests/test_pump_identity.py`.
//!
//! Identity comes only from dispatch: `pump` sends its `read_stream` and
//! `write_stream` from inside its own dispatch, so both far ends judge THE BRIDGE
//! — its `can_call` must reach them and their `callable_by` must accept it. A
//! caller's own grants on the ends play no part: granting `pump` on a bridge
//! lends exactly the reach that bridge has.

use super::*;
use fantastic_kernel::{Agent, Item};
use serde_json::json;

const DATA: &[u8] = b"G-DATA";

async fn op(k: &Arc<Kernel>, target: &str, payload: Value, as_: Option<&str>) -> Value {
    let item = Item::new(AgentId::from(target), payload);
    match as_ {
        None => k.send(vec![item]).await.remove(0).result,
        Some(who) => {
            fantastic_kernel::test_support::as_agent(AgentId::from(who), k.send(vec![item]))
                .await
                .remove(0)
                .result
        }
    }
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

/// G holds a file; C may call F's `pump` and `read_stream`; F reaches G only when
/// `f_reaches_g` — both halves of the F→G edge.
async fn wire(tmp: &tempfile::TempDir, f_reaches_g: bool) -> Arc<Kernel> {
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
    let (g_cb, f_cc) = if f_reaches_g {
        (
            json!({"f": ["read_stream", "write_stream"]}),
            json!({"g": ["read_stream", "write_stream"]}),
        )
    } else {
        (json!({}), json!({}))
    };
    bridge(&k, tmp, "g", json!({ "callable_by": g_cb })).await;
    bridge(
        &k,
        tmp,
        "f",
        json!({"can_call": f_cc, "callable_by": {"c": ["pump", "read_stream"]}}),
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
    let w = k
        .send(vec![Item::new(
            AgentId::from("g"),
            json!({"type": "write_stream", "path": "data.txt", "bytes": null, "truncate": true}),
        )
        .with_binary_key("bytes", DATA.to_vec())])
        .await
        .remove(0)
        .result;
    assert_eq!(w["written"], DATA.len(), "{w}");
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
async fn a_pump_reads_and_writes_as_the_bridge() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = wire(&tmp, true).await;
    // C reaches G not at all — the pump's ends are F's sends, not C's
    let direct = op(
        &k,
        "g",
        json!({"type": "read_stream", "path": "data.txt"}),
        Some("c"),
    )
    .await;
    assert_eq!(direct["reason"], "unauthorized", "{direct}");
    let out = pump(
        &k,
        "c",
        json!({"source": "g", "source_path": "data.txt", "sink_path": "copy.txt"}),
    )
    .await;
    assert_eq!(out["bytes"], DATA.len(), "{out}");
    assert_eq!(
        std::fs::read(tmp.path().join("f").join("copy.txt")).unwrap(),
        DATA
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pump_cannot_reach_an_end_the_bridge_cannot() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = wire(&tmp, false).await;
    let out = pump(
        &k,
        "c",
        json!({"source": "g", "source_path": "data.txt", "sink_path": "copy.txt"}),
    )
    .await;
    assert_eq!(out["reason"], "transport_error", "{out}");
    let err = out["error"].as_str().unwrap();
    assert!(err.starts_with("pump: read from 'g' failed: {"), "{out}");
    assert!(err.contains("f may not call 'read_stream' on g"), "{out}");
    assert!(
        !tmp.path().join("f").join("copy.txt").exists(),
        "nothing was copied"
    );
}
