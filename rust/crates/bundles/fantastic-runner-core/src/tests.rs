//! Dispatch-skeleton tests for [`RunnerCore`] via a `MockTransport`.
//!
//! These exercise the shared verb routing that both runner bundles
//! delegate to (boot=null, restart=stop+start, shutdown=stop alias,
//! unknown-verb error) without spawning processes or opening sockets.
//! Transport-specific behaviour (subprocess / ssh) is tested in each
//! runner crate.

use crate::core::RunnerCore;
use crate::transport::Transport;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Records the order verbs were dispatched in so `restart` ordering is
/// observable.
#[derive(Default)]
struct MockTransport {
    calls: Arc<std::sync::Mutex<Vec<&'static str>>>,
    seq: AtomicUsize,
}

impl MockTransport {
    fn note(&self, what: &'static str) -> usize {
        self.calls.lock().unwrap().push(what);
        self.seq.fetch_add(1, Ordering::SeqCst)
    }
}

#[async_trait]
impl Transport for MockTransport {
    async fn reflect(&self) -> Value {
        self.note("reflect");
        json!({"verb": "reflect"})
    }
    async fn start(&self) -> Value {
        let n = self.note("start");
        json!({"verb": "start", "seq": n})
    }
    async fn stop(&self) -> Value {
        let n = self.note("stop");
        json!({"verb": "stop", "seq": n})
    }
    async fn status(&self) -> Value {
        self.note("status");
        json!({"verb": "status"})
    }
    async fn get_webapp(&self) -> Value {
        self.note("get_webapp");
        json!({"verb": "get_webapp"})
    }
}

#[tokio::test]
async fn dispatches_each_simple_verb() {
    let t = MockTransport::default();
    for (verb, want) in [
        ("reflect", "reflect"),
        ("start", "start"),
        ("stop", "stop"),
        ("status", "status"),
        ("get_webapp", "get_webapp"),
    ] {
        let r = RunnerCore::handle_via(&t, "mock_runner", verb).await;
        assert_eq!(r["verb"], want, "verb {verb} routed wrong: {r}");
    }
}

#[tokio::test]
async fn boot_is_null_no_dispatch() {
    let t = MockTransport::default();
    let r = RunnerCore::handle_via(&t, "mock_runner", "boot").await;
    assert!(r.is_null(), "boot should be null, got {r}");
    assert!(
        t.calls.lock().unwrap().is_empty(),
        "boot must not touch the transport",
    );
}

#[tokio::test]
async fn shutdown_is_stop_alias() {
    let t = MockTransport::default();
    let r = RunnerCore::handle_via(&t, "mock_runner", "shutdown").await;
    assert_eq!(r["verb"], "stop");
    assert_eq!(t.calls.lock().unwrap().as_slice(), ["stop"]);
}

#[tokio::test]
async fn restart_is_stop_then_start_returning_start() {
    let t = MockTransport::default();
    let r = RunnerCore::handle_via(&t, "mock_runner", "restart").await;
    // Restart returns the START reply (stop is discarded).
    assert_eq!(r["verb"], "start", "restart should return start reply: {r}");
    assert_eq!(
        t.calls.lock().unwrap().as_slice(),
        ["stop", "start"],
        "restart must stop before start",
    );
    // start ran after stop (higher seq).
    assert_eq!(r["seq"], 1);
}

#[tokio::test]
async fn unknown_verb_errors_with_name_and_verb() {
    let t = MockTransport::default();
    let r = RunnerCore::handle_via(&t, "local_runner", "garbage").await;
    let msg = r["error"].as_str().unwrap_or("");
    assert!(msg.contains("unknown type"), "{r}");
    assert!(msg.contains("local_runner"), "{r}");
    assert!(msg.contains("garbage"), "{r}");
    assert!(t.calls.lock().unwrap().is_empty());
}
