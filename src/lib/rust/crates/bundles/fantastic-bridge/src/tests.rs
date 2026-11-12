//! Unit tests for the ws_bridge / relay_connector derivations.
//!
//! Most tests drive the memory transport — deterministic, no I/O,
//! and exercises every code path the bridge cares about. WS gets
//! one negative-path test (unreachable host → clean error) because
//! the real two-kernel round-trip is exercised by the cross-runtime
//! selftest, not in-process Rust unit tests.

use super::*;
use fantastic_kernel::bundle::Bundle;
use fantastic_io_bridge::authorizer;
use fantastic_kernel::Agent;
use fantastic_kernel::Item;
use fantastic_kernel::Step;
use serde_json::Map;
use tempfile::TempDir;

/// Build a kernel with the bridge + file bundles registered and a
/// root agent. Each test gets its own tempdir so the global
/// `BRIDGES` map can carry concurrent test agents without id clashes.
async fn mk_kernel(tmp: &TempDir) -> Arc<Kernel> {
    let mut kernel = Kernel::with_storage(fantastic_kernel::StorageMode::Disk {
        state_dir: tmp.path().join(".fantastic"),
        work_dir: tmp.path().to_path_buf(),
    });
    kernel.bundles.register(WS_HANDLER_MODULE, WsBridgeBundle);
    kernel
        .bundles
        .register("file_bridge.tools", fantastic_file::FileBundle::default());
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
    kernel
}

/// Mint a unique-per-test id derived from the tempdir basename so
/// global `BRIDGES` doesn't collide under parallel test runs.
fn id_for(prefix: &str, tmp: &TempDir) -> String {
    let suffix = tmp
        .path()
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
        .replace('.', "_");
    format!("{prefix}_{suffix}")
}

async fn create_bridge(kernel: &Arc<Kernel>, id: &str, peer_id: &str, transport: &str) {
    // io legs SEAL by default; these functional round-trip helpers open the leg so the
    // receiver dispatches inbound frames (mirrors py's _wire_memory_pair allow_all).
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({
                "type": "create_agent", "parent_id": "kernel",
                "handler_module": WS_HANDLER_MODULE, "can_call": {"$parent": "*", "$subtree": "*"},
                "id": id,
                "peer_id": peer_id,
                "transport": transport,
                "ingress_rule": "allow_all_ingress",
            }),
        )])
        .await
        .remove(0)
        .result;
}

/// Open ONE edge, both ends, as an operator would when wiring a bridge.
///
/// An inbound bridge frame dispatches AS THE LEG — the read loop is a task the
/// leg's own `boot` spawned, so its sends are the leg's. A bridge serving calls
/// onto another agent needs the egress grant on itself and the ingress grant on
/// the target; the baseline is `$self` alone.
async fn grant_edge(kernel: &Arc<Kernel>, from: &str, to: &str) {
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": from,
                   "can_call": {"$self": "*", "$parent": "*", "$subtree": "*", to: "*"}}),
        )])
        .await
        .remove(0)
        .result;
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": to,
                   "callable_by": {"$self": "*", "$parent": "*", "$subtree": "*", from: "*"}}),
        )])
        .await
        .remove(0)
        .result;
}

/// Patch ONE record field, as the operator.
fn patch(kernel: &Arc<Kernel>, id: &str, key: &str, value: Value) {
    let mut m = Map::new();
    m.insert(key.to_string(), value);
    kernel.update(&AgentId::from(id), m);
}

/// The ref every relay leg's password sits at, in its own store.
pub(super) const RELAY_REF: &str = "relay/group";

/// Register the credential store a relay leg reads its password from — on its
/// in-memory keychain double, so no test touches a file or the real keychain.
pub(super) fn register_relay_store(reg: &mut fantastic_kernel::BundleRegistry) {
    reg.register(
        fantastic_credential_store::HANDLER_MODULE,
        fantastic_credential_store::test_support::bundle(),
    );
}

/// THE RELAY PASSWORD a relay leg reads before every dial: a
/// credential_store `<leg>_pw` holding `password` at [`RELAY_REF`], bound to the
/// relay URL's host and readable by `leg`, its `callable_by` opening `get` for
/// the leg. Made BEFORE the leg (a started kernel boots a leg at create). Returns
/// the store id; [`with_relay_password`] puts the leg's side on its record.
pub(super) async fn relay_password(
    k: &Arc<Kernel>,
    leg: &str,
    relay_url: &str,
    pw: &str,
) -> String {
    let root = k.root().expect("a root").id.0.clone();
    let store = format!("{leg}_pw");
    let host = relay_url
        .split_once("://")
        .map_or(relay_url, |(_, rest)| rest)
        .trim_end_matches('/');
    let made = k
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "create_agent", "parent_id": root,
                   "handler_module": fantastic_credential_store::HANDLER_MODULE, "id": store,
                   "backend": "keychain",
                   "refs": {RELAY_REF: {"host": host}},
                   "callable_by": {leg: ["get"]}}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(made["id"], json!(store), "{made}");
    set_relay_password(k, &store, pw).await;
    store
}

/// Store (or rotate) the relay password in `store`, marked secret.
pub(super) async fn set_relay_password(k: &Arc<Kernel>, store: &str, pw: &str) {
    let put = k
        .send(vec![Item::new(
            AgentId::from(store),
            json!({"type": "set", "ref": RELAY_REF, "fields": {"password": pw}}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(put["stored"], json!(true), "{put}");
}

/// A relay leg's side of the wiring: `credentials` naming `store`, and a `get`
/// on it merged into the leg's `can_call`.
pub(super) fn with_relay_password(extra: &mut Value, store: &str) {
    extra["credentials"] = json!({"store": store, "ref": RELAY_REF});
    if !extra["can_call"].is_object() {
        extra["can_call"] = json!({});
    }
    extra["can_call"][store] = json!(["get"]);
}

/// A kernel whose root carries an instance `guid` — what a bridge stamps on its
/// outbound frames and what the peer's leg is PINNED to.
async fn mk_kernel_guid(tmp: &TempDir, guid: &str) -> Arc<Kernel> {
    let k = mk_kernel(tmp).await;
    patch(&k, "kernel_state", "guid", json!(guid));
    k
}

#[tokio::test]
async fn identity_from_meta_builds_directory_attrs_blob() {
    // The kernelgroup typing we advertise: well-known keys present → blob; defaults
    // (role=kernel / no owner / empty exposes) OMITTED so a plain peer advertises {}.
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp).await;

    let mut meta = Map::new();
    meta.insert("role".into(), json!("manager"));
    meta.insert("owner_guid".into(), json!("mgrA"));
    meta.insert("exposes".into(), json!(["stop", "restart"]));
    let typed = Agent::new(
        AgentId::from("leg"),
        Some("kernel_state".to_string()),
        None,
        meta,
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&typed));
    let attrs = identity_from_meta(&AgentId::from("leg"), &kernel);
    assert_eq!(attrs["role"], json!("manager"));
    assert_eq!(attrs["owner_guid"], json!("mgrA"));
    assert_eq!(attrs["exposes"], json!(["stop", "restart"]));

    // A plain kernel (role defaults to kernel, no owner/exposes) advertises nothing.
    let plain = Agent::new(
        AgentId::from("plain"),
        Some("kernel_state".to_string()),
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(plain);
    assert_eq!(
        identity_from_meta(&AgentId::from("plain"), &kernel),
        json!({})
    );
}

#[tokio::test]
async fn reflect_shape_for_memory_transport() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp).await;
    let bid = id_for("brg_reflect", &tmp);
    create_bridge(&kernel, &bid, "peer_x", "memory").await;
    let r = kernel
        .send(vec![Item::new(
            AgentId::from(bid.as_str()),
            json!({"type": "reflect"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["id"], bid);
    assert_eq!(r["transport"], "memory");
    assert_eq!(r["connected"], false);
    assert_eq!(r["peer_id"], "peer_x");
    assert_eq!(r["pending_count"], 0);
    assert!(r["verbs"]["forward"].is_string(), "{r}");
    let d = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "docs", "of": bid, "tier": "detailed"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(d["verbs"]["forward"].is_string(), "{d}");
}

#[tokio::test]
async fn boot_pairs_succeed_via_memory() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp).await;
    let a_id = id_for("brg_a_boot", &tmp);
    let b_id = id_for("brg_b_boot", &tmp);
    create_bridge(&kernel, &a_id, &b_id, "memory").await;
    create_bridge(&kernel, &b_id, &a_id, "memory").await;
    inject_pair(&AgentId::from(a_id.as_str()), &AgentId::from(b_id.as_str()));
    let ra = kernel
        .send(vec![Item::new(
            AgentId::from(a_id.as_str()),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;
    let rb = kernel
        .send(vec![Item::new(
            AgentId::from(b_id.as_str()),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(ra["booted"], true);
    assert_eq!(ra["transport"], "memory");
    assert_eq!(rb["booted"], true);

    let ref_a = kernel
        .send(vec![Item::new(
            AgentId::from(a_id.as_str()),
            json!({"type": "reflect"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(ref_a["connected"], true);

    // Cleanup so global BRIDGES doesn't carry into other tests.
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(a_id.as_str()),
            json!({"type": "shutdown"}),
        )])
        .await
        .remove(0)
        .result;
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(b_id.as_str()),
            json!({"type": "shutdown"}),
        )])
        .await
        .remove(0)
        .result;
}

#[tokio::test]
async fn forward_round_trip_over_memory() {
    let tmp = TempDir::new().unwrap();
    let tmp_b = TempDir::new().unwrap();
    // TWO kernels: a leg serves only the ONE peer kernel it is pinned to, and never
    // this kernel's own guid.
    let ka = mk_kernel_guid(&tmp, "guid-a").await;
    let kernel = mk_kernel_guid(&tmp_b, "guid-b").await;
    let a_id = id_for("brg_a_fwd", &tmp);
    let b_id = id_for("brg_b_fwd", &tmp);
    // On B's kernel side we also create a local "target" file agent
    // that the forward will dispatch against.
    let file_id = id_for("ff_fwd", &tmp);
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({
                "type": "create_agent", "parent_id": "kernel",
                "handler_module": "file_bridge.tools", "allow": ["."],
                "id": file_id,
                "root": tmp.path().to_string_lossy(),
            }),
        )])
        .await
        .remove(0)
        .result;
    create_bridge(&ka, &a_id, &b_id, "memory").await;
    create_bridge(&kernel, &b_id, &a_id, "memory").await;
    // B serves the inbound call onto the file agent — a SIBLING, so the leg's
    // egress is explicit, and the file agent accepts A's agents BY NAME.
    grant_edge(&kernel, &b_id, &file_id).await;
    inject_pair(&AgentId::from(a_id.as_str()), &AgentId::from(b_id.as_str()));
    let _ = ka
        .send(vec![Item::new(
            AgentId::from(a_id.as_str()),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(b_id.as_str()),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;

    // A.forward(target=<file_id>, payload={type:"reflect"}) ships a
    // raw call frame → B's read loop dispatches kernel.send(vec![Item::new(<file_id>, // {reflect})]) → file_agent's reflect reply gets shipped back as the
    // forward's return value (asymmetric; no forward envelope).
    let reply = ka
        .send(vec![Item::new(
            AgentId::from(a_id.as_str()),
            json!({
                "type": "forward",
                "target": file_id,
                "payload": {"type": "reflect"},
                "forward_timeout": 30.0,
            }),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(reply["id"], file_id, "forward reply: {reply}");
    assert!(reply["root"].is_string());

    let _ = ka
        .send(vec![Item::new(
            AgentId::from(a_id.as_str()),
            json!({"type": "shutdown"}),
        )])
        .await
        .remove(0)
        .result;
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(b_id.as_str()),
            json!({"type": "shutdown"}),
        )])
        .await
        .remove(0)
        .result;
}

#[tokio::test]
async fn pending_futures_rejected_on_close() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp).await;
    let a_id = id_for("brg_a_close", &tmp);
    let b_id = id_for("brg_b_close", &tmp);
    create_bridge(&kernel, &a_id, &b_id, "memory").await;
    create_bridge(&kernel, &b_id, &a_id, "memory").await;
    inject_pair(&AgentId::from(a_id.as_str()), &AgentId::from(b_id.as_str()));
    // Boot ONLY A. B's half stays attached but no read loop drains
    // it — A's send_frame puts the call frame on B's inbox queue
    // and nothing answers. A's pending oneshot is parked, waiting
    // for a `reply` frame that will never arrive.
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(a_id.as_str()),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;

    let k = Arc::clone(&kernel);
    let a_id_clone = a_id.clone();
    let forward = tokio::spawn(async move {
        k.send(vec![Item::new(
            AgentId::from(a_id_clone.as_str()),
            json!({
                "type": "forward",
                "target": "irrelevant_target",
                "payload": {"type": "reflect"},
                "forward_timeout": 30.0,
            }),
        )])
        .await
        .remove(0)
        .result
    });
    // Tiny delay so the forward registers its pending oneshot
    // before we slam the door.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    // Delete A — its read loop tears down in `on_delete` and every pending
    // forward fails with a close-flavoured reply. Through DELETE, not a
    // `shutdown` verb: as in python, `shutdown` is a lifecycle no-op a caller
    // may send and nothing tears down behind it.
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "delete_agent", "id": a_id.as_str()}),
        )])
        .await
        .remove(0)
        .result;
    let reply = forward.await.unwrap();
    let err = reply["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("shut down") || err.contains("closed") || err.contains("transport"),
        "expected a close-flavoured error, got: {reply}",
    );
}

#[tokio::test]
async fn watch_remote_before_boot_errors() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp).await;
    let bid = id_for("brg_watch_nb", &tmp);
    create_bridge(&kernel, &bid, "peer_x", "memory").await;
    let r = kernel
        .send(vec![Item::new(
            AgentId::from(bid.as_str()),
            json!({"type": "watch_remote", "target": "kernel_state"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(
        r["error"]
            .as_str()
            .unwrap_or_default()
            .contains("not connected"),
        "expected not-connected error, got: {r}"
    );
}

#[tokio::test]
async fn watch_remote_sends_watch_frame() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp).await;
    let bid = id_for("brg_watch", &tmp);
    create_bridge(&kernel, &bid, "stand_in", "memory").await;
    // Inject A's half; keep the peer half to read what A ships.
    let peer = inject_one(&AgentId::from(bid.as_str()));
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(bid.as_str()),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;

    let r = kernel
        .send(vec![Item::new(
            AgentId::from(bid.as_str()),
            json!({"type": "watch_remote", "target": "remote_core"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["ok"], true, "watch_remote: {r}");
    assert_eq!(r["watching"], "remote_core");

    let frame = peer
        .recv_frame()
        .await
        .expect("peer should receive a frame")
        .into_value();
    assert_eq!(frame["type"], "watch");
    assert_eq!(frame["src"], "remote_core");

    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(bid.as_str()),
            json!({"type": "shutdown"}),
        )])
        .await
        .remove(0)
        .result;
}

#[tokio::test]
async fn reconnect_calls_shutdown_then_boot() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp).await;
    let a_id = id_for("brg_a_rec", &tmp);
    let b_id = id_for("brg_b_rec", &tmp);
    create_bridge(&kernel, &a_id, &b_id, "memory").await;
    create_bridge(&kernel, &b_id, &a_id, "memory").await;
    inject_pair(&AgentId::from(a_id.as_str()), &AgentId::from(b_id.as_str()));
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(a_id.as_str()),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(b_id.as_str()),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;

    // Re-inject a fresh pair so reconnect's boot has something to
    // attach to (the original pair has been consumed at first boot).
    inject_pair(&AgentId::from(a_id.as_str()), &AgentId::from(b_id.as_str()));
    let r = kernel
        .send(vec![Item::new(
            AgentId::from(a_id.as_str()),
            json!({"type": "reconnect"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["booted"], true, "reconnect: {r}");
    let reflect = kernel
        .send(vec![Item::new(
            AgentId::from(a_id.as_str()),
            json!({"type": "reflect"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(reflect["connected"], true);

    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(a_id.as_str()),
            json!({"type": "shutdown"}),
        )])
        .await
        .remove(0)
        .result;
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(b_id.as_str()),
            json!({"type": "shutdown"}),
        )])
        .await
        .remove(0)
        .result;
}

#[tokio::test]
async fn ws_boot_fails_cleanly_on_unreachable_host() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp).await;
    let bid = id_for("brg_ws", &tmp);
    // TEST-NET-1 (RFC 5737) — guaranteed unreachable. We give a
    // WS uses tokio-tungstenite. The OS will fail-fast on a non-routable
    // address. Bound the overall test to a few seconds.
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({
                "type": "create_agent", "parent_id": "kernel",
                "handler_module": WS_HANDLER_MODULE, "can_call": {"$parent": "*", "$subtree": "*"},
                "id": bid,
                "peer_id": "peer_x",
                "transport": "ws",
                "host": "192.0.2.1",
                "port": 9,
            }),
        )])
        .await
        .remove(0)
        .result;
    let r = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        kernel.send(vec![Item::new(
            AgentId::from(bid.as_str()),
            json!({"type": "boot"}),
        )]),
    )
    .await
    .expect("ws boot should not hang")
    .remove(0)
    .result;
    let err = r["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("ws connect failed") || err.contains("ws") || !err.is_empty(),
        "expected clean error, got: {r}",
    );
    let ref_r = kernel
        .send(vec![Item::new(
            AgentId::from(bid.as_str()),
            json!({"type": "reflect"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(ref_r["connected"], false, "reflect: {ref_r}");
}

// relay_connector transport framing (wrap/unwrap) is unit-tested in
// `transport::relay`'s own `#[cfg(test)] mod tests`; the engine forward/reply +
// symmetric-serving path it reuses is covered by the memory-transport tests
// above; the full live path is the `integration_tests/relay_e2e` matrix.

// ── authorization seam (ingress/egress rules) ───────────────────────

/// Build a `call` Action carrying an optional envelope token, for the unit tests.
fn call_action(token: Option<&str>) -> Action<'_> {
    Action {
        kind: "call",
        target: "t",
        verb: "reflect",
        token,
    }
}

#[test]
fn ingress_resolves_allow_and_deny() {
    use authorizer::ingress::resolve;
    // absent / null ⇒ DenyInbound (SEALED by default)
    assert!(matches!(
        resolve(None).authorize(&call_action(None)),
        Decision::Deny(_)
    ));
    assert!(matches!(
        resolve(Some(&Value::Null)).authorize(&call_action(None)),
        Decision::Deny(_)
    ));
    // explicit allow_all_ingress opens it
    assert!(matches!(
        resolve(Some(&json!("allow_all_ingress"))).authorize(&call_action(None)),
        Decision::Allow
    ));
    // string + object form; `type` is the ONLY object spelling
    let s = resolve(Some(&json!("deny_ingress")));
    assert!(matches!(s.authorize(&call_action(None)), Decision::Deny(_)));
    let o = resolve(Some(&json!({"type": "deny_ingress"})));
    assert!(matches!(o.authorize(&call_action(None)), Decision::Deny(_)));
    // An object with no `type` is NOT a rule — and python resolves "no rule" to
    // the plain SEAL, not to an error, so every caller renders it the same way.
    let typeless = resolve(Some(&json!({"policy": "deny_ingress"})));
    match typeless.authorize(&call_action(None)) {
        Decision::Deny(d) => assert_eq!(d.reason, "edge sealed by policy"),
        Decision::Allow => panic!("a type-less rule object must seal"),
    }
    // The SEAL refuses EVERY inbound kind — dispatch AND telemetry (py-canonical;
    // allowing `watch` would make the seal partial and let a "sealed" leg leak
    // state to watchers).
    let watch = s.authorize(&Action {
        kind: "watch",
        target: "t",
        verb: "watch",
        token: None,
    });
    assert!(matches!(watch, Decision::Deny(_)));
    // UNKNOWN ⇒ SEALS and names the bad rule — it does NOT Err.
    //
    // Not `Err`: resolution runs PER CALL, not at boot, and in python such an
    // error surfaces as an unhandled exception out of the handler, killing the
    // caller's whole chain with no reply at all. An unresolvable rule is not an
    // authorization, so deny — and say which name was wrong. Mirrors py
    // `test_unknown_rule_names_the_legal_values`.
    let unknown = resolve(Some(&json!("password:kernel-internal-transcripts")));
    match unknown.authorize(&Action {
        kind: "call",
        target: "t",
        verb: "reflect",
        token: None,
    }) {
        Decision::Deny(denial) => {
            // The verdict splits: `reason` states WHAT happened (the name was not
            // resolvable), `hint` states the repair + the legal set. A caller that
            // read only one half would get half a refusal.
            let (reason, hint) = (denial.reason, denial.hint);
            assert!(
                reason.contains("password:kernel-internal-transcripts"),
                "{reason}"
            );
            assert!(
                hint.contains("allow_all_ingress"),
                "must teach the legal set: {hint}"
            );
        }
        Decision::Allow => panic!("an unknown ingress rule must never allow"),
    }
}

#[test]
fn ingress_password_checks_envelope_token() {
    use authorizer::ingress::resolve;
    // test-unique env var so parallel tests don't clobber each other
    let env = "FANTASTIC_GROUP_TOKEN_RS_ING";
    std::env::set_var(env, "s3cret");
    // `env` (new) spelling threads to the rule
    let p = resolve(Some(&json!({"type": "password_ingress", "env": env})));
    assert!(matches!(
        p.authorize(&call_action(Some("s3cret"))),
        Decision::Allow
    ));
    assert!(matches!(
        p.authorize(&call_action(Some("nope"))),
        Decision::Deny(_)
    ));
    assert!(matches!(p.authorize(&call_action(None)), Decision::Deny(_)));
    // fail-closed when the env var is unset
    std::env::remove_var(env);
    assert!(matches!(
        p.authorize(&call_action(Some("s3cret"))),
        Decision::Deny(_)
    ));
}

#[test]
fn egress_resolves_and_presents() {
    use authorizer::egress::resolve;
    // absent ⇒ Silent (present nothing)
    assert!(resolve(None).unwrap().credential().is_none());
    // An inbound-only STEM arriving through `auth` presents nothing — the collapse is
    // spelled out in `manifold_spec`; a bare inbound name written straight into
    // `egress_rule` is a typo, and must be loud.
    for stem in ["deny", "allow_all"] {
        let scoped = authorizer::manifold_spec(None, Some(&json!(stem)), "_egress").unwrap();
        assert_eq!(scoped, json!("silent_egress"));
        assert!(resolve(Some(&scoped)).unwrap().credential().is_none());
    }
    assert!(resolve(Some(&json!("deny_ingress"))).is_err());
    // password ⇒ presents the env token. `env` is the only spelling for the secret key.
    let env = "FANTASTIC_GROUP_TOKEN_RS_EG";
    std::env::set_var(env, "abc");
    let p = resolve(Some(&json!({"type": "password_egress", "env": env}))).unwrap();
    assert_eq!(p.credential().as_deref(), Some("abc"));
    std::env::remove_var(env);
    assert!(p.credential().is_none()); // unset ⇒ presents nothing
                                       // unknown ⇒ Err
    assert!(resolve(Some(&json!("nope"))).is_err());
}

#[test]
fn a_name_from_the_other_manifold_is_loud_not_silent() {
    // Rule names are MANIFOLD-SCOPED so one name can never half-resolve across the two
    // registries. Unscoped, `auth:"silent"` — a legitimate EGRESS policy — would
    // resolve ingress to `DenyInbound(unknown="silent")`, SEALING the leg while
    // recording the unknown name and saying nothing; and `auth:"allow_all"` would
    // hand egress a `Silent` nobody asked for. One unscoped name against two
    // registries with different membership is a fallback, and this kernel has none.
    //
    // It still DENIES — an unresolvable rule is not an authorization — but it names
    // the manifold mistake instead of looking like an ordinary seal.
    let sealed = authorizer::ingress::resolve(Some(&json!("silent_egress")));
    let Decision::Deny(denial) = sealed.authorize(&call_action(None)) else {
        panic!("an egress name in ingress_rule must deny");
    };
    let (reason, hint) = (denial.reason, denial.hint);
    assert!(reason.contains("silent_egress"), "{reason}");
    assert!(hint.contains("manifold-scoped"), "{hint}");
    assert!(hint.contains("allow_all_ingress"), "{hint}");

    // The `auth` stem is scoped per manifold, so an egress-only stem cannot silently
    // seal ingress: it surfaces as THIS manifold's missing rule, by name.
    let scoped = authorizer::manifold_spec(None, Some(&json!("silent")), "_ingress").unwrap();
    assert_eq!(scoped, json!("silent_ingress"));

    // And the stems that DO span both manifolds still work from one field.
    for stem in ["password", "allow_all"] {
        assert!(authorizer::ingress::unknown(
            authorizer::manifold_spec(None, Some(&json!(stem)), "_ingress").as_ref()
        )
        .is_none());
    }
}

#[test]
fn auth_shorthand_is_symmetric() {
    // `auth:"password"` ⇒ ingress checks AND egress presents (group member)
    let env = "FANTASTIC_GROUP_TOKEN_RS_SYM";
    std::env::set_var(env, "k");
    // ONE bare stem on `auth`; each manifold appends its own suffix.
    let spec = json!({"type": "password", "env": env});
    let ing = authorizer::ingress::resolve(
        authorizer::manifold_spec(None, Some(&spec), "_ingress").as_ref(),
    );
    let eg = authorizer::egress::resolve(
        authorizer::manifold_spec(None, Some(&spec), "_egress").as_ref(),
    )
    .unwrap();
    assert!(matches!(
        ing.authorize(&call_action(Some("k"))),
        Decision::Allow
    ));
    assert_eq!(eg.credential().as_deref(), Some("k"));
    std::env::remove_var(env);
}

/// Create a bridge agent carrying an `auth` policy meta field.
async fn create_bridge_with_auth(kernel: &Arc<Kernel>, id: &str, peer_id: &str, auth: &str) {
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({
                "type": "create_agent", "parent_id": "kernel",
                "handler_module": WS_HANDLER_MODULE, "can_call": {"$parent": "*", "$subtree": "*"},
                "id": id,
                "peer_id": peer_id,
                "transport": "memory",
                "auth": auth,
            }),
        )])
        .await
        .remove(0)
        .result;
}

#[tokio::test]
async fn deny_inbound_refuses_inbound_call() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp).await;
    let bid = id_for("brg_deny", &tmp);
    create_bridge_with_auth(&kernel, &bid, "stand_in", "deny").await;
    // Inject A's half; keep the peer half to push a synthetic inbound call.
    let peer = inject_one(&AgentId::from(bid.as_str()));
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(bid.as_str()),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;
    // reflect surfaces the policy back (read-key == write-key, py parity).
    let reflect = kernel
        .send(vec![Item::new(
            AgentId::from(bid.as_str()),
            json!({"type": "reflect"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(reflect["ingress_rule"], "deny_ingress");
    assert_eq!(reflect["sealed"], true);

    peer.send_frame(json!({
        "type": "call",
        "id": "c1",
        "target": "kernel_state",
        "payload": {"type": "reflect"},
    }))
    .await
    .unwrap();
    let reply = peer.recv_frame().await.expect("a reply frame").into_value();
    assert_eq!(reply["type"], "reply");
    assert_eq!(reply["id"], "c1");
    assert_eq!(reply["data"]["reason"], "unauthorized", "reply: {reply}");

    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(bid.as_str()),
            json!({"type": "shutdown"}),
        )])
        .await
        .remove(0)
        .result;
}

#[tokio::test]
async fn binary_call_round_trips_raw_bytes_over_the_bridge() {
    // 4b: a read_stream/write_stream forwarded over the bridge carries RAW BYTES
    // (a binary frame), never base64. Drive the inbound side: the `peer` half
    // pushes binary `call` frames at the bridge's read loop, which dispatches on
    // the binary channel and replies — write_stream → text status, read_stream →
    // a BINARY reply frame whose body is the chunk verbatim.
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp).await;
    // An open file_bridge in the workdir to receive/serve the bytes.
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({
                "type": "create_agent", "parent_id": "kernel",
                "handler_module": "file_bridge.tools", "allow": ["."],
                "id": "fb",
                "root": tmp.path().to_string_lossy(),
                "ingress_rule": "allow_all_ingress",
            }),
        )])
        .await
        .remove(0)
        .result;
    let bid = id_for("brg_bin", &tmp);
    create_bridge_with_auth(&kernel, &bid, "stand_in", "allow_all").await;
    // the leg serves the inbound calls AS ITSELF: `fb` accepts the leg
    grant_edge(&kernel, &bid, "fb").await;
    let peer = inject_one(&AgentId::from(bid.as_str()));
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(bid.as_str()),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;

    // Non-UTF-8 bytes — proves it's raw, not text/base64.
    let payload: Vec<u8> = vec![0x00, 0xFF, 0xCA, 0xFE, 0xBA, 0xBE, 0x10, 0x80];

    // write_stream via a binary inbound call: the body at `["payload", "bytes"]`.
    let at_bytes = || vec![Step::from("payload"), Step::from("bytes")];
    peer.send_binary(
        json!({"type":"call","id":"w1","target":"fb",
               "payload":{"type":"write_stream","path":"blob.bin","truncate":true,"bytes":null}}),
        Blob::new(at_bytes(), payload.clone()),
    )
    .await
    .unwrap();
    let w = peer.recv_frame().await.expect("write reply").into_value();
    assert_eq!(w["type"], "reply");
    assert_eq!(w["data"]["written"], payload.len(), "write reply: {w}");

    // read_stream (a stream verb, so the binary channel even from a text
    // frame) → the reply is a BINARY frame, its body at `["data", "bytes"]`.
    peer.send_frame(json!({"type":"call","id":"r1","target":"fb",
                           "payload":{"type":"read_stream","path":"blob.bin"}}))
        .await
        .unwrap();
    match peer.recv_frame().await.expect("read reply") {
        crate::transport::Frame::Binary(header, body) => {
            assert_eq!(header["id"], "r1");
            assert_eq!(header["data"]["eof"], true);
            assert_eq!(header["data"]["bytes"], Value::Null, "{header}");
            assert_eq!(body.path, vec![Step::from("data"), Step::from("bytes")]);
            assert_eq!(
                body.bytes, payload,
                "bytes must round-trip raw over the bridge"
            );
        }
        crate::transport::Frame::Text(v) => panic!("expected a binary reply, got text: {v}"),
    }

    // A BINARY call is an ordinary execution: the frame id NAMES it, so the same
    // id replayed (a reconnect re-sending its queue) joins the recorded outcome
    // and writes nothing — python `_engine` sets `item["id"]` for every call.
    peer.send_binary(
        json!({"type":"call","id":"w1","target":"fb",
               "payload":{"type":"write_stream","path":"blob.bin","truncate":true,"bytes":null}}),
        Blob::new(at_bytes(), b"other".to_vec()),
    )
    .await
    .unwrap();
    let again = peer
        .recv_frame()
        .await
        .expect("replayed reply")
        .into_value();
    assert_eq!(
        again["data"]["written"],
        payload.len(),
        "the recorded outcome: {again}"
    );
    assert_eq!(std::fs::read(tmp.path().join("blob.bin")).unwrap(), payload);
    let ledger = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "ledger"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(
        ledger["executions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["name"] == "w1" && e["target"] == "fb"),
        "the binary call is a ledgered execution: {ledger}"
    );

    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(bid.as_str()),
            json!({"type": "shutdown"}),
        )])
        .await
        .remove(0)
        .result;
}

#[tokio::test]
async fn deny_inbound_default_refuses_inbound_call() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp).await;
    let bid = id_for("brg_sealed", &tmp);
    // No `ingress_rule` meta ⇒ DenyInbound (SEALED by default).
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({
                "type": "create_agent", "parent_id": "kernel",
                "handler_module": WS_HANDLER_MODULE, "can_call": {"$parent": "*", "$subtree": "*"},
                "id": bid,
                "peer_id": "stand_in",
                "transport": "memory",
            }),
        )])
        .await
        .remove(0)
        .result;
    let peer = inject_one(&AgentId::from(bid.as_str()));
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(bid.as_str()),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;
    let reflect = kernel
        .send(vec![Item::new(
            AgentId::from(bid.as_str()),
            json!({"type": "reflect"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(reflect["ingress_rule"], "deny_ingress");
    assert_eq!(reflect["sealed"], true);

    peer.send_frame(json!({
        "type": "call",
        "id": "c2",
        "target": "kernel_state",
        "payload": {"type": "reflect"},
    }))
    .await
    .unwrap();
    let reply = peer.recv_frame().await.expect("a reply frame").into_value();
    assert_eq!(reply["type"], "reply");
    assert_eq!(
        reply["data"]["reason"], "unauthorized",
        "sealed-by-default leg must deny: {reply}"
    );

    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(bid.as_str()),
            json!({"type": "shutdown"}),
        )])
        .await
        .remove(0)
        .result;
}

#[tokio::test]
async fn password_gate_checks_inbound_and_presents_on_forward() {
    // Unique env var so parallel tests don't collide (codebase pattern).
    let env = "FANTASTIC_GROUP_TOKEN_RS_INTEG";
    std::env::set_var(env, "s3cret");
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp).await;
    let bid = id_for("brg_pw", &tmp);
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({
                "type": "create_agent",
                "parent_id": "kernel",
                "handler_module": WS_HANDLER_MODULE, "can_call": {"$parent": "*", "$subtree": "*"},
                "id": bid,
                "peer_id": "stand_in",
                "transport": "memory",
                "auth": {"type": "password", "env": env},
            }),
        )])
        .await
        .remove(0)
        .result;
    // the leg serves the inbound calls AS ITSELF: the root accepts the leg
    grant_edge(&kernel, &bid, "kernel_state").await;
    let peer = inject_one(&AgentId::from(bid.as_str()));
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(bid.as_str()),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;
    // reflect surfaces only the policy NAME (never the env-var config).
    let reflect = kernel
        .send(vec![Item::new(
            AgentId::from(bid.as_str()),
            json!({"type": "reflect"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(reflect["ingress_rule"], "password_ingress");
    assert_eq!(reflect["sealed"], true);

    // (1) inbound call WITH the matching envelope token dispatches.
    peer.send_frame(json!({
        "type": "call", "id": "ok", "target": "kernel_state",
        "payload": {"type": "reflect"}, "auth_token": "s3cret",
    }))
    .await
    .unwrap();
    let good = peer.recv_frame().await.expect("a reply frame").into_value();
    assert_eq!(
        good["data"]["id"], "kernel_state",
        "valid token should dispatch: {good}"
    );

    // (2) inbound call with a WRONG token is refused unauthorized.
    peer.send_frame(json!({
        "type": "call", "id": "bad", "target": "kernel_state",
        "payload": {"type": "reflect"}, "auth_token": "WRONG",
    }))
    .await
    .unwrap();
    let bad = peer.recv_frame().await.expect("a reply frame").into_value();
    assert_eq!(bad["data"]["reason"], "unauthorized", "wrong token: {bad}");

    // (3) the leg PRESENTS its group token on its own outbound forward (envelope,
    //     not the dispatched payload). Drive forward concurrently, read the frame,
    //     then answer it so the forward resolves.
    let kc = Arc::clone(&kernel);
    let bid2 = bid.clone();
    let fwd = tokio::spawn(async move {
        kc.send(vec![Item::new(
            AgentId::from(bid2.as_str()),
            json!({"type": "forward", "target": "remote", "payload": {"type": "reflect"}, "forward_timeout": 30.0}),
        )])
        .await
        .remove(0)
        .result
    });
    let out = peer
        .recv_frame()
        .await
        .expect("an outbound call frame")
        .into_value();
    assert_eq!(out["type"], "call");
    assert_eq!(
        out["auth_token"], "s3cret",
        "leg should present its group token: {out}"
    );
    assert!(
        out["payload"].get("auth_token").is_none(),
        "payload must stay clean"
    );
    peer.send_frame(json!({"type": "reply", "id": out["id"], "data": {"ok": true}}))
        .await
        .unwrap();
    let fwd_reply = fwd.await.unwrap();
    assert_eq!(fwd_reply["ok"], true);

    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(bid.as_str()),
            json!({"type": "shutdown"}),
        )])
        .await
        .remove(0)
        .result;
    std::env::remove_var(env);
}

#[tokio::test]
async fn asymmetric_ingress_egress_via_engine() {
    // A hub leg: refuse INBOUND calls, but still PRESENT a group token outbound.
    let env = "FANTASTIC_GROUP_TOKEN_RS_ASYM";
    std::env::set_var(env, "fleet");
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp).await;
    let bid = id_for("brg_asym", &tmp);
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({
                "type": "create_agent",
                "parent_id": "kernel",
                "handler_module": WS_HANDLER_MODULE, "can_call": {"$parent": "*", "$subtree": "*"},
                "id": bid,
                "peer_id": "stand_in",
                "transport": "memory",
                "ingress_rule": "deny_ingress",
                "egress_rule": {"type": "password_egress", "env": env},
            }),
        )])
        .await
        .remove(0)
        .result;
    let peer = inject_one(&AgentId::from(bid.as_str()));
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(bid.as_str()),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;
    // reflect surfaces both directions independently.
    let reflect = kernel
        .send(vec![Item::new(
            AgentId::from(bid.as_str()),
            json!({"type": "reflect"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(reflect["ingress_rule"], "deny_ingress");
    assert_eq!(reflect["egress_rule"], "password_egress");
    // no `auth` in reflect — a WRITE shorthand only (py parity)
    assert!(reflect.get("auth").is_none());
    assert_eq!(reflect["sealed"], true);

    // inbound is refused even with a token (ingress = deny_inbound, not password)
    peer.send_frame(json!({
        "type": "call", "id": "in", "target": "kernel_state",
        "payload": {"type": "reflect"}, "auth_token": "fleet",
    }))
    .await
    .unwrap();
    let denied = peer.recv_frame().await.expect("a reply frame").into_value();
    assert_eq!(
        denied["data"]["reason"], "unauthorized",
        "deny inbound: {denied}"
    );

    // outbound still presents the egress group token
    let kc = Arc::clone(&kernel);
    let bid2 = bid.clone();
    let fwd = tokio::spawn(async move {
        kc.send(vec![Item::new(
            AgentId::from(bid2.as_str()),
            json!({"type": "forward", "target": "remote", "payload": {"type": "reflect"}, "forward_timeout": 30.0}),
        )])
        .await
        .remove(0)
        .result
    });
    let out = peer
        .recv_frame()
        .await
        .expect("an outbound call frame")
        .into_value();
    assert_eq!(out["auth_token"], "fleet", "egress should present: {out}");
    peer.send_frame(json!({"type": "reply", "id": out["id"], "data": {"ok": true}}))
        .await
        .unwrap();
    let _ = fwd.await.unwrap();

    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(bid.as_str()),
            json!({"type": "shutdown"}),
        )])
        .await
        .remove(0)
        .result;
    std::env::remove_var(env);
}

// ── the re-entrant round trip + inbound identity ────────────────────

/// A bundle whose one verb forwards BACK over the bridge that just called it.
/// This is the shape that deadlocked: served inline, the read loop is parked
/// inside `kernel.send` and can never read the reply to its own outbound frame.
struct BouncerBundle;

fantastic_kernel::stand_in_verbs!(bouncer_verbs {
    Bounce "bounce" { bridge: String, back_to: String }
});

#[async_trait]
impl Handler for BouncerBundle {
    type Call = bouncer_verbs::Call;
    fn name(&self) -> &str {
        "bouncer.tools"
    }
    fn man(&self) -> &'static str {
        "A test stand-in."
    }
    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        bouncer_verbs::VERBS
    }
    fn dynamic_verbs(&self, agent: &Agent) -> Vec<fantastic_kernel::DynVerb> {
        bouncer_verbs::dynamic_verbs(agent)
    }
    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        fantastic_kernel::fixture_record!()
    }
    async fn handle(
        &self,
        agent_id: &AgentId,
        call: Self::Call,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let bouncer_verbs::Call::Bounce(b) = call else {
            return Ok(Some(json!({"id": agent_id.0, "sentence": "bouncer"})));
        };
        let out = kernel
            .send(vec![Item::new(
                AgentId::from(b.bridge.as_str()),
                json!({
                    "type": "forward",
                    "target": b.back_to,
                    "payload": {"type": "reflect"},
                    // Short, so the OLD inline code fails this in seconds rather
                    // than parking it for the 30s default.
                    "forward_timeout": 3.0,
                }),
            )])
            .await
            .remove(0)
            .result;
        Ok(Some(json!({ "bounced": out })))
    }
}

#[tokio::test]
async fn a_reentrant_forward_completes_instead_of_deadlocking() {
    // TWO kernels: A forwards to B's probe; the probe forwards straight back to A
    // over the SAME bridge. Each leg serves what arrives AS ITSELF.
    let tmp = TempDir::new().unwrap();
    let tmp_b = TempDir::new().unwrap();
    let ka = mk_kernel_guid(&tmp, "guid-a").await;
    let mut kb = Kernel::with_storage(fantastic_kernel::StorageMode::Disk {
        state_dir: tmp_b.path().join(".fantastic"),
        work_dir: tmp_b.path().to_path_buf(),
    });
    kb.bundles.register(WS_HANDLER_MODULE, WsBridgeBundle);
    kb.bundles.register("bouncer.tools", BouncerBundle);
    let kb = Arc::new(kb);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        tmp_b.path().join(".fantastic"),
        false,
    );
    kb.register(Arc::clone(&root));
    kb.set_root(Arc::clone(&root));
    patch(&kb, "kernel_state", "guid", json!("guid-b"));

    let a_id = id_for("brg_a_re", &tmp);
    let b_id = id_for("brg_b_re", &tmp);
    let probe = id_for("bouncer", &tmp);
    create_bridge(&ka, &a_id, &b_id, "memory").await;
    create_bridge(&kb, &b_id, &a_id, "memory").await;
    let _ = kb
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "create_agent", "parent_id": "kernel",
                   "handler_module": "bouncer.tools", "id": probe}),
        )])
        .await
        .remove(0)
        .result;
    // B's leg may reach the probe; the probe may reach B to forward back; A's
    // leg serves the bounce AS ITSELF, so A's root accepts A's leg.
    grant_edge(&kb, &b_id, &probe).await;
    grant_edge(&kb, &probe, &b_id).await;
    grant_edge(&ka, &a_id, "kernel_state").await;
    inject_pair(&AgentId::from(a_id.as_str()), &AgentId::from(b_id.as_str()));
    for (k, id) in [(&ka, &a_id), (&kb, &b_id)] {
        let _ = k
            .send(vec![Item::new(
                AgentId::from(id.as_str()),
                json!({"type": "boot"}),
            )])
            .await
            .remove(0)
            .result;
    }

    let reply = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        ka.send(vec![Item::new(
            AgentId::from(a_id.as_str()),
            json!({"type": "forward", "target": probe,
                   "payload": {"type": "bounce", "bridge": b_id, "back_to": "kernel_state"},
                   "forward_timeout": 10.0}),
        )]),
    )
    .await
    .expect("the round trip must not hang")
    .remove(0)
    .result;
    assert_eq!(
        reply["bounced"]["id"], "kernel_state",
        "re-entrant forward: {reply}"
    );
}

#[tokio::test]
async fn an_inbound_call_is_gated_as_the_leg_not_the_operator() {
    // The read loop was spawned with a bare `tokio::spawn`, which does not inherit
    // `CURRENT_SENDER` — so an inbound frame reached `Kernel::launch` with
    // `current_sender() == None`, the OPERATOR, and the two-sided gate never ran.
    // Here the leg is OPEN inbound and granted NOTHING outbound: an operator call
    // would list the tree, a leg call refuses on its own egress.
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp).await;
    let bid = id_for("brg_ident", &tmp);
    // A SIBLING of the leg: the `$self` baseline does not reach it, so the only
    // way the call lands is an explicit grant — or a bypassed gate.
    let target = id_for("ff_ident", &tmp);
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "create_agent", "parent_id": "kernel",
                   "handler_module": "file_bridge.tools", "allow": ["."], "id": target,
                   "root": tmp.path().to_string_lossy()}),
        )])
        .await
        .remove(0)
        .result;
    create_bridge(&kernel, &bid, "stand_in", "memory").await;
    let peer = inject_one(&AgentId::from(bid.as_str()));
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(bid.as_str()),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;

    peer.send_frame(json!({"type":"call","id":"i1","target":target,"payload":{"type":"reflect"}}))
        .await
        .unwrap();
    let reply = peer.recv_frame().await.expect("reply").into_value();
    assert_eq!(reply["id"], "i1");
    assert_eq!(
        reply["data"]["reason"], "unauthorized",
        "an inbound frame ran ungated: {reply}"
    );
    assert_eq!(reply["data"]["side"], "egress");
    assert!(
        reply["data"]["root"].is_null(),
        "the record leaked past the gate: {reply}"
    );

    // Granting the LEG the edge — both ends — is what opens it: a call a leg
    // serves is the LEG's, whoever called on the other kernel.
    grant_edge(&kernel, &bid, &target).await;
    peer.send_frame(json!({"type":"call","id":"i2","target":target,"payload":{"type":"reflect"}}))
        .await
        .unwrap();
    let ok = peer.recv_frame().await.expect("reply").into_value();
    assert_eq!(ok["data"]["id"], target, "granted edge refused: {ok}");

    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(bid.as_str()),
            json!({"type": "shutdown"}),
        )])
        .await
        .remove(0)
        .result;
}

// ─── python parity (plan §2 A7) ──────────────────────────────────

/// Create a bridge agent of either derivation with arbitrary record meta.
async fn create_leg(kernel: &Arc<Kernel>, handler: &str, id: &str, meta: Value) {
    let mut payload = serde_json::Map::new();
    payload.insert("type".into(), json!("create_agent"));
    payload.insert("parent_id".into(), json!("kernel"));
    payload.insert("handler_module".into(), json!(handler));
    payload.insert("id".into(), json!(id));
    if let Some(o) = meta.as_object() {
        for (k, v) in o {
            payload.insert(k.clone(), v.clone());
        }
    }
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            Value::Object(payload),
        )])
        .await
        .remove(0)
        .result;
}

async fn ask(kernel: &Arc<Kernel>, id: &str, payload: Value) -> Value {
    kernel
        .send(vec![Item::new(AgentId::from(id), payload)])
        .await
        .remove(0)
        .result
}

/// A kernel that carries BOTH derivations.
async fn mk_two_family_kernel(tmp: &TempDir) -> Arc<Kernel> {
    let mut kernel = Kernel::with_storage(fantastic_kernel::StorageMode::Disk {
        state_dir: tmp.path().join(".fantastic"),
        work_dir: tmp.path().to_path_buf(),
    });
    kernel.bundles.register(WS_HANDLER_MODULE, WsBridgeBundle);
    kernel
        .bundles
        .register(RELAY_HANDLER_MODULE, RelayConnectorBundle);
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
    kernel
}

/// An unknown verb is refused by the leg's own entry; `shutdown` is a verb every
/// agent answers (the teardown lives in `on_delete`), and a ws leg's directory verbs
/// are not its verbs.
#[tokio::test]
async fn unknown_type_is_refused_by_the_leg_s_entry() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_two_family_kernel(&tmp).await;
    let ws = id_for("a72_ws", &tmp);
    create_leg(&kernel, WS_HANDLER_MODULE, &ws, json!({"transport": "ws"})).await;
    let r = ask(&kernel, &ws, json!({"type": "garbage"})).await;
    assert_eq!(r["reason"], json!("unhandled"));
    assert!(
        r["error"]
            .as_str()
            .unwrap()
            .starts_with("unknown verb 'garbage'; "),
        "{r}"
    );
    // `shutdown` is a common verb the shared entry answers with null.
    assert_eq!(ask(&kernel, &ws, json!({"type": "shutdown"})).await, Value::Null);
    // The directory verbs belong to relay_connector only.
    for verb in [
        "list_peers",
        "watch_directory",
        "unwatch_directory",
        "set_identity",
    ] {
        let r = ask(&kernel, &ws, json!({ "type": verb })).await;
        assert_eq!(r["reason"], json!("unhandled"), "{verb}: {r}");
        assert_eq!(r["verb"], json!(verb), "{verb} must not exist on a ws leg");
    }
}

/// The twin of python `test_leg_gates.py::
/// test_a_bridge_reflects_exactly_what_its_gate_enforces`: `auth` carries a bare
/// STEM and the gate appends `_ingress`, so `auth: allow_all_ingress` resolves
/// `allow_all_ingress_ingress` — unknown, and it denies. The bridge reflect is
/// the legs' one `describe`, so it reports exactly that rule and `sealed: true`.
#[tokio::test]
async fn a_bridge_reflects_exactly_what_its_gate_enforces() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_two_family_kernel(&tmp).await;
    let br = id_for("br_gate", &tmp);
    create_leg(
        &kernel,
        WS_HANDLER_MODULE,
        &br,
        json!({"transport": "memory", "auth": "allow_all_ingress"}),
    )
    .await;
    assert!(kernel.agents.contains_key(&AgentId::from(br.as_str())));
    let rule = authorizer::ingress::resolve(
        rule_spec(&AgentId::from(br.as_str()), &kernel, "ingress_rule").as_ref(),
    );
    let Decision::Deny(denial) = rule.authorize(&call_action(None)) else {
        panic!("an unknown ingress rule must deny");
    };
    assert_eq!(
        denial.reason,
        "edge sealed — unknown ingress rule 'allow_all_ingress_ingress'"
    );
    let r = ask(&kernel, &br, json!({"type": "reflect"})).await;
    assert_eq!(
        (&r["ingress_rule"], &r["egress_rule"], &r["sealed"]),
        (
            &json!("allow_all_ingress_ingress"),
            &json!("allow_all_ingress_egress"),
            &json!(true)
        ),
        "{r}"
    );
    // The rest of python's `describe` table, as python's ws_bridge reflects it. A
    // TYPE-LESS spec (`{type: ""}`) is no rule at all — the manifold defaults —
    // never the empty stem scoped to `_ingress`.
    for (i, (auth, ingress, egress, sealed)) in [
        (json!({"type": ""}), "deny_ingress", "silent_egress", true),
        (
            json!("allow_all"),
            "allow_all_ingress",
            "silent_egress",
            false,
        ),
        (
            json!({"type": "password", "env": "X"}),
            "password_ingress",
            "password_egress",
            true,
        ),
        (json!("deny"), "deny_ingress", "silent_egress", true),
        (json!("silent"), "silent_ingress", "silent_egress", true),
    ]
    .into_iter()
    .enumerate()
    {
        let id = id_for(&format!("br_gate{i}"), &tmp);
        create_leg(
            &kernel,
            WS_HANDLER_MODULE,
            &id,
            json!({"transport": "memory", "auth": auth}),
        )
        .await;
        let r = ask(&kernel, &id, json!({"type": "reflect"})).await;
        assert_eq!(
            (&r["ingress_rule"], &r["egress_rule"], &r["sealed"]),
            (&json!(ingress), &json!(egress), &json!(sealed)),
            "auth={auth}: {r}"
        );
    }
}

/// Reflect's key order, sentence, default
/// transport, family fields, verbs and emits are python's.
#[tokio::test]
async fn reflect_key_order_per_family() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_two_family_kernel(&tmp).await;
    let ws = id_for("a76_ws", &tmp);
    let relay = id_for("a76_relay", &tmp);
    create_leg(&kernel, WS_HANDLER_MODULE, &ws, json!({"transport": "ws"})).await;
    create_leg(
        &kernel,
        RELAY_HANDLER_MODULE,
        &relay,
        json!({"transport": "relay", "heartbeat": 0, "reconnect": 0}),
    )
    .await;

    let r = ask(&kernel, &ws, json!({"type": "reflect"})).await;
    // (the kernel appends its own gate keys after the bundle's body)
    let keys: Vec<&str> = r
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .take(11)
        .collect();
    assert_eq!(
        keys,
        vec![
            "id",
            "sentence",
            "transport",
            "connected",
            "pending_count",
            "ingress_rule",
            "egress_rule",
            "sealed",
            "host",
            "peer_id",
            "port",
        ]
    );
    assert_eq!(
        r["sentence"],
        json!(crate::WS_MAN)
    );
    assert_eq!(r["transport"], json!("ws"), "the record's transport");
    assert_eq!(r["connected"], json!(false));
    assert_eq!(r["sealed"], json!(true));
    // the declared verbs, in declaration order
    let names: Vec<&str> = Handler::verbs(&WsBridgeBundle).iter().map(|v| v.name).collect();
    assert_eq!(
        names,
        ["boot", "reconnect", "forward", "watch_remote", "unwatch_remote"]
    );

    let r = ask(&kernel, &relay, json!({"type": "reflect"})).await;
    let keys: Vec<&str> = r
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .take(15)
        .collect();
    assert_eq!(
        keys,
        vec![
            "id",
            "sentence",
            "transport",
            "connected",
            "pending_count",
            "ingress_rule",
            "egress_rule",
            "sealed",
            "relay_url",
            "guid",
            "partner_guid",
            "reconnect",
            "role",
            "owner_guid",
            "exposes",
        ]
    );
    assert_eq!(
        r["sentence"],
        json!(crate::RELAY_MAN)
    );
    assert_eq!(r["transport"], json!("relay"), "the record's transport");
    assert_eq!(r["reconnect"], json!(0));
    assert_eq!(r["role"], Value::Null, "no role declared: none reported");
    assert_eq!(r["exposes"], Value::Null, "nothing exposed is declared: none reported");
    let names: Vec<&str> = Handler::verbs(&RelayConnectorBundle)
        .iter()
        .map(|v| v.name)
        .collect();
    assert_eq!(
        names,
        [
            "boot",
            "reconnect",
            "forward",
            "watch_remote",
            "unwatch_remote",
            "list_peers",
            "watch_directory",
            "unwatch_directory",
            "set_identity",
        ]
    );
    let unwatch = Handler::verbs(&RelayConnectorBundle)
        .iter()
        .find(|v| v.name == "unwatch_directory")
        .expect("declared");
    assert_eq!(
        unwatch.doc(),
        "Stop the directory subscription `watch_directory` started."
    );
}

/// Boot's error texts are python's, and a relay leg that is
/// missing `partner_guid` mints no guid.
#[tokio::test]
async fn boot_error_texts_match_python() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_two_family_kernel(&tmp).await;

    // memory with no injection
    let mem = id_for("a713_mem", &tmp);
    create_leg(
        &kernel,
        WS_HANDLER_MODULE,
        &mem,
        json!({"transport": "memory"}),
    )
    .await;
    assert_eq!(
        ask(&kernel, &mem, json!({"type": "boot"})).await,
        json!({"error": "bridge: memory transport requires test injection"})
    );

    // ws with no peer_id / no port — python's ValueError texts, wrapped.
    let ws = id_for("a713_ws", &tmp);
    create_leg(&kernel, WS_HANDLER_MODULE, &ws, json!({"transport": "ws"})).await;
    assert_eq!(
        ask(&kernel, &ws, json!({"type": "boot"})).await,
        json!({"error": "bridge: ws boot failed: peer_id required for the ws transport"})
    );
    let ws2 = id_for("a713_ws2", &tmp);
    create_leg(
        &kernel,
        WS_HANDLER_MODULE,
        &ws2,
        json!({"transport": "ws", "peer_id": "p"}),
    )
    .await;
    assert_eq!(
        ask(&kernel, &ws2, json!({"type": "boot"})).await,
        json!({"error": "bridge: ws boot failed: ws transport requires port"})
    );
    // an unknown kind is refused at the write, by the record's declaration.
    let odd = id_for("a713_odd", &tmp);
    let r = ask(
        &kernel,
        "kernel",
        json!({"type": "create_agent", "parent_id": "kernel", "id": odd,
               "handler_module": WS_HANDLER_MODULE, "transport": "carrier-pigeon",
               "peer_id": "p"}),
    )
    .await;
    assert_eq!(r["reason"], json!("bad_arg"), "{r}");
    assert_eq!(r["arg"], json!("transport"), "{r}");

    // an egress rule the registry does not know refuses the boot, before dialing
    // (the leg's rules are read per frame — boot caches none).
    let bad = id_for("a713_bad", &tmp);
    create_leg(
        &kernel,
        WS_HANDLER_MODULE,
        &bad,
        json!({"transport": "memory", "egress_rule": "nope_egress"}),
    )
    .await;
    inject_pair(
        &AgentId::from(bad.as_str()),
        &AgentId::from("a713_bad_peer"),
    );
    let r = ask(&kernel, &bad, json!({"type": "boot"})).await;
    assert_eq!(
        r,
        json!({"error": "bridge: bad auth rule: unknown egress rule type 'nope_egress'"})
    );
}

/// A relay boot that fails validation mints NO guid.
#[tokio::test]
async fn relay_boot_missing_partner_mints_no_guid() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_two_family_kernel(&tmp).await;
    let relay = id_for("a714_relay", &tmp);
    create_leg(
        &kernel,
        RELAY_HANDLER_MODULE,
        &relay,
        json!({"transport": "relay", "relay_url": "ws://127.0.0.1:1", "heartbeat": 0, "reconnect": 0}),
    )
    .await;
    assert_eq!(
        ask(&kernel, &relay, json!({"type": "boot"})).await,
        json!({
            "error": "bridge: relay boot failed: relay_connector requires relay_url, partner_guid"
        })
    );
    assert_eq!(
        ask(&kernel, &relay, json!({"type": "reflect"})).await["guid"],
        Value::Null,
        "a refused boot leaves no identity behind"
    );
    // A bad `role` is python's ValueError text, ROLES repr and all. The guid is
    // minted BEFORE that check, through `kernel.update` — so the `updated` state
    // event the root loader persists from is published (python `_relay.py`).
    let seen: Arc<std::sync::Mutex<Vec<Value>>> = Arc::default();
    let sink = Arc::clone(&seen);
    let _tap = kernel.add_state_subscriber(Arc::new(move |ev, _| {
        sink.lock().unwrap().push(ev.clone());
    }));
    let bad_role = id_for("a714_role", &tmp);
    create_leg(
        &kernel,
        RELAY_HANDLER_MODULE,
        &bad_role,
        json!({"transport": "relay", "relay_url": "ws://127.0.0.1:1",
               "partner_guid": "B", "heartbeat": 0, "reconnect": 0,
               "credentials": {"store": "creds", "ref": RELAY_REF}}),
    )
    .await;
    // the dial fails AFTER the guid is minted
    let booted = ask(&kernel, &bad_role, json!({"type": "boot"})).await;
    assert!(
        booted["error"]
            .as_str()
            .is_some_and(|e| e.starts_with("bridge: relay boot failed: ")),
        "{booted}"
    );
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .any(|e| e["agent_id"] == json!(bad_role)
                && e["kind"] == "updated"
                && e["changed"] == json!(["guid"])),
        "the guid mint published no `updated` event"
    );
}

/// `connected` follows the transport's closed state, and a closed
/// memory leg is REBUILT by the next boot instead of answering `already`.
#[tokio::test]
async fn reflect_connected_false_after_peer_close_and_boot_rebuilds() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp).await;
    let bid = id_for("a710", &tmp);
    create_bridge(&kernel, &bid, "peer", "memory").await;
    let peer = inject_one(&AgentId::from(bid.as_str()));
    assert_eq!(
        ask(&kernel, &bid, json!({"type": "boot"})).await,
        json!({"booted": true, "transport": "memory"})
    );
    assert_eq!(
        ask(&kernel, &bid, json!({"type": "reflect"})).await["connected"],
        json!(true)
    );
    // Re-booting a LIVE leg is a no-op.
    assert_eq!(
        ask(&kernel, &bid, json!({"type": "boot"})).await,
        json!({"already": true, "transport": "memory"})
    );
    peer.close().await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        ask(&kernel, &bid, json!({"type": "reflect"})).await["connected"],
        json!(false),
        "a peer that hung up is not connected"
    );
    // …and a closed leg boots again (python: `already` only while open).
    inject_pair(&AgentId::from(bid.as_str()), &AgentId::from("a710_peer2"));
    assert_eq!(
        ask(&kernel, &bid, json!({"type": "boot"})).await,
        json!({"booted": true, "transport": "memory"})
    );
}

/// Forward's refusals and error texts are python's.
#[tokio::test]
async fn forward_error_texts_match_python() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp).await;
    let bid = id_for("a715", &tmp);
    create_bridge(&kernel, &bid, "peer", "memory").await;
    // validation: the declaration names the missing argument.
    let r = ask(&kernel, &bid, json!({"type": "forward", "target": "x", "forward_timeout": 30.0})).await;
    assert_eq!(r["error"], json!("forward: missing argument 'payload'"), "{r}");
    assert_eq!(r["reason"], json!("missing_arg"), "{r}");
    // pre-boot
    let r = ask(
        &kernel,
        &bid,
        json!({"type": "forward", "target": "x", "payload": {"type": "reflect"}, "forward_timeout": 30.0}),
    )
    .await;
    assert_eq!(r["error"], json!("bridge.forward: not connected"));
    assert_eq!(r["reason"], json!("not_connected"));
    assert_eq!(r["hint"], json!("call boot first"));

    // timeout — python renders the float (`30.0s`, not `30s`).
    let peer = inject_one(&AgentId::from(bid.as_str()));
    let _ = ask(&kernel, &bid, json!({"type": "boot"})).await;
    let r = ask(
        &kernel,
        &bid,
        json!({"type": "forward", "target": "x", "payload": {"type": "reflect"}, "forward_timeout": 0.05}),
    )
    .await;
    assert_eq!(r, json!({"error": "bridge.forward: timeout after 0.05s"}));

    // an inbound `error` frame RESOLVES the forward with python's text.
    let call = peer
        .recv_frame()
        .await
        .expect("the timed-out call")
        .into_value();
    let _ = call;
    let bridge_id = bid.clone();
    let peer2 = Arc::clone(&peer);
    let kernel2 = Arc::clone(&kernel);
    let answer = tokio::spawn(async move {
        let frame = peer2.recv_frame().await.expect("call").into_value();
        let id = frame["id"].as_str().unwrap_or("").to_string();
        let _ = peer2
            .send_frame(json!({"type": "error", "id": id, "error": "boom"}))
            .await;
    });
    let r = ask(
        &kernel2,
        &bridge_id,
        json!({"type": "forward", "target": "x", "payload": {"type": "reflect"}, "forward_timeout": 5.0}),
    )
    .await;
    answer.await.unwrap();
    assert_eq!(r, json!({"error": "remote error: boom"}));
}

/// The directory replies are python's shapes.
#[tokio::test]
async fn directory_replies_match_python() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_two_family_kernel(&tmp).await;
    let relay = id_for("a716", &tmp);
    create_leg(
        &kernel,
        RELAY_HANDLER_MODULE,
        &relay,
        json!({"transport": "memory", "heartbeat": 0, "reconnect": 0}),
    )
    .await;
    // pre-boot: unwatch_directory is simply ok; the others refuse.
    assert_eq!(
        ask(&kernel, &relay, json!({"type": "unwatch_directory"})).await,
        json!({"ok": true})
    );
    // the declared 30 s default stands in for an absent `list_timeout`
    let r = ask(&kernel, &relay, json!({"type": "list_peers"})).await;
    assert_eq!(
        r["error"],
        json!("relay_connector.list_peers: not connected")
    );
    assert_eq!(r["reason"], json!("not_connected"));
    assert_eq!(r["hint"], json!("call boot first"));
    // a bad role is a bad_arg refusal naming the arg, by the declaration — before
    // the connection is looked at.
    let r = ask(
        &kernel,
        &relay,
        json!({"type": "set_identity", "role": "boss"}),
    )
    .await;
    assert_eq!(r["reason"], json!("bad_arg"), "{r}");
    assert_eq!(r["arg"], json!("role"), "{r}");
}

/// A reconnect announces exactly one `bridge_down`, then `bridge_up`.
#[tokio::test]
async fn reconnect_emits_bridge_down_then_bridge_up() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp).await;
    let bid = id_for("a717", &tmp);
    create_bridge(&kernel, &bid, "peer", "memory").await;
    let _peer = inject_one(&AgentId::from(bid.as_str()));
    let mut rx = kernel.ensure_inbox(&AgentId::from(bid.as_str()));
    let _ = ask(&kernel, &bid, json!({"type": "boot"})).await;
    inject_pair(&AgentId::from(bid.as_str()), &AgentId::from("a717_peer2"));
    let _ = ask(&kernel, &bid, json!({"type": "reconnect"})).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let mut kinds: Vec<String> = Vec::new();
    while let Some(v) = rx.try_recv() {
        if let Some(t) = v.payload.get("type").and_then(Value::as_str) {
            kinds.push(t.to_string());
        }
    }
    // A reader on the bridge's OWN inbox sees the REQUESTS too, each mirrored
    // BEFORE its handler runs — python's `>reconnect reconnect bridge_down
    // bridge_up` — so the echo never lands behind the events the handler emits.
    assert_eq!(
        kinds,
        vec!["boot", "bridge_up", "reconnect", "bridge_down", "bridge_up"],
        "each request precedes its own events; exactly one bridge_down per teardown"
    );
}

/// A booted leg's work is registered as python names it: `read_loop`
/// (both derivations), and each registration is a `task` ROW under the
/// id `ps` reports.
#[tokio::test]
async fn booted_bridge_ps_rows_match_python() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp).await;
    let bid = id_for("a718", &tmp);
    create_bridge(&kernel, &bid, "peer", "memory").await;
    let _peer = inject_one(&AgentId::from(bid.as_str()));
    let _ = ask(&kernel, &bid, json!({"type": "boot"})).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let table = ask(&kernel, "kernel", json!({"type": "ps"})).await;
    let verbs: Vec<&str> = table["tasks"]
        .as_array()
        .expect("tasks")
        .iter()
        .filter(|r| r["agent"] == json!(bid.as_str()))
        .filter_map(|r| r["verb"].as_str())
        .collect();
    assert_eq!(
        verbs,
        vec!["read_loop"],
        "python names the task `read_loop`"
    );
    // The fork is a MESSAGE: one `task` row on the leg, and the pid in
    // `ps` is that row's id.
    let store: Vec<_> =
        fantastic_kernel::test_support::rows_about(&kernel, &AgentId::from(bid.as_str()))
            .into_iter()
            .filter(|r| r.get("kind") == Some(&json!("task")))
            .collect();
    assert_eq!(
        store
            .iter()
            .filter_map(|r| r.get("verb").and_then(Value::as_str))
            .collect::<Vec<_>>(),
        vec!["read_loop"],
        "{store:?}"
    );
    let row_id = store[0]["id"].as_i64().expect("a row id");
    assert!(
        table["tasks"]
            .as_array()
            .expect("tasks")
            .iter()
            .any(|r| r["id"] == json!(row_id)),
        "the task id is the row id"
    );
}

/// A remote `{type:'event', src, payload}` is re-emitted (payload + `src`) ONLY for
/// a `src` this leg subscribed to with `watch_remote`: nothing before the
/// subscription, nothing without a `src` or a payload object, nothing after
/// `unwatch_remote`.
#[tokio::test]
async fn a_remote_event_flows_only_for_a_subscribed_src() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp).await;
    let bid = id_for("a73", &tmp);
    create_bridge(&kernel, &bid, "peer", "memory").await;
    let peer = inject_one(&AgentId::from(bid.as_str()));
    let mut rx = kernel.ensure_inbox(&AgentId::from(bid.as_str()));
    let _ = ask(&kernel, &bid, json!({"type": "boot"})).await;
    let drain = |rx: &mut fantastic_kernel::inbox::InboxReceiver| {
        let mut seen: Vec<Value> = Vec::new();
        while let Some(v) = rx.try_recv() {
            seen.push(v.payload);
        }
        seen
    };
    let settle = || tokio::time::sleep(std::time::Duration::from_millis(150));
    let ev = |src: Option<&str>, payload: Option<Value>| {
        let mut f = json!({"type": "event"});
        if let Some(s) = src {
            f["src"] = json!(s);
        }
        if let Some(p) = payload {
            f["payload"] = p;
        }
        f
    };
    peer.send_frame(ev(Some("ai"), Some(json!({"type": "token"}))))
        .await
        .unwrap();
    settle().await;
    let before = drain(&mut rx);
    assert!(
        !before.iter().any(|v| v["type"] == "token"),
        "an unsubscribed event leaked: {before:?}"
    );

    let w = ask(
        &kernel,
        &bid,
        json!({"type": "watch_remote", "target": "ai"}),
    )
    .await;
    assert_eq!(w, json!({"ok": true, "watching": "ai"}));
    assert_eq!(
        peer.recv_frame().await.unwrap().into_value(),
        json!({"type": "watch", "src": "ai"})
    );
    drain(&mut rx);
    for frame in [
        ev(None, Some(json!({"type": "token", "text": "no-src"}))),
        ev(
            Some("ghost"),
            Some(json!({"type": "token", "text": "ghost"})),
        ),
        ev(Some("ai"), None),
        ev(Some("ai"), Some(json!({"type": "token", "text": "hi"}))),
    ] {
        peer.send_frame(frame).await.unwrap();
    }
    settle().await;
    let seen: Vec<Value> = drain(&mut rx)
        .into_iter()
        .filter(|v| v["type"] == "token")
        .collect();
    assert_eq!(
        seen,
        vec![json!({"type": "token", "text": "hi", "src": "ai"})]
    );

    let _ = ask(
        &kernel,
        &bid,
        json!({"type": "unwatch_remote", "target": "ai"}),
    )
    .await;
    let _ = peer.recv_frame().await;
    peer.send_frame(ev(
        Some("ai"),
        Some(json!({"type": "token", "text": "late"})),
    ))
    .await
    .unwrap();
    settle().await;
    let after = drain(&mut rx);
    assert!(
        !after.iter().any(|v| v["type"] == "token"),
        "leaked after unwatch: {after:?}"
    );
}

/// A frame that does not decode (or decodes to a non-object) is SKIPPED
/// and the reader keeps going, exactly as python's `WSTransport.recv` loops
/// (`ws_bridge/_ws.py:67-81`). Before the fix the first bad frame became a
/// `TransportError::Other` that broke the engine read loop while the transport
/// still reported `connected:true`.
#[tokio::test]
async fn a_malformed_frame_is_skipped_and_the_reader_survives() {
    use futures_util::SinkExt;
    use tokio_tungstenite::tungstenite::Message;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        // 1. text that is not JSON            → "skipping malformed frame"
        ws.send(Message::Text("{not json".into())).await.unwrap();
        // 2. text that decodes to a non-dict  → "skipping non-object frame"
        ws.send(Message::Text("[1, 2, 3]".into())).await.unwrap();
        // 3. a binary frame whose 4-byte length prefix overruns the frame
        ws.send(Message::Binary(vec![0, 0, 0, 200, 1, 2, 3]))
            .await
            .unwrap();
        // 4. a good frame — the reader must still be alive to deliver it.
        ws.send(Message::Text(
            serde_json::to_string(&json!({"type": "reply", "id": "c1", "data": {"ok": true}}))
                .unwrap(),
        ))
        .await
        .unwrap();
        // Hold the socket open so the client sees no close frame first.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    });

    let t = transport::ws::WsTransport::connect(&format!("ws://127.0.0.1:{port}/peer/ws"))
        .await
        .expect("connect to the local ws server");
    let frame = tokio::time::timeout(std::time::Duration::from_secs(5), t.recv_frame())
        .await
        .expect("the reader must survive three bad frames")
        .expect("the fourth frame decodes");
    assert_eq!(
        frame.into_value(),
        json!({"type": "reply", "id": "c1", "data": {"ok": true}}),
        "the first DECODABLE frame is what recv hands back"
    );
    t.close().await;
    // python `_ws.send` raises `ConnectionClosed(str(e))`: the library's text,
    // with no `ws send:` prefix of the port's own.
    match t.send_frame(json!({"type": "ping"})).await {
        Err(transport::TransportError::ConnectionClosed(m)) => {
            assert!(!m.starts_with("ws send"), "a rust-only prefix: {m}");
            assert!(!m.is_empty());
        }
        other => panic!("a send on a closed socket must fail: {other:?}"),
    }
    server.abort();
}

/// `forward_timeout` (and `list_timeout`) is a number: the declaration refuses
/// `null` or a string by name, none is coerced to one.
#[tokio::test]
async fn forward_timeout_is_a_number() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_two_family_kernel(&tmp).await;
    let leg = id_for("a739_leg", &tmp);
    create_leg(
        &kernel,
        WS_HANDLER_MODULE,
        &leg,
        json!({"transport": "memory"}),
    )
    .await;
    let fwd = |timeout: Value| {
        json!({"type": "forward", "target": "x", "payload": {"type": "reflect"},
               "forward_timeout": timeout})
    };
    // a `forward_timeout` that is not a number is refused by name — no string is
    // coerced to one; an ABSENT one takes the declared 30 s default
    let r = ask(&kernel, &leg, fwd(Value::Null)).await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(r["arg"], "forward_timeout", "{r}");
    let r = ask(
        &kernel,
        &leg,
        json!({"type": "forward", "target": "x", "payload": {"type": "reflect"}}),
    )
    .await;
    assert_eq!(r["reason"], "not_connected", "{r}");
    for bad in [json!("soon"), json!("5")] {
        let r = ask(&kernel, &leg, fwd(bad.clone())).await;
        assert_eq!(r["reason"], "bad_arg", "{bad}: {r}");
        assert_eq!(r["arg"], "forward_timeout", "{bad}: {r}");
    }
    // a number reaches the connection check
    let r = ask(&kernel, &leg, fwd(json!(5))).await;
    assert_eq!(r["reason"], "not_connected", "{r}");
}

/// An unknown verb is refused by each leg's own entry, naming what the leg answers;
/// the relay's directory verbs are not a ws leg's. The raw `type` is echoed back.
#[tokio::test]
async fn an_unknown_verb_is_refused_by_the_leg_on_both_families() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_two_family_kernel(&tmp).await;
    let ws = id_for("s4_ws", &tmp);
    create_leg(&kernel, WS_HANDLER_MODULE, &ws, json!({"transport": "ws"})).await;
    let relay = id_for("s4_relay", &tmp);
    create_leg(
        &kernel,
        RELAY_HANDLER_MODULE,
        &relay,
        json!({"transport": "relay", "relay_url": "ws://127.0.0.1:1", "heartbeat": 0, "reconnect": 0}),
    )
    .await;
    for (id, answers) in [
        (
            &ws,
            "boot, reconnect, forward, watch_remote, unwatch_remote",
        ),
        (
            &relay,
            "boot, reconnect, forward, watch_remote, unwatch_remote, list_peers, \
             watch_directory, unwatch_directory, set_identity",
        ),
    ] {
        let r = ask(&kernel, id, json!({"type": "garbage"})).await;
        assert_eq!(r["reason"], json!("unhandled"), "{id}");
        assert_eq!(r["verb"], json!("garbage"), "{id}");
        let error = r["error"].as_str().unwrap();
        assert!(
            error.starts_with("unknown verb 'garbage'; ") && error.contains(answers),
            "{id}: {error}"
        );
    }
    // the directory verbs are the relay's only
    let r = ask(&kernel, &ws, json!({"type": "list_peers"})).await;
    assert_eq!(r["reason"], json!("unhandled"), "{r}");
    assert_eq!(r["verb"], json!("list_peers"), "{r}");
}

/// A relay leg's minted `guid` is python's
/// `uuid.uuid4().hex` — 32 lowercase hex, version nibble 4, RFC-4122 variant.
#[tokio::test]
async fn a_minted_relay_guid_is_a_uuid4_hex() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_two_family_kernel(&tmp).await;
    let mut seen = std::collections::HashSet::new();
    for n in 0..4 {
        let relay = id_for(&format!("s4_guid{n}"), &tmp);
        // a failed dial fails the boot AFTER the guid is minted
        create_leg(
            &kernel,
            RELAY_HANDLER_MODULE,
            &relay,
            json!({"transport": "relay", "relay_url": "ws://127.0.0.1:1",
                   "partner_guid": "B", "heartbeat": 0, "reconnect": 0,
                   "credentials": {"store": "creds", "ref": RELAY_REF}}),
        )
        .await;
        let _ = ask(&kernel, &relay, json!({"type": "boot"})).await;
        let g = ask(&kernel, &relay, json!({"type": "reflect"})).await["guid"]
            .as_str()
            .unwrap_or_else(|| panic!("no guid minted on {relay}"))
            .to_string();
        assert_eq!(g.len(), 32, "{g}");
        assert!(
            g.chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "{g}"
        );
        assert_eq!(&g[12..13], "4", "uuid4 version nibble: {g}");
        assert!(matches!(&g[16..17], "8" | "9" | "a" | "b"), "variant: {g}");
        seen.insert(g);
    }
    assert_eq!(seen.len(), 4);
}

// ─── the bridge's BINARY dispatch ───────────────────────────────────────────

/// A booted ws leg on an injected memory transport; returns the far end.
async fn booted_leg(
    tmp: &TempDir,
    tag: &str,
) -> (
    Arc<Kernel>,
    String,
    Arc<dyn crate::transport::BridgeTransport>,
) {
    let kernel = mk_kernel_guid(tmp, "g-local").await;
    let bid = id_for(tag, tmp);
    create_bridge_with_auth(&kernel, &bid, "stand_in", "allow_all").await;
    let peer = inject_one(&AgentId::from(bid.as_str()));
    let booted = ask(&kernel, &bid, json!({"type": "boot"})).await;
    assert!(booted.get("error").is_none(), "{booted}");
    // the leg's first frame names its kernel to the far side
    let hello = peer.recv_frame().await.expect("hello").into_value();
    assert_eq!(
        hello,
        json!({"type": "hello", "guid": "g-local", "leg": bid})
    );
    (kernel, bid, peer)
}

async fn send_bin(
    kernel: &Arc<Kernel>,
    id: &str,
    header: Value,
    blob: Vec<u8>,
) -> fantastic_kernel::Outcome {
    send_bin_at(kernel, id, header, vec![Step::from("bytes")], blob).await
}

/// [`send_bin`] with the body at `path` of the payload.
async fn send_bin_at(
    kernel: &Arc<Kernel>,
    id: &str,
    header: Value,
    path: Vec<Step>,
    blob: Vec<u8>,
) -> fantastic_kernel::Outcome {
    kernel
        .send(vec![
            Item::new(AgentId::from(id), header).with_binary(path, blob)
        ])
        .await
        .remove(0)
}

/// A NON-`forward` verb on the binary channel takes the TEXT dispatch: the same
/// reply the text channel gives, and no body.
#[tokio::test]
async fn a_binary_non_forward_verb_is_answered_by_the_text_dispatch() {
    let tmp = TempDir::new().unwrap();
    let (kernel, bid, _peer) = booted_leg(&tmp, "s4_bin_text").await;
    for header in [json!({"type": "garbage"}), json!({"type": "forward"})] {
        let bin = send_bin(&kernel, &bid, header.clone(), vec![1, 2, 3]).await;
        let txt = ask(&kernel, &bid, header.clone()).await;
        if header["type"] == "forward" {
            // `forward` IS the binary verb: it validates its own envelope
            assert_eq!(
                bin.result["error"],
                json!("forward: missing argument 'payload'"),
                "{:?}",
                bin.result
            );
        } else {
            assert_eq!(bin.result, txt, "{header}");
        }
        assert_eq!(bin.body, None, "{header}");
    }
    let bin = send_bin(&kernel, &bid, json!({"type": "reflect"}), vec![1]).await;
    assert_eq!(bin.result["id"], json!(bid));
    assert_eq!(bin.result["transport"], json!("memory"));
    assert_eq!(bin.body, None);
}

/// A binary `forward` ships ONE binary `call` frame — the body raw beside the
/// header at the path it had inside the forwarded payload (`_binary_path:
/// ["payload", "bytes"]`, `bytes: null` in python's position) — and a binary
/// reply's body comes back as the reply body, at its slot of the reply.
#[tokio::test]
async fn a_binary_forward_ships_raw_bytes_and_returns_the_reply_body() {
    let tmp = TempDir::new().unwrap();
    let (kernel, bid, peer) = booted_leg(&tmp, "s4_bin_fwd").await;
    let k = Arc::clone(&kernel);
    let b = bid.clone();
    let join = tokio::spawn(async move {
        send_bin_at(
            &k,
            &b,
            json!({"type": "forward", "target": "far_fb",
                   "payload": {"type": "write_stream", "path": "x.bin"},
                   "forward_timeout": 30.0}),
            vec![Step::from("payload"), Step::from("bytes")],
            vec![0x00, 0xFF, 0x10],
        )
        .await
    });
    let (header, body) = match peer.recv_frame().await.expect("a call frame") {
        crate::transport::Frame::Binary(h, b) => (h, b),
        crate::transport::Frame::Text(v) => panic!("expected a binary call, got text: {v}"),
    };
    assert_eq!(body.bytes, vec![0x00, 0xFF, 0x10], "the blob rides raw");
    assert_eq!(body.path, vec![Step::from("payload"), Step::from("bytes")]);
    let corr = header["id"].as_str().expect("a correlator").to_string();
    // `<leg id>-<boot nonce>-<n>`: dash, not colon — never a kernel pid
    let nonce = corr
        .strip_prefix(&format!("{bid}-"))
        .and_then(|rest| rest.strip_suffix("-1"))
        .unwrap_or_else(|| panic!("{corr}"));
    assert!(
        nonce.len() == 12 && nonce.bytes().all(|b| b.is_ascii_hexdigit()),
        "{corr}"
    );
    assert!(!corr.contains(':'), "dash, not colon: never a kernel pid");
    assert_eq!(
        header,
        json!({"type": "call", "id": corr, "target": "far_fb",
               "payload": {"type": "write_stream", "path": "x.bin", "bytes": null}})
    );
    // on the wire the codec names the slot last, as a list of steps
    let wire = fantastic_io_bridge::codec::encode_binary_frame(&header, &body.path, &body.bytes);
    let h = u32::from_be_bytes([wire[0], wire[1], wire[2], wire[3]]) as usize;
    let head: Value = serde_json::from_slice(&wire[4..4 + h]).unwrap();
    assert_eq!(head["_binary_path"], json!(["payload", "bytes"]));
    let keys: Vec<&str> = head
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        vec!["type", "id", "target", "payload", "_binary_path"]
    );
    peer.send_binary(
        json!({"type": "reply", "id": corr, "data": {"eof": true, "bytes": null}}),
        Blob::new(vec![Step::from("data"), Step::from("bytes")], vec![7, 7]),
    )
    .await
    .unwrap();
    let out = join.await.unwrap();
    assert_eq!(out.result, json!({"eof": true, "bytes": null}));
    assert_eq!(
        out.body,
        Some(Blob::key("bytes", vec![7, 7])),
        "a binary reply's body is the reply body, at its slot of the reply"
    );

    // a TEXT reply frame to a binary forward carries no chunk at all
    let k = Arc::clone(&kernel);
    let b = bid.clone();
    let join = tokio::spawn(async move {
        send_bin(
            &k,
            &b,
            json!({"type": "forward", "target": "far_fb", "payload": {"type": "stat"}, "forward_timeout": 30.0}),
            vec![1],
        )
        .await
    });
    let header = peer.recv_frame().await.expect("a call frame").into_value();
    let corr = header["id"].as_str().expect("a correlator");
    assert!(
        corr.starts_with(&format!("{bid}-")) && corr.ends_with("-2"),
        "this boot's nonce, the second forward: {corr}"
    );
    peer.send_frame(json!({"type": "reply", "id": header["id"], "data": {"ok": 1}}))
        .await
        .unwrap();
    let out = join.await.unwrap();
    assert_eq!(out.result, json!({"ok": 1}));
    assert_eq!(out.body, None);
}

/// A binary `forward` on a leg that is not connected is the coded refusal.
#[tokio::test]
async fn a_binary_forward_before_boot_is_not_connected() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp).await;
    let bid = id_for("s4_bin_nc", &tmp);
    create_bridge_with_auth(&kernel, &bid, "stand_in", "allow_all").await;
    let out = send_bin(
        &kernel,
        &bid,
        json!({"type": "forward", "target": "far", "payload": {"type": "x"}, "forward_timeout": 30.0}),
        vec![1],
    )
    .await;
    assert_eq!(
        serde_json::to_string(&out.result).unwrap(),
        r#"{"error":"bridge.forward: not connected","reason":"not_connected","hint":"call boot first"}"#
    );
    assert_eq!(out.body, None);
}

/// The two bundle shells differ ONLY in family and name — the same binary routing, the same delete teardown.
#[tokio::test]
async fn both_family_shells_share_one_behaviour() {
    assert_eq!(Handler::name(&WsBridgeBundle), "ws_bridge");
    assert_eq!(Handler::name(&RelayConnectorBundle), "relay_connector");
    let tmp = TempDir::new().unwrap();
    let kernel = mk_two_family_kernel(&tmp).await;
    for (hm, tag) in [
        (WS_HANDLER_MODULE, "s4_fam_ws"),
        (RELAY_HANDLER_MODULE, "s4_fam_rl"),
    ] {
        // binary: a non-forward verb is the text dispatch; forward before boot
        // is not connected
        let id = id_for(tag, &tmp);
        let rec = if hm == RELAY_HANDLER_MODULE {
            json!({"transport": "memory", "heartbeat": 0, "reconnect": 0})
        } else {
            json!({"transport": "memory"})
        };
        create_leg(&kernel, hm, &id, rec).await;
        let out = kernel
            .send(vec![Item::new(
                AgentId::from(id.as_str()),
                json!({"type": "garbage"}),
            )
            .with_binary_key("bytes", vec![1])])
            .await
            .remove(0);
        assert_eq!(
            out.result,
            ask(&kernel, &id, json!({"type": "garbage"})).await,
            "{hm}"
        );
        assert_eq!(out.body, None);
        let out = kernel
            .send(vec![Item::new(
                AgentId::from(id.as_str()),
                json!({"type": "forward", "target": "far", "payload": {"type": "x"}, "forward_timeout": 30.0}),
            )
            .with_binary_key("bytes", vec![1])])
            .await
            .remove(0);
        assert_eq!(
            out.result["reason"],
            json!("not_connected"),
            "{hm}: {}",
            out.result
        );
        // delete runs the family's teardown and the agent is gone
        let gone = kernel
            .send(vec![Item::new(
                AgentId::from("kernel_state"),
                json!({"type": "delete_agent", "id": id}),
            )])
            .await
            .remove(0)
            .result;
        assert!(gone.get("error").is_none(), "{hm}: {gone}");
        assert!(
            !kernel.agents.contains_key(&AgentId::from(id.as_str())),
            "{hm}"
        );
    }
}

/// A leg across its lifetime: a live record change, a sender restart (python
/// `ws_bridge/tests/test_leg_lifetime.py`).
#[path = "tests_leg_lifetime.rs"]
mod leg_lifetime;

/// The relay password lives in a credential_store (python
/// `relay_connector/tests/test_relay_credentials.py`).
#[path = "tests_relay_credentials.rs"]
mod relay_credentials;

/// A dropped `ws` link is redialled by the leg itself (#493).
#[path = "tests_redial.rs"]
mod redial;

/// The two-way leg (#476): `hello` at every boot, an inbound `watch` gated and
/// streamed back (python `ws_bridge/tests/test_two_way_leg.py`).
#[path = "tests_two_way.rs"]
mod two_way;

/// A bridge leg makes the one write-time check every leg makes
/// (python `io_bridge._engine.validate_meta` → `unknown_ingress_refusal`): an
/// unknown name or a malformed spec is refused with the wire legal values; a rule
/// it implements is not. Both bundles, the same text.
#[test]
fn validate_meta_refuses_an_unknown_or_malformed_ingress_rule() {
    let legs: [&dyn fantastic_kernel::Bundle; 2] = [&WsBridgeBundle, &RelayConnectorBundle];
    for leg in legs {
        for (spec, shown) in [
            (json!("open_sesame"), "'open_sesame'"),
            (json!(7), "7"),
            (json!(["allow_all_ingress"]), "['allow_all_ingress']"),
            (json!({"type": ["x"]}), "{'type': ['x']}"),
            (
                json!({"type": "password_ingress", "env": 7}),
                "{'type': 'password_ingress', 'env': 7}",
            ),
        ] {
            let mut meta = Map::new();
            meta.insert("ingress_rule".into(), spec);
            assert_eq!(
                leg.validate_meta(&meta, &Map::new()),
                Some(format!(
                    "ingress_rule={shown} is not a rule this bundle implements — legal \
                     values: allow_all_ingress, deny_ingress, password_ingress"
                ))
            );
        }
        for spec in [
            json!("password_ingress"),
            json!({"type": "password_ingress", "env": "GROUP_TOKEN"}),
        ] {
            let mut meta = Map::new();
            meta.insert("ingress_rule".into(), spec);
            assert_eq!(leg.validate_meta(&meta, &Map::new()), None);
        }
    }
}

/// A record can still carry a malformed rule (written below the
/// verbs, or by hand on disk): the inbound call is ANSWERED with the seal's
/// denial, in python's words.
#[tokio::test]
async fn a_malformed_rule_on_the_record_denies_in_python_s_words() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp).await;
    let bid = id_for("brg_malformed", &tmp);
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({
                "type": "create_agent", "parent_id": "kernel",
                "handler_module": WS_HANDLER_MODULE, "can_call": {"$parent": "*", "$subtree": "*"},
                "id": bid,
                "peer_id": "stand_in",
                "transport": "memory",
            }),
        )])
        .await
        .remove(0)
        .result;
    patch(&kernel, &bid, "ingress_rule", json!(7));
    let peer = inject_one(&AgentId::from(bid.as_str()));
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(bid.as_str()),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;
    peer.send_frame(json!({
        "type": "call",
        "id": "c7",
        "target": "kernel_state",
        "payload": {"type": "reflect"},
    }))
    .await
    .unwrap();
    let reply = peer.recv_frame().await.expect("a reply frame").into_value();
    assert_eq!(reply["id"], "c7");
    assert_eq!(
        reply["data"]["error"],
        "edge sealed — unknown ingress rule '7'"
    );
    assert_eq!(reply["data"]["reason"], "unauthorized");
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(bid.as_str()),
            json!({"type": "shutdown"}),
        )])
        .await;
}

/// Every bridge verb is checked by its declaration before it runs: an unknown
/// argument, a missing one, a wrongly typed one is refused by name, on both families.
#[tokio::test]
async fn every_bridge_verb_is_checked_by_its_declaration() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_two_family_kernel(&tmp).await;
    let ws = id_for("decl_ws", &tmp);
    create_leg(&kernel, WS_HANDLER_MODULE, &ws, json!({"transport": "ws"})).await;
    let relay = id_for("decl_relay", &tmp);
    create_leg(
        &kernel,
        RELAY_HANDLER_MODULE,
        &relay,
        json!({"transport": "relay", "heartbeat": 0, "reconnect": 0}),
    )
    .await;
    let both = [
        (json!({"type": "boot", "now": true}), "boot: unknown argument 'now'"),
        (
            json!({"type": "reconnect", "now": true}),
            "reconnect: unknown argument 'now'",
        ),
        (
            json!({"type": "forward", "payload": {"type": "x"}, "forward_timeout": 30.0}),
            "forward: missing argument 'target'",
        ),
        (
            json!({"type": "forward", "target": "t", "payload": "x", "forward_timeout": 30.0}),
            "forward: argument 'payload' must be",
        ),
        (
            json!({"type": "forward", "target": "t", "payload": {"type": "x"}, "timeout": 1}),
            "forward: unknown argument 'timeout'",
        ),
        (
            json!({"type": "forward", "target": "t"}),
            "forward: missing argument 'payload'",
        ),
        (
            json!({"type": "watch_remote"}),
            "watch_remote: missing argument 'target'",
        ),
        (
            json!({"type": "unwatch_remote", "target": 7}),
            "unwatch_remote: argument 'target' must be",
        ),
        (
            json!({"type": "watch_remote", "target": ""}),
            "watch_remote: missing argument 'target'",
        ),
    ];
    for id in [&ws, &relay] {
        for (payload, want) in &both {
            let r = ask(&kernel, id, payload.clone()).await;
            assert!(
                r["error"].as_str().unwrap_or_default().starts_with(want),
                "{id} {payload} -> {r}"
            );
        }
    }
    for (payload, want) in [
        (
            json!({"type": "list_peers", "list_timeout": "5"}),
            "list_peers: argument 'list_timeout' must be",
        ),
        (
            json!({"type": "watch_directory", "x": 1}),
            "watch_directory: unknown argument 'x'",
        ),
        (
            json!({"type": "unwatch_directory", "x": 1}),
            "unwatch_directory: unknown argument 'x'",
        ),
        (
            json!({"type": "set_identity", "exposes": "all"}),
            "set_identity: argument 'exposes' must be",
        ),
        (
            json!({"type": "set_identity", "owner_guid": 5}),
            "set_identity: argument 'owner_guid' must be",
        ),
        (
            json!({"type": "set_identity", "role": "kernel", "nope": 1}),
            "set_identity: unknown argument 'nope'",
        ),
    ] {
        let r = ask(&kernel, &relay, payload.clone()).await;
        assert!(
            r["error"].as_str().unwrap_or_default().starts_with(want),
            "{payload} -> {r}"
        );
    }
}

/// `set_identity` tells an ABSENT field from an explicit null: a field sent is merged
/// over the record (null retracts it), a field left out stays as it was.
#[tokio::test]
async fn set_identity_merges_what_is_sent_and_null_retracts() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_two_family_kernel(&tmp).await;
    let relay = id_for("ident_relay", &tmp);
    create_leg(
        &kernel,
        RELAY_HANDLER_MODULE,
        &relay,
        json!({"transport": "memory", "heartbeat": 0, "reconnect": 0, "owner_guid": "M1", "exposes": ["a"]}),
    )
    .await;
    let _peer = inject_one(&AgentId::from(relay.as_str()));
    let _ = ask(&kernel, &relay, json!({"type": "boot"})).await;
    let _ = ask(&kernel, &relay, json!({"type": "set_identity", "role": "manager"})).await;
    let r = ask(&kernel, &relay, json!({"type": "reflect"})).await;
    assert_eq!(r["role"], json!("manager"), "{r}");
    assert_eq!(r["owner_guid"], json!("M1"), "a field left out is untouched: {r}");
    assert_eq!(r["exposes"], json!(["a"]), "{r}");
    let _ = ask(
        &kernel,
        &relay,
        json!({"type": "set_identity", "owner_guid": null, "role": null, "exposes": ["b", "c"]}),
    )
    .await;
    let r = ask(&kernel, &relay, json!({"type": "reflect"})).await;
    assert_eq!(r["role"], Value::Null, "null retracts the role: {r}");
    assert_eq!(r["owner_guid"], Value::Null, "{r}");
    assert_eq!(r["exposes"], json!(["b", "c"]), "{r}");
}

/// A record is born with the fields its bundle requires: a bridge without a
/// `transport`, a relay connector without `transport`, `heartbeat` or `reconnect`,
/// is refused at the write, by the record's declaration.
#[tokio::test]
async fn a_bridge_record_is_born_with_its_required_fields() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_two_family_kernel(&tmp).await;
    for (handler, meta, arg) in [
        (WS_HANDLER_MODULE, json!({}), "transport"),
        (RELAY_HANDLER_MODULE, json!({"heartbeat": 0, "reconnect": 0}), "transport"),
    ] {
        let id = id_for(&format!("req_{arg}"), &tmp);
        let mut payload = meta.as_object().cloned().unwrap();
        payload.insert("type".into(), json!("create_agent"));
        payload.insert("parent_id".into(), json!("kernel"));
        payload.insert("handler_module".into(), json!(handler));
        payload.insert("id".into(), json!(id));
        let r = kernel
            .send(vec![Item::new(
                AgentId::from("kernel_state"),
                Value::Object(payload),
            )])
            .await
            .remove(0)
            .result;
        assert_eq!(r["reason"], json!("missing_arg"), "{handler} {arg}: {r}");
        assert_eq!(r["arg"], json!(arg), "{handler} {arg}: {r}");
    }
}

/// The relay record's `heartbeat` and `reconnect` and a call's timeouts are API
/// defaults: reflect shows them, an absent field is born without error.
#[tokio::test]
async fn declared_timeout_defaults_show_in_reflect() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_two_family_kernel(&tmp).await;
    let id = id_for("dflt_relay", &tmp);
    create_leg(&kernel, RELAY_HANDLER_MODULE, &id, json!({"transport": "memory"})).await;
    let r = ask(&kernel, &id, json!({"type": "reflect"})).await;
    let text = r.to_string();
    for want in ["forward_timeout:number (default 30.0)", "list_timeout:number (default 30.0)"] {
        assert!(text.contains(want), "{want}: {text}");
    }
    let schema = crate::record::relay::RECORD.schema().clone();
    assert_eq!(schema["properties"]["heartbeat"]["default"], json!(30.0), "{schema}");
    assert_eq!(schema["properties"]["reconnect"]["default"], json!(10.0), "{schema}");
}

/// A forwarded `set` names its fields as secret in the CALLER's records (the far
/// bundle is unknown before sending); nothing else a bridge is sent does.
#[test]
fn bridge_payload_secret_marks_only_a_forwarded_set_fields() {
    let set = json!({"type": "forward", "target": "creds",
                     "payload": {"type": "set", "ref": "r", "fields": {"a": "1", "b": "2"}}});
    let dotted = json!({"type": "forward", "target": "creds",
                        "payload": {"type": "set", "ref": "r", "fields": {"a.b": "1"}}});
    let get = json!({"type": "forward", "target": "creds",
                     "payload": {"type": "get", "ref": "r", "fields": {"a": "1"}}});
    let reflect = json!({"type": "forward", "target": "creds", "payload": {"type": "reflect"}});
    let plain = json!({"type": "set", "fields": {"a": "1"}});
    let nofields = json!({"type": "forward", "target": "creds", "payload": {"type": "set"}});
    for b in [
        &WsBridgeBundle as &dyn Bundle,
        &RelayConnectorBundle as &dyn Bundle,
    ] {
        assert_eq!(
            b.payload_secret(&set),
            vec!["payload.fields.a".to_string(), "payload.fields.b".to_string()]
        );
        assert_eq!(b.payload_secret(&dotted), vec!["payload.fields".to_string()]);
        // a `set` the store's entry refuses is marked whole: its values were never checked.
        assert_eq!(b.payload_secret(&nofields), vec!["payload.fields".to_string()]);
        for none in [&get, &reflect, &plain] {
            assert!(b.payload_secret(none).is_empty(), "{none}");
        }
    }
}
