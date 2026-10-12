//! A backend's key, read from a credential store at USE time
//! ([`crate::credentials`]) — the twin of python
//! `ai_core/tests/test_credentials.py`.
//!
//! Against a real `credential_store` (file backend under a temp dir, keychain in
//! memory): the reader asks AS the agent, pinned to the endpoint's host, the
//! answer marked secret; nothing is cached, so a rotated key is used by the next
//! request; no value lands in the ledger.

use crate::credentials::{key_reader, CredentialError};
use crate::provider::ProviderError;
use fantastic_credential_store::test_support as creds;
use fantastic_kernel::agent::Agent;
use fantastic_kernel::{AgentId, Item, Kernel};
use serde_json::{json, Map, Value};
use std::sync::Arc;

const EP: &str = "https://api.example.com/v1/messages";
const KEY: &str = "sk-ant-live-0123456789";
const KEY2: &str = "sk-ant-rotated-9876543210";

/// A kernel with a guid, the credential store, and agent `ai` whose record names
/// `credentials: {store: creds, ref: claude}` — the ref bound to
/// `api.example.com`.
async fn mk(tmp: &tempfile::TempDir) -> Arc<Kernel> {
    let mut kernel = Kernel::new();
    kernel
        .bundles
        .register(fantastic_credential_store::HANDLER_MODULE, creds::bundle());
    kernel.bundles.register(
        fantastic_ram_store::HANDLER_MODULE,
        fantastic_ram_store::RamStoreBundle::default(),
    );
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        json!({"guid": "g1"}).as_object().cloned().unwrap(),
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    creds::set_key(&kernel, tmp.path(), "ai", "claude", "api.example.com", KEY).await;
    let mut meta = Map::new();
    creds::add_credentials(&mut meta, "claude");
    kernel.register(Agent::new(
        AgentId::from("ai"),
        None,
        Some(root.id.clone()),
        meta,
        std::path::PathBuf::new(),
        false,
    ));
    kernel
}

#[tokio::test]
async fn the_reader_reads_the_key_per_request_and_records_none() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp).await;
    let read = key_reader(&AgentId::from("ai"), &k, "responses_backend", true);
    assert_eq!(read.read(EP).await, Ok(Some(KEY.to_string())));
    // rotate
    creds::set_key(&k, tmp.path(), "ai", "claude", "api.example.com", KEY2).await;
    assert_eq!(
        read.read(EP).await,
        Ok(Some(KEY2.to_string())),
        "a rotated key is used by the next request"
    );
    let ledger = k
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "ledger"}),
        )])
        .await
        .remove(0)
        .result
        .to_string();
    assert!(!ledger.contains(KEY) && !ledger.contains(KEY2), "{ledger}");
}

#[tokio::test]
async fn a_key_bound_to_another_host_is_refused() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp).await;
    let read = key_reader(&AgentId::from("ai"), &k, "responses_backend", true);
    let e = read
        .read("https://evil.example.net/v1/messages")
        .await
        .unwrap_err();
    assert!(e.0.contains("bound to host 'api.example.com'"), "{e}");
    assert_eq!(
        e.0,
        "responses_backend: credentials creds/claude: credential_store: ref 'claude' is bound \
         to host 'api.example.com', not 'evil.example.net' — refused, nothing read"
    );
}

#[tokio::test]
async fn no_credentials_is_no_key_or_a_loud_refusal() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp).await;
    k.register(Agent::new(
        AgentId::from("bare"),
        None,
        Some(AgentId::from("kernel_state")),
        Map::new(),
        std::path::PathBuf::new(),
        false,
    ));
    let bare = AgentId::from("bare");
    assert_eq!(
        key_reader(&bare, &k, "vllm_backend", false).read(EP).await,
        Ok(None)
    );
    let e = key_reader(&bare, &k, "responses_backend", true)
        .read(EP)
        .await
        .unwrap_err();
    assert_eq!(
        e.0,
        "responses_backend: no `credentials` on bare — name the key's credential store and ref \
         (update_agent bare credentials={store: <id>, ref: <ref>})"
    );
    // a raise renders under python's class name
    let pe: ProviderError = e.into();
    assert_eq!(pe.kind, "CredentialError");
}

#[tokio::test]
async fn a_malformed_record_or_a_missing_field_is_named() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp).await;
    let ai = AgentId::from("ai");
    let set_cred = |v: Value| {
        let mut patch = Map::new();
        patch.insert("credentials".into(), v);
        k.update(&ai, patch);
    };
    set_cred(json!({"store": "creds"}));
    assert_eq!(
        key_reader(&ai, &k, "responses_backend", true)
            .read(EP)
            .await
            .unwrap_err(),
        CredentialError(
            "responses_backend: credentials must be {store: <credential_store id>, ref: <ref>}"
                .to_string()
        )
    );
    // a ref with no `api_key` field — declared by the operator's `update_agent`
    // (which replaces the map, so `claude` rides along), then stored
    set_cred(json!({"store": "creds", "ref": "other"}));
    let store = AgentId::from(creds::STORE);
    let mut refs = k.agents.get(&store).unwrap().meta.read().unwrap()["refs"].clone();
    refs["other"] = json!({"host": "api.example.com"});
    let declared = k
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "update_agent", "id": creds::STORE, "refs": refs}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(declared["updated"], json!(true), "{declared}");
    let out = k
        .send(vec![Item::new(
            store,
            json!({"type": "set", "ref": "other", "fields": {"token": "t"}}),
        )])
        .await
        .remove(0);
    assert_eq!(out.result["stored"], json!(true), "{}", out.result);
    assert_eq!(
        key_reader(&ai, &k, "responses_backend", true)
            .read(EP)
            .await
            .unwrap_err()
            .0,
        "responses_backend: credential 'other' in 'creds' has no 'api_key' field"
    );
}

/// Audit S1: a key that is not a legal HTTP header value — a control character,
/// a non-ASCII one, whitespace at an edge — is refused by the reader, naming
/// the ref and never the value (python's client quotes a refused header in its
/// error). The store keeps whatever it was given; a key with inner spaces rides.
#[tokio::test]
async fn a_key_that_is_not_a_legal_header_value_is_refused_by_its_ref() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk(&tmp).await;
    let read = key_reader(&AgentId::from("ai"), &k, "responses_backend", true);
    for bad in [
        format!("{KEY}\n"),
        format!("{KEY}\r\nX-Probe: y"),
        format!("{KEY} "),
        format!(" {KEY}"),
        format!("{KEY}\t"),
        format!("{}\u{0}{}", &KEY[..4], &KEY[4..]),
        format!("{}\u{7f}{}", &KEY[..4], &KEY[4..]),
        format!("{KEY}é"),
    ] {
        creds::set_key(&k, tmp.path(), "ai", "claude", "api.example.com", &bad).await;
        let e = read.read(EP).await.unwrap_err();
        assert_eq!(
            e.0,
            "responses_backend: credential 'claude' in 'creds': 'api_key' is not a legal \
             HTTP header value (a control or non-ASCII character, or whitespace at an edge) \
             — store it again without",
            "{bad:?}"
        );
        assert!(!e.0.contains(&KEY[4..]));
    }
    creds::set_key(&k, tmp.path(), "ai", "claude", "api.example.com", "sk a\tb").await;
    assert_eq!(read.read(EP).await, Ok(Some("sk a\tb".to_string())));
}
