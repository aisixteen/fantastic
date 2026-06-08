//! Unit tests for kernel_bridge.
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
    kernel.bundles.register(HANDLER_MODULE, KernelBridgeBundle);
    kernel
        .bundles
        .register("file.tools", fantastic_file::FileBundle);
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
    kernel
        .send(
            &AgentId::from("core"),
            json!({
                "type": "create_agent",
                "handler_module": HANDLER_MODULE,
                "id": id,
                "peer_id": peer_id,
                "transport": transport,
            }),
        )
        .await;
}

#[test]
fn readme_present_and_titled() {
    assert!(!README.is_empty());
    assert!(README.contains("kernel_bridge"));
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
                "handler_module": "file.tools",
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
        .expect("peer should receive a frame");
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
                "handler_module": HANDLER_MODULE,
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
                "handler_module": HANDLER_MODULE,
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
        tb.recv_frame().await.unwrap(),
        json!({"type": "call", "id": "1", "target": "x"})
    );
    // keepalive is dropped; the real reply (and a >64KB frame) surface intact.
    let blob = "x".repeat(200_000);
    tb.send_frame(json!({"type": "keepalive"})).await.unwrap();
    tb.send_frame(json!({"type": "reply", "id": "1", "data": {"blob": blob}}))
        .await
        .unwrap();
    let got = ta.recv_frame().await.unwrap();
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
