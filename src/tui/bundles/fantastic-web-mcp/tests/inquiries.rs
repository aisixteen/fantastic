//! AN INQUIRY TO THE HUMAN ON THE MCP LEG (#307), against a real kernel.
//!
//! An agent that cannot proceed without a decision asks the person who drives the
//! kernel over the CLI/MCP leg. The leg accepts it, holds it on its record, lists it,
//! and the answer goes back to the asker under the inquiry's pid. The only fake is the
//! asker, a test bundle that asks and records what it is sent.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use fantastic_kernel::{
    bootstrap::{bootstrap, BootstrapOptions},
    bundle::BundleError,
    AgentId, Bundle, BundleRegistry, Item, Kernel, Reply,
};
use fantastic_web_mcp::{WebMcpBundle, HANDLER_MODULE};
use serde_json::{json, Value};

/// Asks the leg an inquiry on request, and records every message it is sent.
#[derive(Default)]
struct Asker {
    got: Mutex<Vec<String>>,
}

#[async_trait]
impl Bundle for Asker {
    fn name(&self) -> &str {
        "asker"
    }
    async fn handle(
        &self,
        _id: &AgentId,
        p: &Value,
        k: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        match p.get("type").and_then(Value::as_str) {
            Some("ask") => {
                let ask = json!({"type": "inquiry", "text": p["text"], "request_id": "r1"});
                let out = Arc::clone(k)
                    .send(vec![Item::new(AgentId::from("mcp"), ask)])
                    .await
                    .remove(0);
                Ok(Some(out.result))
            }
            Some("send") => {
                self.got
                    .lock()
                    .unwrap()
                    .push(p["text"].as_str().unwrap_or("").to_string());
                Ok(Some(json!({"ok": true})))
            }
            Some("got") => Ok(Some(json!({"got": self.got.lock().unwrap().clone()}))),
            _ => Ok(Some(
                json!({"verbs": {"ask": "args: text", "got": "no args"}}),
            )),
        }
    }
}

async fn op(k: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    // a send outside every dispatch is the operator's
    Arc::clone(k)
        .send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
        .result
}

/// A kernel with the leg `mcp` (its record may already carry `open`) and an `asker`.
async fn setup(open: Option<Value>) -> Arc<Kernel> {
    let mut reg = BundleRegistry::new();
    reg.register(HANDLER_MODULE, WebMcpBundle);
    reg.register("asker.tools", Asker::default());
    let k = Arc::clone(
        &bootstrap(reg, BootstrapOptions::in_memory())
            .expect("boots")
            .kernel,
    );
    let root = k.root().expect("root").id.0.clone();
    let r = op(
        &k,
        &root,
        json!({"type": "create_agent", "parent_id": root, "id": "asker",
        "handler_module": "asker.tools", "can_call": {"mcp": "*"}, "callable_by": {"mcp": "*"}}),
    )
    .await;
    assert!(r.get("error").is_none(), "create asker: {r}");
    let mut mcp = json!({"type": "create_agent", "parent_id": root, "id": "mcp",
        "handler_module": HANDLER_MODULE, "can_call": {"asker": "*"}, "callable_by": {"asker": "*"}});
    if let Some(o) = open {
        mcp["open_inquiries"] = o;
    }
    let r = op(&k, &root, mcp).await;
    assert!(r.get("error").is_none(), "create mcp: {r}");
    k
}

async fn ask(k: &Arc<Kernel>, text: &str) -> Value {
    op(k, "asker", json!({"type": "ask", "text": text})).await
}

#[tokio::test(flavor = "multi_thread")]
async fn an_inquiry_to_the_mcp_leg_is_accepted_and_listed() {
    let k = setup(None).await;
    let r = ask(&k, "ship?").await;
    assert!(r.get("error").is_none(), "the leg refused the inquiry: {r}");
    assert_eq!(r["accepted"], true, "{r}");
    let re = r["re"].as_u64().expect("an inquiry pid");
    assert_eq!(r["request_id"], "r1");

    let l = op(&k, "mcp", json!({"type": "inquiries"})).await;
    let rows = l["inquiries"].as_array().expect("a list");
    assert_eq!(rows.len(), 1, "{l}");
    assert_eq!(rows[0]["re"].as_u64(), Some(re));
    assert_eq!(rows[0]["asker"], "asker");
    assert_eq!(rows[0]["text"], "ship?");
    assert_eq!(rows[0]["request_id"], "r1");

    let none = op(
        &k,
        "mcp",
        json!({"type": "inquiries", "asker": "someone_else"}),
    )
    .await;
    assert_eq!(
        none["inquiries"],
        json!([]),
        "a named asker lists only its own"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn answering_it_reaches_the_asker_under_its_pid() {
    let k = setup(None).await;
    let re = ask(&k, "ship?").await["re"]
        .as_u64()
        .expect("an inquiry pid");

    let r = op(
        &k,
        "mcp",
        json!({"type": "answer", "re": re, "text": "ship it"}),
    )
    .await;
    assert_eq!(r["answered"].as_u64(), Some(re), "{r}");
    assert_eq!(r["asker"], "asker");

    let got = op(&k, "asker", json!({"type": "got"})).await;
    let msgs = got["got"].as_array().expect("messages");
    assert_eq!(msgs.len(), 1, "{got}");
    let m = msgs[0].as_str().unwrap();
    assert!(
        m.starts_with(&format!("[reply to {re} · mcp inquiry]")),
        "{m}"
    );
    assert!(
        m.contains("<answer>ship it</answer>") && m.contains("<request_id>r1</request_id>"),
        "{m}"
    );

    let l = op(&k, "mcp", json!({"type": "inquiries"})).await;
    assert_eq!(l["inquiries"], json!([]), "an answered inquiry is closed");
    let again = op(
        &k,
        "mcp",
        json!({"type": "answer", "re": re, "text": "twice"}),
    )
    .await;
    assert_eq!(again["reason"], "not_found", "{again}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unanswered_inquiry_survives_a_restart() {
    let a = setup(None).await;
    let re = ask(&a, "ship?").await["re"]
        .as_u64()
        .expect("an inquiry pid");
    // what the record writer persists: the open list is ON THE LEG'S RECORD
    let held = {
        let e = a.agents.get(&AgentId::from("mcp")).expect("the leg");
        let m = e.meta.read().expect("meta").clone();
        m.get("open_inquiries")
            .cloned()
            .expect("the inquiry is on the record")
    };
    assert_eq!(held.as_array().map(Vec::len), Some(1), "{held}");

    // the next start: a fresh kernel whose leg record carries what was written
    let b = setup(Some(held)).await;
    let l = op(&b, "mcp", json!({"type": "inquiries"})).await;
    assert_eq!(l["inquiries"][0]["re"].as_u64(), Some(re), "{l}");
    let r = op(
        &b,
        "mcp",
        json!({"type": "answer", "re": re, "text": "yes"}),
    )
    .await;
    assert_eq!(r["answered"].as_u64(), Some(re), "{r}");
    let got = op(&b, "asker", json!({"type": "got"})).await;
    assert!(
        got["got"][0]
            .as_str()
            .unwrap()
            .contains("<answer>yes</answer>"),
        "{got}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_inquiry_without_text_is_refused() {
    let k = setup(None).await;
    let r = ask(&k, "  ").await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(r["arg"], "text");
    let l = op(&k, "mcp", json!({"type": "inquiries"})).await;
    assert_eq!(l["inquiries"], json!([]));
}
