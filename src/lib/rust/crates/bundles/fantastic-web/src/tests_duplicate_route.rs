//! TWO CHILDREN, ONE LITERAL ROUTE: refused, never silently shadowed. Route
//! resolution is first-match-wins, so the second declaration of the same literal
//! `(kind, method, path)` could never answer. Catch-all patterns such as
//! `/{host_id}/ws` (one per `web_ws` leg) stay allowed. The twin of python
//! `web/tests/test_mount.py::test_two_children_declaring_the_same_literal_route_path_are_refused_at_boot`
//! and the four tests beside it.

use super::*;
use crate::routes::{resolve, Resolved};

/// A leg that declares ONE configurable route.
struct DupLeg(Value);

#[async_trait::async_trait]
impl fantastic_kernel::bundle::Bundle for DupLeg {
    fn name(&self) -> &str {
        "dup_leg"
    }
    async fn handle(
        &self,
        _id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<fantastic_kernel::bundle::Reply, fantastic_kernel::bundle::BundleError> {
        if payload["type"] == "get_routes" {
            return Ok(Some(json!({"routes": [self.0.clone()]})));
        }
        Ok(None)
    }
}

fn ws(path: &str) -> Value {
    json!({"kind": "websocket", "path": path})
}

fn http_get(path: &str) -> Value {
    json!({"kind": "http", "method": "GET", "path": path})
}

async fn make(kernel: &Arc<Kernel>, rec: Value) {
    let mut p = json!({"type": "create_agent"});
    for (k, v) in rec.as_object().unwrap() {
        p[k] = v.clone();
    }
    let r = kernel
        .send(vec![Item::new(AgentId::from("kernel"), p)])
        .await
        .remove(0)
        .result;
    assert_eq!(r["id"], rec["id"], "{r}");
}

fn leg_rec(web_id: &str, id: &str, module: &str) -> Value {
    json!({"parent_id": web_id, "handler_module": module, "id": id,
           "callable_by": {"$parent": ["get_routes"]}})
}

/// A kernel with a web host `web_id` on a free port, no children yet. `legs`
/// registers each `(module, routes)` bundle.
async fn host(tmp: &TempDir, web_id: &str, legs: Vec<(&str, Value)>) -> (Arc<Kernel>, u16) {
    host_with(tmp, web_id, legs, true).await
}

/// `port_now: false` creates the host WITHOUT a port (create-time boot refuses,
/// nothing serves); the caller sets it with [`give_port`] once the children exist.
async fn host_with(
    tmp: &TempDir,
    web_id: &str,
    legs: Vec<(&str, Value)>,
    port_now: bool,
) -> (Arc<Kernel>, u16) {
    let port = free_port();
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, WebBundle);
    for (module, route) in legs {
        kernel.bundles.register(module, DupLeg(route));
    }
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
    let mut rec = json!({"parent_id": "kernel", "handler_module": HANDLER_MODULE,
               "callable_by": {"$subtree": ["watch"]}, "can_call": {"$subtree": ["get_routes"]},
               "id": web_id});
    if port_now {
        rec["port"] = json!(port);
    }
    make(&kernel, rec).await;
    (kernel, port)
}

async fn give_port(kernel: &Arc<Kernel>, web_id: &str, port: u16) {
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "update_agent", "id": web_id, "port": port}),
        )])
        .await
        .remove(0)
        .result;
    assert!(r.get("error").is_none(), "{r}");
}

async fn verb(kernel: &Arc<Kernel>, web_id: &str, payload: Value) -> Value {
    kernel
        .send(vec![Item::new(AgentId::from(web_id), payload)])
        .await
        .remove(0)
        .result
}

fn table_of(web_id: &str) -> Arc<AsyncRwLock<crate::routes::RouteTable>> {
    SERVERS
        .lock()
        .get(&AgentId::from(web_id))
        .map(|h| Arc::clone(&h.table))
        .expect("a running host")
}

#[tokio::test]
async fn two_children_declaring_the_same_literal_route_path_are_refused_at_boot() {
    let tmp = TempDir::new().unwrap();
    let web_id = "web_dup_boot";
    let (kernel, port) = host_with(
        &tmp,
        web_id,
        vec![
            ("dup_a.tools", ws("/shared/ws")),
            ("dup_b.tools", ws("/shared/ws")),
        ],
        false,
    )
    .await;
    make(&kernel, leg_rec(web_id, "a_dup", "dup_a.tools")).await;
    make(&kernel, leg_rec(web_id, "b_dup", "dup_b.tools")).await;
    give_port(&kernel, web_id, port).await;
    let r = verb(&kernel, web_id, json!({"type": "boot"})).await;
    let text = r.to_string();
    assert!(r.get("running").is_none(), "boot must refuse: {r}");
    for needle in ["RuntimeError", "/shared/ws", "a_dup", "b_dup"] {
        assert!(text.contains(needle), "{needle} missing: {r}");
    }
    let serving = SERVERS
        .lock()
        .get(&AgentId::from(web_id))
        .map(|h| h.task.is_some())
        .unwrap_or(false);
    assert!(!serving, "no serving task may start");
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err(),
        "nothing may answer on the port"
    );
    stop(&kernel, web_id).await;
}

#[tokio::test]
async fn a_duplicate_literal_route_is_refused_on_mount_too() {
    let tmp = TempDir::new().unwrap();
    let web_id = "web_dup_mount";
    let (kernel, _) = host(
        &tmp,
        web_id,
        vec![
            ("dup_a.tools", ws("/shared/ws")),
            ("dup_b.tools", ws("/shared/ws")),
        ],
    )
    .await;
    make(&kernel, leg_rec(web_id, "a_dup", "dup_a.tools")).await;
    let r = verb(&kernel, web_id, json!({"type": "boot"})).await;
    assert_eq!(r["running"], true, "boot: {r}");
    make(&kernel, leg_rec(web_id, "b_dup", "dup_b.tools")).await;
    let r = verb(
        &kernel,
        web_id,
        json!({"type": "mount", "child_id": "b_dup"}),
    )
    .await;
    let err = r["error"]
        .as_str()
        .unwrap_or_else(|| panic!("no error: {r}"));
    for needle in ["/shared/ws", "a_dup", "b_dup"] {
        assert!(err.contains(needle), "{needle} missing: {r}");
    }
    let table = table_of(web_id);
    let t = table.read().await;
    assert!(t.paths_for(&AgentId::from("b_dup")).is_empty());
    match resolve(&t, "/shared/ws", &axum::http::Method::GET, true) {
        Resolved::Full(route, _) => {
            assert!(matches!(route.handler, crate::routes::Handler::Ws(ref l) if l.0 == "a_dup"))
        }
        _ => panic!("the first leg must still answer"),
    }
    drop(t);
    stop(&kernel, web_id).await;
}

#[tokio::test]
async fn the_same_catch_all_pattern_on_two_ws_legs_is_still_allowed() {
    let tmp = TempDir::new().unwrap();
    let web_id = "web_dup_pattern";
    let (kernel, _) = host(
        &tmp,
        web_id,
        vec![
            ("dup_a.tools", ws("/{host_id}/ws")),
            ("dup_b.tools", ws("/{host_id}/ws")),
        ],
    )
    .await;
    make(&kernel, leg_rec(web_id, "a_dup", "dup_a.tools")).await;
    make(&kernel, leg_rec(web_id, "b_dup", "dup_b.tools")).await;
    let r = verb(&kernel, web_id, json!({"type": "boot"})).await;
    assert_eq!(r["running"], true, "boot: {r}");
    stop(&kernel, web_id).await;
}

#[tokio::test]
async fn the_same_literal_path_with_different_kinds_or_methods_is_allowed() {
    let tmp = TempDir::new().unwrap();
    let web_id = "web_dup_kinds";
    let post = json!({"kind": "http", "method": "POST", "path": "/shared/_reflect"});
    let (kernel, _) = host(
        &tmp,
        web_id,
        vec![
            ("dup_a.tools", ws("/shared/_reflect")),
            ("dup_b.tools", http_get("/shared/_reflect")),
            ("dup_c.tools", post),
        ],
    )
    .await;
    make(&kernel, leg_rec(web_id, "a_dup", "dup_a.tools")).await;
    make(&kernel, leg_rec(web_id, "b_dup", "dup_b.tools")).await;
    make(&kernel, leg_rec(web_id, "c_dup", "dup_c.tools")).await;
    let r = verb(&kernel, web_id, json!({"type": "boot"})).await;
    assert_eq!(r["running"], true, "boot: {r}");
    let table = table_of(web_id);
    for c in ["a_dup", "b_dup", "c_dup"] {
        assert_eq!(
            table.read().await.paths_for(&AgentId::from(c)),
            vec!["/shared/_reflect"]
        );
    }
    stop(&kernel, web_id).await;
}

#[tokio::test]
async fn a_reboot_of_one_child_does_not_conflict_with_itself() {
    let tmp = TempDir::new().unwrap();
    let web_id = "web_dup_self";
    let (kernel, _) = host(&tmp, web_id, vec![("dup_a.tools", ws("/shared/ws"))]).await;
    make(&kernel, leg_rec(web_id, "a_dup", "dup_a.tools")).await;
    let r = verb(&kernel, web_id, json!({"type": "boot"})).await;
    assert_eq!(r["running"], true, "boot: {r}");
    let r = verb(
        &kernel,
        web_id,
        json!({"type": "mount", "child_id": "a_dup"}),
    )
    .await;
    assert!(r.get("error").is_none(), "{r}");
    assert_eq!(r["mounted"], json!(["/shared/ws"]));
    stop(&kernel, web_id).await;
}
