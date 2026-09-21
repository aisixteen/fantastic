//! Unit tests for [`crate::bundle`].

use super::*;
use serde_json::json;

struct FakeBundle;

#[async_trait]
impl Bundle for FakeBundle {
    fn name(&self) -> &str {
        "fake"
    }
    async fn handle(
        &self,
        _agent_id: &AgentId,
        _payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        Ok(Some(json!({"ok": true})))
    }
}

#[tokio::test]
async fn register_and_lookup() {
    let mut reg = BundleRegistry::new();
    reg.register("fake.tools", FakeBundle);
    let b = reg.get("fake.tools").expect("registered");
    let kernel = Arc::new(Kernel::new());
    let reply = b
        .handle(&AgentId::from("x"), &json!({"type": "ping"}), &kernel)
        .await
        .unwrap();
    assert_eq!(reply, Some(json!({"ok": true})));
}

#[test]
fn unknown_handler_returns_none() {
    let reg = BundleRegistry::new();
    assert!(reg.get("nope.tools").is_none());
}

/// Default `handle_binary` impl dispatches the JSON header through `handle`
/// and DROPS the raw blob — never base64-encoded onto the payload (pure
/// streams: base64 on the wire is banned).
#[tokio::test]
async fn default_handle_binary_dispatches_header_and_drops_blob() {
    struct CaptureBundle;
    #[async_trait]
    impl Bundle for CaptureBundle {
        fn name(&self) -> &str {
            "capture"
        }
        async fn handle(
            &self,
            _agent_id: &AgentId,
            payload: &Value,
            _kernel: &Arc<Kernel>,
        ) -> Result<Reply, BundleError> {
            Ok(Some(json!({
                "type": payload.get("type").cloned().unwrap_or(Value::Null),
                "data": payload.get("data").cloned().unwrap_or(Value::Null),
            })))
        }
    }
    let b = CaptureBundle;
    let kernel = Arc::new(Kernel::new());
    let header = json!({"type": "frob"});
    let blob = vec![0xCA, 0xFE, 0xBA, 0xBE];
    let (reply, body) = b
        .handle_binary(&AgentId::from("x"), header, blob.clone(), &kernel)
        .await
        .unwrap();
    let reply = reply.unwrap();
    assert_eq!(reply["type"], "frob");
    // The raw blob is NOT base64-encoded onto the payload — `data` stays absent.
    assert_eq!(reply["data"], Value::Null);
    // Default impl returns no reply body — `None`, which is what tells a leg
    // "this verb answers no chunk" apart from a 0-byte one (C37).
    assert_eq!(body, None);
}
