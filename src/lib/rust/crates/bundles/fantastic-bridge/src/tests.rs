//! Unit tests for the ws_bridge / relay_connector derivations.
//!
//! Most tests drive the memory transport — deterministic, no I/O,
//! and exercises every code path the bridge cares about. WS gets
//! one negative-path test (unreachable host → clean error) because
//! the real two-kernel round-trip is exercised by the cross-runtime
//! selftest, not in-process Rust unit tests.

use super::*;
use fantastic_kernel::Agent;
use fantastic_kernel::Item;
use serde_json::Map;
use tempfile::TempDir;

/// Build a kernel with the bridge + file bundles registered and a
/// root agent. Each test gets its own tempdir so the global
/// `BRIDGES` map can carry concurrent test agents without id clashes.
async fn mk_kernel(tmp: &TempDir) -> Arc<Kernel> {
    let mut kernel = Kernel::new();
    kernel.bundles.register(WS_HANDLER_MODULE, WsBridgeBundle);
    kernel
        .bundles
        .register("file_bridge.tools", fantastic_file::FileBundle);
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
                "handler_module": WS_HANDLER_MODULE,
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
/// An inbound bridge frame now dispatches AS THE LEG — the read loop runs under
/// `with_sender(<bridge id>)`, closing the gate bypass where a peer's frame
/// reached `Kernel::launch` with no sender and ran as the OPERATOR. So a bridge
/// serving calls onto a SIBLING needs the egress grant on itself and the ingress
/// grant on the target; the structural baseline covers only its own subtree and
/// its parent.
async fn grant_edge(kernel: &Arc<Kernel>, from: &str, to: &str) {
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": from,
                   "can_call": {"$self": "*", "$parent": "*", "$children": "*", to: "*"}}),
        )])
        .await
        .remove(0)
        .result;
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": to,
                   "callable_by": {"$self": "*", "$parent": "*", "$children": "*", from: "*"}}),
        )])
        .await
        .remove(0)
        .result;
}

/// A7.1 — TWO readmes, each python's byte for byte.
#[test]
fn readme_present_and_titled() {
    let py = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../..")
        .join("src/lib/python/bundled_agents/io");
    let Ok(ws) = std::fs::read_to_string(py.join("ws_bridge/src/ws_bridge/readme.md")) else {
        eprintln!("skipping: python tree not present at {}", py.display());
        return;
    };
    let relay =
        std::fs::read_to_string(py.join("relay_connector/src/relay_connector/readme.md")).unwrap();
    assert_eq!(ws, WS_README, "ws_bridge readme has drifted");
    assert_eq!(relay, RELAY_README, "relay_connector readme has drifted");
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
    assert!(r["verbs"]["forward"].is_string());
    assert!(r["emits"]["bridge_up"].is_string());
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
    let kernel = mk_kernel(&tmp).await;
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
                "handler_module": "file_bridge.tools",
                "id": file_id,
                "root": tmp.path().to_string_lossy(),
            }),
        )])
        .await
        .remove(0)
        .result;
    create_bridge(&kernel, &a_id, &b_id, "memory").await;
    create_bridge(&kernel, &b_id, &a_id, "memory").await;
    // B serves the inbound call onto the file agent — a SIBLING, so the edge is
    // explicit on both ends.
    grant_edge(&kernel, &b_id, &file_id).await;
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

    // A.forward(target=<file_id>, payload={type:"reflect"}) ships a
    // raw call frame → B's read loop dispatches kernel.send(vec![Item::new(<file_id>, // {reflect})]) → file_agent's reflect reply gets shipped back as the
    // forward's return value (asymmetric; no forward envelope).
    let reply = kernel
        .send(vec![Item::new(
            AgentId::from(a_id.as_str()),
            json!({
                "type": "forward",
                "target": file_id,
                "payload": {"type": "reflect"},
            }),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(reply["id"], file_id, "forward reply: {reply}");
    assert!(reply["root"].is_string());

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
                "timeout": 30.0,
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
    // may send and nothing tears down behind it (2026-09-12).
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

#[cfg(feature = "full")]
#[tokio::test]
async fn ssh_transport_unreachable_host_fails_cleanly() {
    // Skip gracefully if `ssh` isn't on PATH (CI containers without
    // openssh-client should not flake on this).
    if which::which("ssh").is_err() {
        eprintln!("skipping ssh_transport_unreachable_host_fails_cleanly — no `ssh` on PATH");
        return;
    }

    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp).await;
    let bid = id_for("brg_sshws", &tmp);
    // TEST-NET-1 (RFC 5737) — guaranteed unreachable. ExitOnForwardFailure
    // + BatchMode mean ssh exits non-zero quickly on auth/route failure,
    // so this should resolve well before the 5s tunnel-ready deadline.
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({
                "type": "create_agent", "parent_id": "kernel",
                "handler_module": WS_HANDLER_MODULE,
                "id": bid,
                "peer_id": "peer_x",
                "transport": "ssh+ws",
                "host": "192.0.2.1",
                "remote_port": 9,
                "local_port": 0,
            }),
        )])
        .await
        .remove(0)
        .result;
    let r = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        kernel.send(vec![Item::new(
            AgentId::from(bid.as_str()),
            json!({"type": "boot"}),
        )]),
    )
    .await
    .expect("ssh+ws boot should not hang")
    .remove(0)
    .result;
    let err = r["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("ssh+ws") || !err.is_empty(),
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
                "handler_module": WS_HANDLER_MODULE,
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
        resolve(None).unwrap().authorize(&call_action(None)),
        Decision::Deny(_)
    ));
    assert!(matches!(
        resolve(Some(&Value::Null))
            .unwrap()
            .authorize(&call_action(None)),
        Decision::Deny(_)
    ));
    // explicit allow_all_ingress opens it
    assert!(matches!(
        resolve(Some(&json!("allow_all_ingress")))
            .unwrap()
            .authorize(&call_action(None)),
        Decision::Allow
    ));
    // string + object form; `type` is the ONLY object spelling (the `policy` alias was
    // rust-only and is deleted — python's parse_spec never accepted it)
    let s = resolve(Some(&json!("deny_ingress"))).unwrap();
    assert!(matches!(s.authorize(&call_action(None)), Decision::Deny(_)));
    let o = resolve(Some(&json!({"type": "deny_ingress"}))).unwrap();
    assert!(matches!(o.authorize(&call_action(None)), Decision::Deny(_)));
    // An object with no `type` is NOT a rule — and python resolves "no rule" to
    // the plain SEAL, not to an error (A1.2). It used to be an `Err` each caller
    // rendered its own way: `{error:"rule object missing 'type'"}` with no
    // `reason` from the fs gate, a throw out of `handle` in swift.
    let typeless = resolve(Some(&json!({"policy": "deny_ingress"}))).unwrap();
    match typeless.authorize(&call_action(None)) {
        Decision::Deny(d) => assert_eq!(d.reason, "edge sealed by policy"),
        Decision::Allow => panic!("a type-less rule object must seal"),
    }
    // The SEAL refuses EVERY inbound kind — dispatch AND telemetry (py-canonical;
    // this used to pin `watch` as allowed, which made the rust seal partial and
    // let a "sealed" leg leak state to watchers).
    let watch = s.authorize(&Action {
        kind: "watch",
        target: "t",
        verb: "watch",
        token: None,
    });
    assert!(matches!(watch, Decision::Deny(_)));
    // UNKNOWN ⇒ SEALS and names the bad rule — it does NOT Err.
    //
    // It used to be `Err`, on the theory of failing loudly. But resolution runs PER
    // CALL, not at boot: in python that error surfaced as an unhandled exception out
    // of the handler, which killed the caller's whole chain with no reply at all. An
    // unresolvable rule is not an authorization, so deny — and say which name was
    // wrong. Mirrors py `test_resolve_ingress_unknown_type_seals_and_names_the_bad_rule`.
    let unknown = resolve(Some(&json!("password:kernel-internal-transcripts")))
        .expect("unknown rule must resolve to a seal, not an error");
    match unknown.authorize(&Action {
        kind: "call",
        target: "t",
        verb: "reflect",
        token: None,
    }) {
        Decision::Deny(denial) => {
            // The verdict splits: `reason` states WHAT happened (the name was not
            // resolvable), `hint` states the repair + the legal set. A caller that
            // read only one half used to get half a refusal.
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
    let p = resolve(Some(&json!({"type": "password_ingress", "env": env}))).unwrap();
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
    // spelled out in `manifold_spec`, so the registry itself carries no cross-manifold
    // alias any more (a bare inbound name written straight into `egress_rule` is a typo,
    // and must be loud).
    for stem in ["deny", "allow_all"] {
        let scoped = authorizer::manifold_spec(None, Some(&json!(stem)), "_egress").unwrap();
        assert_eq!(scoped, json!("silent_egress"));
        assert!(resolve(Some(&scoped)).unwrap().credential().is_none());
    }
    assert!(resolve(Some(&json!("deny_ingress"))).is_err());
    // password ⇒ presents the env token. `env` is the only spelling for the secret key
    // (the `token_env` wire alias was rust-only and is deleted).
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
    // registries. Measured 2026-08-21, before scoping (python AND this runtime, which
    // carried the identical aliases): `auth:"silent"` — a legitimate EGRESS policy —
    // resolved ingress to `DenyInbound(unknown="silent")`, SEALING the leg while
    // recording the unknown name and saying nothing; and `auth:"allow_all"` handed
    // egress a `Silent` nobody asked for. One unscoped name against two registries with
    // different membership is a fallback, and this kernel has none.
    //
    // It still DENIES — an unresolvable rule is not an authorization — but it now names
    // the manifold mistake instead of looking like an ordinary seal.
    let sealed = authorizer::ingress::resolve(Some(&json!("silent_egress"))).unwrap();
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
        assert!(authorizer::ingress::resolve(
            authorizer::manifold_spec(None, Some(&json!(stem)), "_ingress").as_ref()
        )
        .is_ok());
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
    )
    .unwrap();
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
                "handler_module": WS_HANDLER_MODULE,
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
                "handler_module": "file_bridge.tools",
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

    // write_stream via a binary inbound call.
    peer.send_binary(
        json!({"type":"call","id":"w1","target":"fb",
               "payload":{"type":"write_stream","path":"blob.bin","truncate":true}}),
        payload.clone(),
    )
    .await
    .unwrap();
    let w = peer.recv_frame().await.expect("write reply").into_value();
    assert_eq!(w["type"], "reply");
    assert_eq!(w["data"]["written"], payload.len(), "write reply: {w}");

    // read_stream via a binary inbound call → reply is a BINARY frame.
    peer.send_binary(
        json!({"type":"call","id":"r1","target":"fb",
               "payload":{"type":"read_stream","path":"blob.bin"}}),
        Vec::new(),
    )
    .await
    .unwrap();
    match peer.recv_frame().await.expect("read reply") {
        crate::transport::Frame::Binary(header, body) => {
            assert_eq!(header["id"], "r1");
            assert_eq!(header["data"]["eof"], true);
            assert_eq!(body, payload, "bytes must round-trip raw over the bridge");
        }
        crate::transport::Frame::Text(v) => panic!("expected a binary reply, got text: {v}"),
    }

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
                "handler_module": WS_HANDLER_MODULE,
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
                "handler_module": WS_HANDLER_MODULE,
                "id": bid,
                "peer_id": "stand_in",
                "transport": "memory",
                "auth": {"type": "password", "env": env},
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
            json!({"type": "forward", "target": "remote", "payload": {"type": "reflect"}}),
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
                "handler_module": WS_HANDLER_MODULE,
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
    // no `auth` in reflect — a legacy WRITE shorthand only (py parity)
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
            json!({"type": "forward", "target": "remote", "payload": {"type": "reflect"}}),
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

#[async_trait]
impl Bundle for BouncerBundle {
    fn name(&self) -> &str {
        "bouncer.tools"
    }
    async fn handle(
        &self,
        _agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        if payload.get("type").and_then(Value::as_str) != Some("bounce") {
            return Ok(Some(json!({"error": "unknown verb"})));
        }
        let bridge = payload
            .get("bridge")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let out = kernel
            .send(vec![Item::new(
                AgentId::from(bridge.as_str()),
                json!({
                    "type": "forward",
                    "target": payload.get("back_to").cloned().unwrap_or(Value::Null),
                    "payload": {"type": "reflect"},
                    // Short, so the OLD inline code fails this in seconds rather
                    // than parking it for the 30s default.
                    "timeout": 3.0,
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
    let tmp = TempDir::new().unwrap();
    let mut kernel = Kernel::new();
    kernel.bundles.register(WS_HANDLER_MODULE, WsBridgeBundle);
    kernel.bundles.register("bouncer.tools", BouncerBundle);
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

    let a_id = id_for("brg_a_re", &tmp);
    let b_id = id_for("brg_b_re", &tmp);
    let probe = id_for("bouncer", &tmp);
    create_bridge(&kernel, &a_id, &b_id, "memory").await;
    create_bridge(&kernel, &b_id, &a_id, "memory").await;
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "create_agent", "parent_id": "kernel",
                   "handler_module": "bouncer.tools", "id": probe}),
        )])
        .await
        .remove(0)
        .result;
    // B may reach the probe; the probe may reach B to forward back.
    grant_edge(&kernel, &b_id, &probe).await;
    grant_edge(&kernel, &probe, &b_id).await;
    // A serves the probe's return hop onto the root.
    grant_edge(&kernel, &a_id, "kernel_state").await;
    inject_pair(&AgentId::from(a_id.as_str()), &AgentId::from(b_id.as_str()));
    for id in [&a_id, &b_id] {
        let _ = kernel
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
        kernel.send(vec![Item::new(
            AgentId::from(a_id.as_str()),
            json!({"type": "forward", "target": probe,
                   "payload": {"type": "bounce", "bridge": b_id, "back_to": "kernel_state"},
                   "timeout": 10.0}),
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
    // A SIBLING of the leg: no structural baseline token reaches it, so the only
    // way the call lands is an explicit grant — or a bypassed gate.
    let target = id_for("ff_ident", &tmp);
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "create_agent", "parent_id": "kernel",
                   "handler_module": "file_bridge.tools", "id": target,
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

    peer.send_frame(json!({"type":"call","id":"i1","target":target,
                          "payload":{"type":"reflect"}}))
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

    // And the gate is not simply always-deny: the granted edge dispatches.
    grant_edge(&kernel, &bid, &target).await;
    peer.send_frame(json!({"type":"call","id":"i2","target":target,
                          "payload":{"type":"reflect"}}))
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
    let mut kernel = Kernel::new();
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

/// A7.2 / A7.5 — an unknown verb (and `shutdown`, and a ws leg's directory
/// verbs) take python's `bridge: unknown type` reply, envelope hint and all.
#[tokio::test]
async fn unknown_type_is_pythons_reply_with_the_envelope_hint() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_two_family_kernel(&tmp).await;
    let ws = id_for("a72_ws", &tmp);
    create_leg(&kernel, WS_HANDLER_MODULE, &ws, json!({})).await;
    let r = ask(&kernel, &ws, json!({"type": "garbage"})).await;
    assert_eq!(r["error"], json!("bridge: unknown type 'garbage'"));
    assert_eq!(
        r["hint"],
        json!("every bridge frame needs a \"type\"; to reach a remote agent use {\"type\":\"forward\",\"target\":\"<remote_id>\",\"payload\":{\"type\":\"<verb>\",...}} — the inner payload is itself a verb frame and needs its own \"type\".")
    );
    // `shutdown` is not in the table either — the teardown lives in on_delete.
    assert_eq!(
        ask(&kernel, &ws, json!({"type": "shutdown"})).await["error"],
        json!("bridge: unknown type 'shutdown'")
    );
    // A7.5 — the directory verbs belong to relay_connector only.
    for verb in [
        "list_peers",
        "watch_directory",
        "unwatch_directory",
        "set_identity",
    ] {
        assert_eq!(
            ask(&kernel, &ws, json!({ "type": verb })).await["error"],
            json!(format!("bridge: unknown type '{verb}'")),
            "{verb} must not exist on a ws leg"
        );
    }
}

/// A7.6 / A7.8 / A7.9 / A7.11 / A7.12 — reflect's key order, sentence, default
/// transport, family fields, verbs and emits are python's.
#[tokio::test]
async fn reflect_key_order_per_family() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_two_family_kernel(&tmp).await;
    let ws = id_for("a76_ws", &tmp);
    let relay = id_for("a76_relay", &tmp);
    create_leg(&kernel, WS_HANDLER_MODULE, &ws, json!({})).await;
    create_leg(&kernel, RELAY_HANDLER_MODULE, &relay, json!({})).await;

    let r = ask(&kernel, &ws, json!({"type": "reflect"})).await;
    // (the kernel appends its own gate keys after the bundle's body)
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
            "host",
            "peer_id",
            "local_port",
            "remote_port",
            "tunnel_pid",
            "verbs",
            "emits",
        ]
    );
    assert_eq!(
        r["sentence"],
        json!(
            "Cross-kernel comms bridge — WS-only, asymmetric (no peer bridge needed); weak proxy."
        )
    );
    assert_eq!(r["transport"], json!("ws"), "the ws default kind");
    assert_eq!(r["connected"], json!(false));
    assert_eq!(r["sealed"], json!(true));
    assert_eq!(r["verbs"].as_object().unwrap().len(), 6);
    assert_eq!(
        r["verbs"]["reflect"],
        json!("Identity + transport + connectivity. No args.")
    );
    assert_eq!(
        r["emits"]["bridge_down"],
        json!("{type:'bridge_down'} — emitted when the transport drops (peer closed, tunnel died)")
    );

    let r = ask(&kernel, &relay, json!({"type": "reflect"})).await;
    let keys: Vec<&str> = r
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .take(17)
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
            "verbs",
            "emits",
        ]
    );
    assert_eq!(
        r["sentence"],
        json!("Cross-kernel comms through a relay-kernel router — dial-out WS, group-password auth (X-Fantastic-Auth), GUID-routed; symmetric RPC tunneled over the relay; auto-reconnecting.")
    );
    assert_eq!(r["transport"], json!("relay"), "the relay default kind");
    assert_eq!(r["reconnect"], json!(10.0));
    assert_eq!(r["role"], json!("kernel"));
    assert_eq!(r["exposes"], json!([]));
    assert_eq!(r["verbs"].as_object().unwrap().len(), 10);
    assert_eq!(
        r["verbs"]["unwatch_directory"],
        json!("No args. Stop the directory subscription started by `watch_directory`.")
    );
}

/// A7.13 / A7.14 — boot's error texts are python's, and a relay leg that is
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
    create_leg(&kernel, WS_HANDLER_MODULE, &ws, json!({})).await;
    assert_eq!(
        ask(&kernel, &ws, json!({"type": "boot"})).await,
        json!({"error": "bridge: ws boot failed: peer_id required for ws/ssh+ws transports"})
    );
    let ws2 = id_for("a713_ws2", &tmp);
    create_leg(&kernel, WS_HANDLER_MODULE, &ws2, json!({"peer_id": "p"})).await;
    assert_eq!(
        ask(&kernel, &ws2, json!({"type": "boot"})).await,
        json!({"error": "bridge: ws boot failed: ws transport requires local_port or remote_port"})
    );
    // an unknown kind on the ws derivation (no "not in this derivation" gate).
    let odd = id_for("a713_odd", &tmp);
    create_leg(
        &kernel,
        WS_HANDLER_MODULE,
        &odd,
        json!({"transport": "carrier-pigeon", "peer_id": "p"}),
    )
    .await;
    assert_eq!(
        ask(&kernel, &odd, json!({"type": "boot"})).await,
        json!({
            "error": "bridge: carrier-pigeon boot failed: unknown transport 'carrier-pigeon'"
        })
    );

    // a bad auth rule tears the leg down and says so.
    let bad = id_for("a713_bad", &tmp);
    create_leg(
        &kernel,
        WS_HANDLER_MODULE,
        &bad,
        json!({"transport": "memory", "ingress_rule": 42}),
    )
    .await;
    inject_pair(
        &AgentId::from(bad.as_str()),
        &AgentId::from("a713_bad_peer"),
    );
    let r = ask(&kernel, &bad, json!({"type": "boot"})).await;
    assert!(
        r["error"]
            .as_str()
            .unwrap_or("")
            .starts_with("bridge: bad auth rule: "),
        "{r}"
    );
}

/// A7.14 — a relay boot that fails validation mints NO guid.
#[tokio::test]
async fn relay_boot_missing_partner_mints_no_guid() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_two_family_kernel(&tmp).await;
    let relay = id_for("a714_relay", &tmp);
    create_leg(
        &kernel,
        RELAY_HANDLER_MODULE,
        &relay,
        json!({"transport": "relay", "relay_url": "ws://127.0.0.1:1"}),
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
    // A bad `role` is python's ValueError text, ROLES repr and all.
    let bad_role = id_for("a714_role", &tmp);
    create_leg(
        &kernel,
        RELAY_HANDLER_MODULE,
        &bad_role,
        json!({"transport": "relay", "relay_url": "ws://127.0.0.1:1",
               "partner_guid": "B", "role": "boss"}),
    )
    .await;
    assert_eq!(
        ask(&kernel, &bad_role, json!({"type": "boot"})).await,
        json!({
            "error": "bridge: relay boot failed: relay_connector role must be one of ('manager', 'kernel'), got 'boss'"
        })
    );
}

/// A7.10 — `connected` follows the transport's closed state, and a closed
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

/// A7.15 — forward's refusals and error texts are python's.
#[tokio::test]
async fn forward_error_texts_match_python() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp).await;
    let bid = id_for("a715", &tmp);
    create_bridge(&kernel, &bid, "peer", "memory").await;
    // validation — python says `(dict)`.
    assert_eq!(
        ask(&kernel, &bid, json!({"type": "forward", "target": "x"})).await,
        json!({"error": "bridge.forward: target (str) + payload (dict) required"})
    );
    // pre-boot
    let r = ask(
        &kernel,
        &bid,
        json!({"type": "forward", "target": "x", "payload": {"type": "reflect"}}),
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
        json!({"type": "forward", "target": "x", "payload": {"type": "reflect"}, "timeout": 0.05}),
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
        json!({"type": "forward", "target": "x", "payload": {"type": "reflect"}, "timeout": 5.0}),
    )
    .await;
    answer.await.unwrap();
    assert_eq!(r, json!({"error": "remote error: boom"}));
}

/// A7.16 — the directory replies are python's shapes.
#[tokio::test]
async fn directory_replies_match_python() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_two_family_kernel(&tmp).await;
    let relay = id_for("a716", &tmp);
    create_leg(
        &kernel,
        RELAY_HANDLER_MODULE,
        &relay,
        json!({"transport": "memory"}),
    )
    .await;
    // pre-boot: unwatch_directory is simply ok; the others refuse.
    assert_eq!(
        ask(&kernel, &relay, json!({"type": "unwatch_directory"})).await,
        json!({"ok": true})
    );
    let r = ask(&kernel, &relay, json!({"type": "list_peers"})).await;
    assert_eq!(
        r["error"],
        json!("relay_connector.list_peers: not connected")
    );
    assert_eq!(r["reason"], json!("not_connected"));
    assert_eq!(r["hint"], json!("call boot first"));
    // a bad role is a bad_arg refusal naming the arg — python checks the
    // CONNECTION first, so the leg has to be up to reach it.
    let _peer = inject_one(&AgentId::from(relay.as_str()));
    let _ = ask(&kernel, &relay, json!({"type": "boot"})).await;
    let r = ask(
        &kernel,
        &relay,
        json!({"type": "set_identity", "role": "boss"}),
    )
    .await;
    assert_eq!(
        r["error"],
        json!("set_identity: role must be one of ('manager', 'kernel')")
    );
    assert_eq!(r["reason"], json!("bad_arg"));
    assert_eq!(r["arg"], json!("role"));
}

/// A7.17 — a reconnect announces exactly one `bridge_down`, then `bridge_up`.
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
    // C76: a reader on the bridge's OWN inbox now sees the REQUESTS too, each
    // mirrored BEFORE its handler runs — python's measured `>reconnect reconnect
    // bridge_down bridge_up`. Rust used to fan out after the reply, so the echo
    // landed behind the events the handler had already emitted.
    assert_eq!(
        kinds,
        vec!["boot", "bridge_up", "reconnect", "bridge_down", "bridge_up"],
        "each request precedes its own events; exactly one bridge_down per teardown"
    );
}

/// A7.18 — a booted leg's work is registered as python names it: `read_loop`
/// (both derivations), and since C36 each registration is a `task` ROW under the
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
    // C36 — the fork is a MESSAGE: one `task` row on the leg, and the pid in
    // `ps` is that row's id.
    let store: Vec<_> = fantastic_kernel::test_support::rows_about(&kernel, &AgentId::from(bid.as_str()))
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

/// A7.3 — a remote event with no payload re-emits `{}` (python `payload or {}`).
#[tokio::test]
async fn a_remote_event_without_a_payload_re_emits_an_empty_object() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp).await;
    let bid = id_for("a73", &tmp);
    create_bridge(&kernel, &bid, "peer", "memory").await;
    let peer = inject_one(&AgentId::from(bid.as_str()));
    let mut rx = kernel.ensure_inbox(&AgentId::from(bid.as_str()));
    let _ = ask(&kernel, &bid, json!({"type": "boot"})).await;
    peer.send_frame(json!({"type": "event"})).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let mut seen: Vec<Value> = Vec::new();
    while let Some(v) = rx.try_recv() {
        seen.push(v.payload);
    }
    assert!(
        seen.contains(&json!({})),
        "a payload-less event re-emits {{}}: {seen:?}"
    );
}

/// A15.1 — a frame that does not decode (or decodes to a non-object) is SKIPPED
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
    server.abort();
}
