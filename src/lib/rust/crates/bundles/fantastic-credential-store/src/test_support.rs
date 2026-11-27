//! TEST WIRING for an agent that reads a key from a credential store — the twin
//! of python `ai_core/tests/test_credentials.py`'s `_mk`. Composed the way an
//! operator would: one FILE-backed store under the root (its file in the test's
//! temp dir — never the user's home), the ref DECLARED by `update_agent` with its
//! its `host`, the value stored by a MARKED `set` (which never
//! declares a ref). The agent's own record then names `credentials: {store, ref}`
//! and holds a `call` line on the store ([`add_credentials`]); the store keeps no
//! list of callers.
//!
//! The store's keychain is an in-memory double ([`bundle`]), so no test ever
//! reaches the real keychain.

use std::path::Path;
use std::sync::Arc;

use fantastic_kernel::{AgentId, Item, Kernel};
use serde_json::{json, Map, Value};

use crate::keychain::MemoryKeychain;
use crate::CredentialStoreBundle;

/// The shared test store's id.
pub const STORE: &str = "creds";

/// The bundle a test registers under [`crate::HANDLER_MODULE`]: its keychain is
/// in memory.
#[must_use]
pub fn bundle() -> CredentialStoreBundle {
    CredentialStoreBundle::with_keychain(Arc::new(MemoryKeychain::default()))
}

/// The store under the root, created on first use with its file at
/// `<dir>/creds.yaml`. A `reader` already live gets a `call *` line on it, as
/// the operator would write it; a reader created later carries the line in its
/// create payload ([`add_credentials`]).
///
/// # Panics
/// When the kernel has no root or the store cannot be created.
pub async fn ensure_store(kernel: &Arc<Kernel>, dir: &Path, reader: &str) -> AgentId {
    let store = AgentId::from(STORE);
    if !kernel.exists(store.as_str()) {
        let root = kernel.root().expect("ensure_store: the kernel has no root");
        let r = fantastic_kernel::test_support::send_settled(kernel, vec![Item::new(
                root.id.clone(),
                json!({
                    "type": "create_agent",
                    "parent_id": root.id.0,
                    "handler_module": crate::HANDLER_MODULE,
                    "id": STORE,
                    "file": dir.join("creds.yaml").to_string_lossy(),
                }),
            )])
            .await
            .remove(0)
            .result;
        assert_eq!(r["id"], json!(STORE), "ensure_store: {r}");
    }
    if kernel.exists(reader) {
        fantastic_kernel::test_support::grant(kernel, reader, STORE, json!({"call": ["*"]}));
    }
    store
}

/// Store (or rotate) `key` as the `api_key` of `ref_`, bound to `endpoint`'s
/// host, the store reachable by `reader` — the operator DECLARES the ref with
/// `update_agent` (the refs already there ride along, since it replaces the map),
/// then stores the value with a `set`.
///
/// # Panics
/// When the store refuses the declaration or the write.
pub async fn set_key(
    kernel: &Arc<Kernel>,
    dir: &Path,
    reader: &str,
    ref_: &str,
    endpoint: &str,
    key: &str,
) {
    let store = ensure_store(kernel, dir, reader).await;
    let mut refs = meta_map(kernel, &store, "refs");
    refs.insert(ref_.to_string(), json!({"host": endpoint}));
    let declared = fantastic_kernel::test_support::send_settled(kernel, vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "update_agent", "id": store.0, "refs": refs}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(declared["updated"], json!(true), "set_key: {declared}");
    let out = fantastic_kernel::test_support::send_settled(kernel, vec![Item::new(
            store.clone(),
            json!({"type": "set", "ref": ref_, "fields": {"api_key": key}}),
        )])
        .await
        .remove(0);
    assert_eq!(out.result["stored"], json!(true), "set_key: {}", out.result);
}

/// What an agent's record adds to read its key from the test store, written into
/// `meta`: `credentials: {store, ref}` and a `call *` line on the store (merged
/// into the `can` already there).
pub fn add_credentials(meta: &mut Map<String, Value>, ref_: &str) {
    let mut lines = meta
        .get("can")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    lines.insert(STORE.to_string(), json!({"call": ["*"]}));
    meta.insert(
        "credentials".to_string(),
        json!({"store": STORE, "ref": ref_}),
    );
    meta.insert("can".to_string(), Value::Object(lines));
}

fn meta_map(kernel: &Kernel, id: &AgentId, key: &str) -> Map<String, Value> {
    kernel.resolve_target(id.as_str())
        .and_then(|a| {
            a.record().meta
                .get(key)
                .and_then(Value::as_object)
                .cloned()
        })
        .unwrap_or_default()
}

/// A SLOW STORE (#795): the test store, holding every call one agent makes until the
/// test opens its gate — so a test sees what that agent does while its key read is out.
/// [`held_bundle`] makes it with its gate.
pub struct HeldBundle {
    inner: CredentialStoreBundle,
    holder: String,
    held: Arc<std::sync::atomic::AtomicUsize>,
    open: tokio::sync::watch::Receiver<bool>,
}

/// The gate of a [`HeldBundle`]: how many of the holder's calls it took, and the switch
/// that lets them through.
#[derive(Clone)]
pub struct Gate {
    held: Arc<std::sync::atomic::AtomicUsize>,
    open: Arc<tokio::sync::watch::Sender<bool>>,
}

impl Gate {
    /// How many of the holder's calls the store took so far.
    #[must_use]
    pub fn held(&self) -> usize {
        self.held.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Let every held call (and every later one) through.
    pub fn open(&self) {
        let _ = self.open.send(true);
    }
}

/// The test store as a [`HeldBundle`] that holds every call of `holder`, and its gate.
#[must_use]
pub fn held_bundle(holder: &str) -> (HeldBundle, Gate) {
    let (tx, rx) = tokio::sync::watch::channel(false);
    let held = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let gate = Gate {
        held: Arc::clone(&held),
        open: Arc::new(tx),
    };
    (
        HeldBundle {
            inner: bundle(),
            holder: holder.to_string(),
            held,
            open: rx,
        },
        gate,
    )
}

#[async_trait::async_trait]
impl fantastic_kernel::bundle::Handler for HeldBundle {
    type Call = <CredentialStoreBundle as fantastic_kernel::bundle::Handler>::Call;
    fn name(&self) -> &str {
        fantastic_kernel::bundle::Handler::name(&self.inner)
    }
    fn man(&self) -> &'static str {
        fantastic_kernel::bundle::Handler::man(&self.inner)
    }
    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        fantastic_kernel::bundle::Handler::verbs(&self.inner)
    }
    fn dynamic_verbs(&self, a: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
        fantastic_kernel::bundle::Handler::dynamic_verbs(&self.inner, a)
    }
    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        fantastic_kernel::bundle::Handler::record(&self.inner)
    }
    fn payload_secret(&self, payload: &Value) -> Vec<String> {
        fantastic_kernel::bundle::Handler::payload_secret(&self.inner, payload)
    }
    fn reply_secret(&self, payload: &Value, reply: &Value) -> Vec<String> {
        fantastic_kernel::bundle::Handler::reply_secret(&self.inner, payload, reply)
    }
    async fn handle(
        &self,
        agent_id: &AgentId,
        call: Self::Call,
        kernel: &Arc<Kernel>,
    ) -> Result<fantastic_kernel::bundle::Reply, fantastic_kernel::bundle::BundleError> {
        if fantastic_kernel::current_origin().is_some_and(|a| a.as_str() == self.holder) {
            self.held.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut rx = self.open.clone();
            while !*rx.borrow_and_update() {
                if rx.changed().await.is_err() {
                    break;
                }
            }
        }
        fantastic_kernel::bundle::Handler::handle(&self.inner, agent_id, call, kernel).await
    }
}
