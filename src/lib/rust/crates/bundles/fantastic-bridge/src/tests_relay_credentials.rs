//! The relay password lives in a credential_store — never on the record. The
//! twin of python `relay_connector/tests/test_relay_credentials.py`.
//!
//! A secret lives only in a credential_store, never in a plain record field
//! (visible in `list_agents`, and in cleartext in
//! `.fantastic/agents/<id>/agent.yaml`). The connector's record names WHERE the
//! password lives — `credentials: {store, ref}` — and before every dial it reads
//! the ref's `password` field AS ITSELF, the answer marked `secret: ['result']`,
//! pinned to the relay URL's host, exactly as an AI backend reads its key. There
//! is no `relay_token` field.

use super::*;
use futures_util::StreamExt;

const PW: &str = "relay-group-pw-SECRET-9f9f";
const PW2: &str = "relay-group-pw-ROTATED-0e0e";

/// Every upgrade the hub saw: `(path, X-Fantastic-Auth)`.
type Dials = Arc<std::sync::Mutex<Vec<(String, String)>>>;

/// A relay that accepts every upgrade and records its path and auth header — the
/// dial is what is under test, not the routing.
async fn recording_relay() -> (String, Dials) {
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
    use tokio_tungstenite::tungstenite::http::HeaderValue;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let dials: Dials = Arc::default();
    let seen = Arc::clone(&dials);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let seen = Arc::clone(&seen);
            tokio::spawn(async move {
                #[allow(clippy::result_large_err)]
                let cb = move |req: &Request, mut resp: Response| {
                    let auth = req
                        .headers()
                        .get("X-Fantastic-Auth")
                        .map(|v| String::from_utf8_lossy(v.as_bytes()).to_string())
                        .unwrap_or_default();
                    let path = req.uri().path().to_string();
                    seen.lock().unwrap().push((path, auth));
                    resp.headers_mut().insert(
                        "Sec-WebSocket-Protocol",
                        HeaderValue::from_static(transport::relay::SUBPROTOCOL),
                    );
                    Ok(resp)
                };
                let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(stream, cb).await else {
                    return;
                };
                while let Some(Ok(_)) = ws.next().await {}
            });
        }
    });
    (format!("ws://127.0.0.1:{port}"), dials)
}

/// python's `relay_kernel` fixture: a kernel with a credential store `creds`
/// holding the relay password at ref `relay/group`, bound to the relay's host and
/// readable by the leg; returns the kernel, the relay URL, its dials, the leg id.
async fn relay_kernel(tmp: &TempDir) -> (Arc<Kernel>, String, Dials, String) {
    let mut kernel = Kernel::new();
    kernel
        .bundles
        .register(RELAY_HANDLER_MODULE, RelayConnectorBundle);
    register_relay_store(&mut kernel.bundles);
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
    let (url, dials) = recording_relay().await;
    let leg = id_for("rl", tmp);
    let host = url.trim_start_matches("ws://").to_string();
    let made = ask(
        &kernel,
        "kernel",
        json!({"type": "create_agent", "parent_id": "kernel_state",
               "handler_module": fantastic_credential_store::HANDLER_MODULE, "id": "creds",
               "backend": "keychain",
               "refs": {RELAY_REF: {"host": host}},
               "callable_by": {leg.as_str(): ["get"]}}),
    )
    .await;
    assert_eq!(made["id"], "creds", "{made}");
    (kernel, url, dials, leg)
}

/// python `_leg`: the connector `leg`, granted `get` on the store, with `meta`.
async fn mk_leg(k: &Arc<Kernel>, leg: &str, url: &str, meta: Value) -> Value {
    let mut p = json!({"type": "create_agent", "parent_id": "kernel_state",
                       "handler_module": RELAY_HANDLER_MODULE, "id": leg, "transport": "relay",
                       "relay_url": url, "guid": "A", "partner_guid": "B",
                       "heartbeat": 0, "reconnect": 0, "can_call": {"creds": ["get"]}});
    for (key, v) in meta.as_object().cloned().unwrap() {
        p[key] = v;
    }
    ask(k, "kernel", p).await
}

fn creds() -> Value {
    json!({"credentials": {"store": "creds", "ref": RELAY_REF}})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_password_is_read_from_the_store_and_never_recorded() {
    let tmp = TempDir::new().unwrap();
    let (k, url, dials, leg) = relay_kernel(&tmp).await;
    set_relay_password(&k, "creds", PW).await;
    let made = mk_leg(&k, &leg, &url, creds()).await;
    assert_eq!(made["id"], json!(leg), "{made}");
    let booted = ask(&k, &leg, json!({"type": "boot"})).await;
    assert!(
        booted["booted"] == json!(true) || booted["already"] == json!(true),
        "{booted}"
    );
    assert_eq!(
        dials.lock().unwrap().last().cloned(),
        Some(("/A".to_string(), PW.to_string()))
    );
    // no record, listing or ledger of the tree carries it
    let rec = k.agents.get(&AgentId::from(leg.as_str())).unwrap().record();
    assert!(!format!("{rec:?}").contains(PW));
    let listed = ask(&k, "kernel", json!({"type": "list_agents"})).await;
    assert!(!listed.to_string().contains(PW), "{listed}");
    let ledger = ask(&k, "kernel", json!({"type": "ledger"})).await;
    assert!(!ledger.to_string().contains(PW), "{ledger}");
    ask(&k, &leg, json!({"type": "reconnect"})).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rotated_password_is_used_by_the_next_dial() {
    let tmp = TempDir::new().unwrap();
    let (k, url, dials, leg) = relay_kernel(&tmp).await;
    set_relay_password(&k, "creds", PW).await;
    mk_leg(&k, &leg, &url, creds()).await;
    ask(&k, &leg, json!({"type": "boot"})).await;
    set_relay_password(&k, "creds", PW2).await;
    let r = ask(&k, &leg, json!({"type": "reconnect"})).await;
    assert_eq!(r["booted"], true, "{r}");
    assert_eq!(
        dials.lock().unwrap().last().map(|d| d.1.clone()),
        Some(PW2.to_string())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_leg_with_no_credentials_fails_boot_loud() {
    let tmp = TempDir::new().unwrap();
    let (k, url, dials, leg) = relay_kernel(&tmp).await;
    mk_leg(&k, &leg, &url, json!({})).await;
    let out = ask(&k, &leg, json!({"type": "boot"})).await;
    assert_eq!(
        out,
        json!({"error": format!(
            "bridge: relay boot failed: relay_connector: no `credentials` on {leg} — name \
             the relay password's credential store and ref (update_agent {leg} \
             credentials={{store: <id>, ref: <ref>}}; the ref's field `password`, bound to \
             the relay's host)"
        )})
    );
    assert!(dials.lock().unwrap().is_empty(), "nothing dialed");
}

/// Pinned to the relay URL's host: a ref bound elsewhere is refused by the store,
/// and the boot names the store, the ref and the store's own words.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_store_refusal_fails_the_dial_naming_store_and_ref() {
    let tmp = TempDir::new().unwrap();
    let (k, url, dials, leg) = relay_kernel(&tmp).await;
    set_relay_password(&k, "creds", PW).await;
    let mut meta = creds();
    meta["relay_url"] = json!("ws://elsewhere:1");
    mk_leg(&k, &leg, &url, meta).await;
    let out = ask(&k, &leg, json!({"type": "boot"})).await;
    let host = url.trim_start_matches("ws://");
    assert_eq!(
        out,
        json!({"error": format!(
            "bridge: relay boot failed: relay_connector: credentials creds/relay/group: \
             credential_store: ref 'relay/group' is bound to host '{host}', not \
             'elsewhere:1' — refused, nothing read"
        )})
    );
    assert!(dials.lock().unwrap().is_empty() && !out.to_string().contains(PW));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_malformed_credentials_field_is_refused_at_the_write() {
    let shape = "credentials must be {store: <credential_store id>, ref: <ref>}";
    for (bad, why) in [
        (json!("creds"), shape),
        (json!({"store": "creds"}), shape),
        (
            json!({"store": "", "ref": "r"}),
            "credentials.store must be a non-empty string",
        ),
    ] {
        let tmp = TempDir::new().unwrap();
        let (k, url, _dials, leg) = relay_kernel(&tmp).await;
        let out = mk_leg(&k, &leg, &url, json!({"credentials": bad})).await;
        assert_eq!(
            out,
            json!({"error": why, "reason": "invalid_meta"}),
            "{bad}"
        );
    }
}
