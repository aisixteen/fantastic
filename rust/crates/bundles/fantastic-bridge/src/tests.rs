//! Unit tests for the ws_bridge / cloud_bridge derivations.
//!
//! Most tests drive the memory transport — deterministic, no I/O,
//! and exercises every code path the bridge cares about. WS gets
//! one negative-path test (unreachable host → clean error) because
//! the real two-kernel round-trip is exercised by the cross-runtime
//! selftest, not in-process Rust unit tests.

use super::*;
use fantastic_kernel::Agent;
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
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    let _rx = kernel.register(Arc::clone(&root));
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
    kernel
        .send(
            &AgentId::from("core"),
            json!({
                "type": "create_agent",
                "handler_module": WS_HANDLER_MODULE,
                "id": id,
                "peer_id": peer_id,
                "transport": transport,
                "ingress_rule": "allow_all",
            }),
        )
        .await;
}

#[test]
fn readme_present_and_titled() {
    assert!(!README.is_empty());
    assert!(README.contains("ws_bridge"));
}

#[tokio::test]
async fn reflect_shape_for_memory_transport() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp).await;
    let bid = id_for("brg_reflect", &tmp);
    create_bridge(&kernel, &bid, "peer_x", "memory").await;
    let r = kernel
        .send(&AgentId::from(bid.as_str()), json!({"type": "reflect"}))
        .await;
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
        .send(&AgentId::from(a_id.as_str()), json!({"type": "boot"}))
        .await;
    let rb = kernel
        .send(&AgentId::from(b_id.as_str()), json!({"type": "boot"}))
        .await;
    assert_eq!(ra["booted"], true);
    assert_eq!(ra["transport"], "memory");
    assert_eq!(rb["booted"], true);

    let ref_a = kernel
        .send(&AgentId::from(a_id.as_str()), json!({"type": "reflect"}))
        .await;
    assert_eq!(ref_a["connected"], true);

    // Cleanup so global BRIDGES doesn't carry into other tests.
    let _ = kernel
        .send(&AgentId::from(a_id.as_str()), json!({"type": "shutdown"}))
        .await;
    let _ = kernel
        .send(&AgentId::from(b_id.as_str()), json!({"type": "shutdown"}))
        .await;
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
    kernel
        .send(
            &AgentId::from("core"),
            json!({
                "type": "create_agent",
                "handler_module": "file_bridge.tools",
                "id": file_id,
                "root": tmp.path().to_string_lossy(),
            }),
        )
        .await;
    create_bridge(&kernel, &a_id, &b_id, "memory").await;
    create_bridge(&kernel, &b_id, &a_id, "memory").await;
    inject_pair(&AgentId::from(a_id.as_str()), &AgentId::from(b_id.as_str()));
    let _ = kernel
        .send(&AgentId::from(a_id.as_str()), json!({"type": "boot"}))
        .await;
    let _ = kernel
        .send(&AgentId::from(b_id.as_str()), json!({"type": "boot"}))
        .await;

    // A.forward(target=<file_id>, payload={type:"reflect"}) ships a
    // raw call frame → B's read loop dispatches kernel.send(<file_id>,
    // {reflect}) → file_agent's reflect reply gets shipped back as the
    // forward's return value (asymmetric; no forward envelope).
    let reply = kernel
        .send(
            &AgentId::from(a_id.as_str()),
            json!({
                "type": "forward",
                "target": file_id,
                "payload": {"type": "reflect"},
            }),
        )
        .await;
    assert_eq!(reply["id"], file_id, "forward reply: {reply}");
    assert!(reply["root"].is_string());

    let _ = kernel
        .send(&AgentId::from(a_id.as_str()), json!({"type": "shutdown"}))
        .await;
    let _ = kernel
        .send(&AgentId::from(b_id.as_str()), json!({"type": "shutdown"}))
        .await;
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
        .send(&AgentId::from(a_id.as_str()), json!({"type": "boot"}))
        .await;

    let k = Arc::clone(&kernel);
    let a_id_clone = a_id.clone();
    let forward = tokio::spawn(async move {
        k.send(
            &AgentId::from(a_id_clone.as_str()),
            json!({
                "type": "forward",
                "target": "irrelevant_target",
                "payload": {"type": "reflect"},
                "timeout": 30.0,
            }),
        )
        .await
    });
    // Tiny delay so the forward registers its pending oneshot
    // before we slam the door.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    // Shut A down — its read loop tears down + every pending
    // forward fails with a ConnectionError-flavored reply via the
    // shutdown drain path.
    let _ = kernel
        .send(&AgentId::from(a_id.as_str()), json!({"type": "shutdown"}))
        .await;
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
        .send(
            &AgentId::from(bid.as_str()),
            json!({"type": "watch_remote", "target": "core"}),
        )
        .await;
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
        .send(&AgentId::from(bid.as_str()), json!({"type": "boot"}))
        .await;

    let r = kernel
        .send(
            &AgentId::from(bid.as_str()),
            json!({"type": "watch_remote", "target": "remote_core"}),
        )
        .await;
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
        .send(&AgentId::from(bid.as_str()), json!({"type": "shutdown"}))
        .await;
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
        .send(&AgentId::from(a_id.as_str()), json!({"type": "boot"}))
        .await;
    let _ = kernel
        .send(&AgentId::from(b_id.as_str()), json!({"type": "boot"}))
        .await;

    // Re-inject a fresh pair so reconnect's boot has something to
    // attach to (the original pair has been consumed at first boot).
    inject_pair(&AgentId::from(a_id.as_str()), &AgentId::from(b_id.as_str()));
    let r = kernel
        .send(&AgentId::from(a_id.as_str()), json!({"type": "reconnect"}))
        .await;
    assert_eq!(r["booted"], true, "reconnect: {r}");
    let reflect = kernel
        .send(&AgentId::from(a_id.as_str()), json!({"type": "reflect"}))
        .await;
    assert_eq!(reflect["connected"], true);

    let _ = kernel
        .send(&AgentId::from(a_id.as_str()), json!({"type": "shutdown"}))
        .await;
    let _ = kernel
        .send(&AgentId::from(b_id.as_str()), json!({"type": "shutdown"}))
        .await;
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
    kernel
        .send(
            &AgentId::from("core"),
            json!({
                "type": "create_agent",
                "handler_module": WS_HANDLER_MODULE,
                "id": bid,
                "peer_id": "peer_x",
                "transport": "ssh+ws",
                "host": "192.0.2.1",
                "remote_port": 9,
                "local_port": 0,
            }),
        )
        .await;
    let r = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        kernel.send(&AgentId::from(bid.as_str()), json!({"type": "boot"})),
    )
    .await
    .expect("ssh+ws boot should not hang");
    let err = r["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("ssh+ws") || !err.is_empty(),
        "expected clean error, got: {r}",
    );
    let ref_r = kernel
        .send(&AgentId::from(bid.as_str()), json!({"type": "reflect"}))
        .await;
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
    kernel
        .send(
            &AgentId::from("core"),
            json!({
                "type": "create_agent",
                "handler_module": WS_HANDLER_MODULE,
                "id": bid,
                "peer_id": "peer_x",
                "transport": "ws",
                "host": "192.0.2.1",
                "port": 9,
            }),
        )
        .await;
    let r = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        kernel.send(&AgentId::from(bid.as_str()), json!({"type": "boot"})),
    )
    .await
    .expect("ws boot should not hang");
    let err = r["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("ws connect failed") || err.contains("ws") || !err.is_empty(),
        "expected clean error, got: {r}",
    );
    let ref_r = kernel
        .send(&AgentId::from(bid.as_str()), json!({"type": "reflect"}))
        .await;
    assert_eq!(ref_r["connected"], false, "reflect: {ref_r}");
}

// ── cloud_bridge: in-process TLS 1.3 mTLS loopback (no relay, no net) ──

#[tokio::test]
async fn cloud_bridge_tls_loopback_round_trip() {
    use crate::transport::cloud::{
        der_to_pem, self_signed_cert, CloudTransport, MemoryByteChannel,
    };
    use crate::transport::BridgeTransport;

    let (cert_a, key_a) = self_signed_cert(&[1u8; 32]).unwrap();
    let (cert_b, key_b) = self_signed_cert(&[2u8; 32]).unwrap();
    let approved_a = [der_to_pem(&cert_b)]; // A pins B
    let approved_b = [der_to_pem(&cert_a)]; // B pins A
    let (ch_a, ch_b) = MemoryByteChannel::pair();

    // A = TLS client, B = TLS server; each PINS the other's cert. Both connects
    // drive the handshake, so they must run concurrently to exchange messages.
    let (ra, rb) = tokio::join!(
        CloudTransport::connect(ch_a, false, cert_a, key_a, &approved_a),
        CloudTransport::connect(ch_b, true, cert_b, key_b, &approved_b),
    );
    let ta = ra.expect("client handshake/pin");
    let tb = rb.expect("server handshake/pin");
    // Each learned the other's real Ed25519 identity (32-byte pubkey), distinct.
    assert_eq!(ta.peer_pubkey.len(), 32);
    assert_eq!(tb.peer_pubkey.len(), 32);
    assert_ne!(ta.peer_pubkey, tb.peer_pubkey);

    // call A→B round-trips as TLS app-data.
    ta.send_frame(json!({"type": "call", "id": "1", "target": "x"}))
        .await
        .unwrap();
    assert_eq!(
        tb.recv_frame().await.unwrap().into_value(),
        json!({"type": "call", "id": "1", "target": "x"})
    );
    // keepalive is dropped; the real reply (and a >64KB frame) surface intact.
    let blob = "x".repeat(200_000);
    tb.send_frame(json!({"type": "keepalive"})).await.unwrap();
    tb.send_frame(json!({"type": "reply", "id": "1", "data": {"blob": blob}}))
        .await
        .unwrap();
    let got = ta.recv_frame().await.unwrap().into_value();
    assert_eq!(got["data"]["blob"], json!(blob));

    ta.close().await;
    tb.close().await;
}

#[tokio::test]
async fn cloud_bridge_pins_peer_cert_rejects_unapproved() {
    use crate::transport::cloud::{
        der_to_pem, self_signed_cert, CloudTransport, MemoryByteChannel,
    };

    let (cert_a, key_a) = self_signed_cert(&[1u8; 32]).unwrap();
    let (cert_b, key_b) = self_signed_cert(&[2u8; 32]).unwrap();
    let (cert_c, _key_c) = self_signed_cert(&[9u8; 32]).unwrap();
    let approved_a = [der_to_pem(&cert_c)]; // A trusts C, NOT B → B is unapproved
    let approved_b = [der_to_pem(&cert_a)];
    let (ch_a, ch_b) = MemoryByteChannel::pair();

    let (ra, _rb) = tokio::join!(
        CloudTransport::connect(ch_a, false, cert_a, key_a, &approved_a),
        CloudTransport::connect(ch_b, true, cert_b, key_b, &approved_b),
    );
    assert!(ra.is_err(), "client must reject the unapproved server cert");
}

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
    // explicit allow_all opens it
    assert!(matches!(
        resolve(Some(&json!("allow_all")))
            .unwrap()
            .authorize(&call_action(None)),
        Decision::Allow
    ));
    // string + object form (both `type` and legacy `policy`)
    let s = resolve(Some(&json!("deny_inbound"))).unwrap();
    assert!(matches!(s.authorize(&call_action(None)), Decision::Deny(_)));
    let o = resolve(Some(&json!({"type": "deny_inbound"}))).unwrap();
    assert!(matches!(o.authorize(&call_action(None)), Decision::Deny(_)));
    let legacy = resolve(Some(&json!({"policy": "deny_inbound"}))).unwrap();
    assert!(matches!(
        legacy.authorize(&call_action(None)),
        Decision::Deny(_)
    ));
    // watch/unwatch not gated by deny_inbound
    let watch = s.authorize(&Action {
        kind: "watch",
        target: "t",
        verb: "watch",
        token: None,
    });
    assert!(matches!(watch, Decision::Allow));
    // unknown ⇒ Err (fails the boot loudly)
    assert!(resolve(Some(&json!("nope"))).is_err());
}

#[test]
fn ingress_password_checks_envelope_token() {
    use authorizer::ingress::resolve;
    // test-unique env var so parallel tests don't clobber each other
    let env = "FANTASTIC_GROUP_TOKEN_RS_ING";
    std::env::set_var(env, "s3cret");
    // `env` (new) spelling threads to the rule
    let p = resolve(Some(&json!({"type": "password", "env": env}))).unwrap();
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
    // absent + inbound-only names ⇒ Silent (present nothing)
    assert!(resolve(None).unwrap().credential().is_none());
    assert!(resolve(Some(&json!("deny_inbound")))
        .unwrap()
        .credential()
        .is_none());
    // password ⇒ presents the env token (legacy `token_env` spelling accepted)
    let env = "FANTASTIC_GROUP_TOKEN_RS_EG";
    std::env::set_var(env, "abc");
    let p = resolve(Some(&json!({"type": "password", "token_env": env}))).unwrap();
    assert_eq!(p.credential().as_deref(), Some("abc"));
    std::env::remove_var(env);
    assert!(p.credential().is_none()); // unset ⇒ presents nothing
                                       // unknown ⇒ Err
    assert!(resolve(Some(&json!("nope"))).is_err());
}

#[test]
fn auth_shorthand_is_symmetric() {
    // `auth:"password"` ⇒ ingress checks AND egress presents (group member)
    let env = "FANTASTIC_GROUP_TOKEN_RS_SYM";
    std::env::set_var(env, "k");
    let spec = json!({"type": "password", "env": env});
    let ing = authorizer::ingress::resolve(Some(&spec)).unwrap();
    let eg = authorizer::egress::resolve(Some(&spec)).unwrap();
    assert!(matches!(
        ing.authorize(&call_action(Some("k"))),
        Decision::Allow
    ));
    assert_eq!(eg.credential().as_deref(), Some("k"));
    std::env::remove_var(env);
}

/// Create a bridge agent carrying an `auth` policy meta field.
async fn create_bridge_with_auth(kernel: &Arc<Kernel>, id: &str, peer_id: &str, auth: &str) {
    kernel
        .send(
            &AgentId::from("core"),
            json!({
                "type": "create_agent",
                "handler_module": WS_HANDLER_MODULE,
                "id": id,
                "peer_id": peer_id,
                "transport": "memory",
                "auth": auth,
            }),
        )
        .await;
}

#[tokio::test]
async fn deny_inbound_refuses_inbound_call() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp).await;
    let bid = id_for("brg_deny", &tmp);
    create_bridge_with_auth(&kernel, &bid, "stand_in", "deny_inbound").await;
    // Inject A's half; keep the peer half to push a synthetic inbound call.
    let peer = inject_one(&AgentId::from(bid.as_str()));
    let _ = kernel
        .send(&AgentId::from(bid.as_str()), json!({"type": "boot"}))
        .await;
    // reflect surfaces the policy back (read-key == write-key, py parity).
    let reflect = kernel
        .send(&AgentId::from(bid.as_str()), json!({"type": "reflect"}))
        .await;
    assert_eq!(reflect["ingress_rule"], "deny_inbound");
    assert_eq!(reflect["sealed"], true);

    peer.send_frame(json!({
        "type": "call",
        "id": "c1",
        "target": "core",
        "payload": {"type": "reflect"},
    }))
    .await
    .unwrap();
    let reply = peer.recv_frame().await.expect("a reply frame").into_value();
    assert_eq!(reply["type"], "reply");
    assert_eq!(reply["id"], "c1");
    assert_eq!(reply["data"]["reason"], "unauthorized", "reply: {reply}");

    let _ = kernel
        .send(&AgentId::from(bid.as_str()), json!({"type": "shutdown"}))
        .await;
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
    kernel
        .send(
            &AgentId::from("core"),
            json!({
                "type": "create_agent",
                "handler_module": "file_bridge.tools",
                "id": "fb",
                "root": tmp.path().to_string_lossy(),
                "ingress_rule": "allow_all",
            }),
        )
        .await;
    let bid = id_for("brg_bin", &tmp);
    create_bridge_with_auth(&kernel, &bid, "stand_in", "allow_all").await;
    let peer = inject_one(&AgentId::from(bid.as_str()));
    let _ = kernel
        .send(&AgentId::from(bid.as_str()), json!({"type": "boot"}))
        .await;

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
        .send(&AgentId::from(bid.as_str()), json!({"type": "shutdown"}))
        .await;
}

#[tokio::test]
async fn deny_inbound_default_refuses_inbound_call() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp).await;
    let bid = id_for("brg_sealed", &tmp);
    // No `ingress_rule` meta ⇒ DenyInbound (SEALED by default).
    kernel
        .send(
            &AgentId::from("core"),
            json!({
                "type": "create_agent",
                "handler_module": WS_HANDLER_MODULE,
                "id": bid,
                "peer_id": "stand_in",
                "transport": "memory",
            }),
        )
        .await;
    let peer = inject_one(&AgentId::from(bid.as_str()));
    let _ = kernel
        .send(&AgentId::from(bid.as_str()), json!({"type": "boot"}))
        .await;
    let reflect = kernel
        .send(&AgentId::from(bid.as_str()), json!({"type": "reflect"}))
        .await;
    assert_eq!(reflect["ingress_rule"], "deny_inbound");
    assert_eq!(reflect["sealed"], true);

    peer.send_frame(json!({
        "type": "call",
        "id": "c2",
        "target": "core",
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
        .send(&AgentId::from(bid.as_str()), json!({"type": "shutdown"}))
        .await;
}

#[tokio::test]
async fn password_gate_checks_inbound_and_presents_on_forward() {
    // Unique env var so parallel tests don't collide (codebase pattern).
    let env = "FANTASTIC_GROUP_TOKEN_RS_INTEG";
    std::env::set_var(env, "s3cret");
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp).await;
    let bid = id_for("brg_pw", &tmp);
    kernel
        .send(
            &AgentId::from("core"),
            json!({
                "type": "create_agent",
                "handler_module": WS_HANDLER_MODULE,
                "id": bid,
                "peer_id": "stand_in",
                "transport": "memory",
                "auth": {"policy": "password", "token_env": env},
            }),
        )
        .await;
    let peer = inject_one(&AgentId::from(bid.as_str()));
    let _ = kernel
        .send(&AgentId::from(bid.as_str()), json!({"type": "boot"}))
        .await;
    // reflect surfaces only the policy NAME (never the env-var config).
    let reflect = kernel
        .send(&AgentId::from(bid.as_str()), json!({"type": "reflect"}))
        .await;
    assert_eq!(reflect["ingress_rule"], "password");
    assert_eq!(reflect["sealed"], true);

    // (1) inbound call WITH the matching envelope token dispatches.
    peer.send_frame(json!({
        "type": "call", "id": "ok", "target": "core",
        "payload": {"type": "reflect"}, "auth_token": "s3cret",
    }))
    .await
    .unwrap();
    let good = peer.recv_frame().await.expect("a reply frame").into_value();
    assert_eq!(
        good["data"]["id"], "core",
        "valid token should dispatch: {good}"
    );

    // (2) inbound call with a WRONG token is refused unauthorized.
    peer.send_frame(json!({
        "type": "call", "id": "bad", "target": "core",
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
        kc.send(
            &AgentId::from(bid2.as_str()),
            json!({"type": "forward", "target": "remote", "payload": {"type": "reflect"}}),
        )
        .await
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
        .send(&AgentId::from(bid.as_str()), json!({"type": "shutdown"}))
        .await;
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
    kernel
        .send(
            &AgentId::from("core"),
            json!({
                "type": "create_agent",
                "handler_module": WS_HANDLER_MODULE,
                "id": bid,
                "peer_id": "stand_in",
                "transport": "memory",
                "ingress_rule": "deny_inbound",
                "egress_rule": {"type": "password", "env": env},
            }),
        )
        .await;
    let peer = inject_one(&AgentId::from(bid.as_str()));
    let _ = kernel
        .send(&AgentId::from(bid.as_str()), json!({"type": "boot"}))
        .await;
    // reflect surfaces both directions independently.
    let reflect = kernel
        .send(&AgentId::from(bid.as_str()), json!({"type": "reflect"}))
        .await;
    assert_eq!(reflect["ingress_rule"], "deny_inbound");
    assert_eq!(reflect["egress_rule"], "password");
    // no `auth` in reflect — a legacy WRITE shorthand only (py parity)
    assert!(reflect.get("auth").is_none());
    assert_eq!(reflect["sealed"], true);

    // inbound is refused even with a token (ingress = deny_inbound, not password)
    peer.send_frame(json!({
        "type": "call", "id": "in", "target": "core",
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
        kc.send(
            &AgentId::from(bid2.as_str()),
            json!({"type": "forward", "target": "remote", "payload": {"type": "reflect"}}),
        )
        .await
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
        .send(&AgentId::from(bid.as_str()), json!({"type": "shutdown"}))
        .await;
    std::env::remove_var(env);
}
