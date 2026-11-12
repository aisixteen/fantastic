//! A LITERAL ROUTE BEATS A CATCH-ALL MOUNTED BEFORE IT. Mount order is the
//! children's folder order on a hydrated host; a `web_ws` leg declares the
//! catch-all `/{host_id}/ws`, and mounted first it swallowed a leg's literal
//! websocket path. The twin of python
//! `web/tests/test_mount.py::test_a_literal_route_beats_a_catch_all_mounted_before_it`.

use super::*;
use crate::routes::{resolve, Handler, Resolved};

/// A leg that declares ONE literal websocket path.
struct LiteralLeg;

fantastic_kernel::stand_in_verbs!(literal_verbs {
    GetRoutes "get_routes" {}
});

#[async_trait::async_trait]
impl fantastic_kernel::Handler for LiteralLeg {
    type Call = literal_verbs::Call;
    fn name(&self) -> &str {
        "literal_leg"
    }
    fn man(&self) -> &'static str {
        "A test stand-in."
    }
    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        literal_verbs::VERBS
    }
    fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
        literal_verbs::dynamic_verbs(agent)
    }
    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        fantastic_kernel::fixture_record!()
    }
    async fn handle(
        &self,
        _id: &AgentId,
        call: Self::Call,
        _kernel: &Arc<Kernel>,
    ) -> Result<fantastic_kernel::bundle::Reply, fantastic_kernel::bundle::BundleError> {
        if let literal_verbs::Call::GetRoutes(_) = call {
            return Ok(Some(
                json!({"routes": [{"kind": "websocket", "path": "/z_literal/ws"}]}),
            ));
        }
        Ok(None)
    }
}

/// The path the first FULL websocket match lands on, or `None`.
fn first_ws_match(table: &crate::routes::RouteTable, path: &str) -> Option<(String, Handler)> {
    match resolve(table, path, &axum::http::Method::GET, true) {
        Resolved::Full(route, _) => Some((route.path.clone(), route.handler.clone())),
        _ => None,
    }
}

#[tokio::test]
async fn a_literal_route_beats_a_catch_all_mounted_before_it() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let web_id = "web_route_order";
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, WebBundle);
    kernel
        .bundles
        .register("web_ws.tools", fantastic_web_ws::WebWsBundle);
    kernel.bundles.register("literal_leg.tools", LiteralLeg);
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
    for rec in [
        json!({"parent_id": "kernel", "handler_module": HANDLER_MODULE, "callable_by": {"$subtree": ["watch"]}, "can_call": {"$subtree": ["get_routes"]}, "id": web_id,
               "port": port}),
        // the catch-all leg sorts (and mounts) FIRST
        json!({"parent_id": web_id, "handler_module": "web_ws.tools", "can_call": {"$parent": ["watch"]}, "callable_by": {"$parent": ["get_routes"]}, "id": "a_ws",
               "ingress_rule": "allow_all_ingress"}),
        json!({"parent_id": web_id, "handler_module": "literal_leg.tools", "id": "z_literal",
               "callable_by": {"$parent": ["get_routes"]}}),
    ] {
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
    // THE BOOT MOUNT WALK.
    let r = kernel
        .send(vec![Item::new(
            AgentId::from(web_id),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["running"], true, "boot: {r}");
    let table = SERVERS
        .lock()
        .get(&AgentId::from(web_id))
        .map(|h| Arc::clone(&h.table))
        .expect("a running host");

    {
        let t = table.read().await;
        let paths: Vec<String> = t.iter().map(|r| r.path.clone()).collect();
        let at = |p: &str| paths.iter().position(|x| x == p).unwrap_or(usize::MAX);
        assert!(at("/z_literal/ws") < at("/{host_id}/ws"), "{paths:?}");
        let (path, handler) = first_ws_match(&t, "/z_literal/ws").expect("a full match");
        assert_eq!(path, "/z_literal/ws");
        assert!(
            matches!(handler, Handler::Ws(ref leg) if leg.0 == "z_literal"),
            "{handler:?}"
        );
        // the catch-all still serves every other host
        let (path, _) = first_ws_match(&t, "/elsewhere/ws").expect("a full match");
        assert_eq!(path, "/{host_id}/ws");
    }

    // RE-ORDERED ON EVERY MOUNT AND UNMOUNT: the catch-all re-mounted LAST is
    // still behind the literal, and with the literal unmounted the catch-all
    // takes the path.
    let r = kernel
        .send(vec![Item::new(
            AgentId::from(web_id),
            json!({"type": "mount", "child_id": "a_ws"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(r.get("error").is_none(), "{r}");
    assert_eq!(
        first_ws_match(&*table.read().await, "/z_literal/ws").map(|(p, _)| p),
        Some("/z_literal/ws".to_string())
    );
    let r = kernel
        .send(vec![Item::new(
            AgentId::from(web_id),
            json!({"type": "unmount", "child_id": "z_literal"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(r.get("error").is_none(), "{r}");
    assert_eq!(
        first_ws_match(&*table.read().await, "/z_literal/ws").map(|(p, _)| p),
        Some("/{host_id}/ws".to_string())
    );
    stop(&kernel, web_id).await;
}

/// A `web_ws` leg whose record names a `route` mounts ONLY that literal, beside
/// the one catch-all leg of the host; each answers as itself (#559).
#[tokio::test]
async fn a_web_ws_route_leg_beside_a_catch_all_leg_gets_its_own_url() {
    use tokio_tungstenite::tungstenite::Message as TMessage;
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let web_id = "web_ws_route";
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, WebBundle);
    kernel
        .bundles
        .register("web_ws.tools", fantastic_web_ws::WebWsBundle);
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
    for rec in [
        json!({"parent_id": "kernel", "handler_module": HANDLER_MODULE, "callable_by": {"$subtree": ["watch"]}, "can_call": {"$subtree": ["get_routes"]}, "id": web_id,
               "port": port}),
        json!({"parent_id": web_id, "handler_module": "web_ws.tools", "can_call": {"$parent": ["watch"]}, "callable_by": {"$parent": ["get_routes"]}, "id": "a_ws",
               "ingress_rule": "allow_all_ingress"}),
        json!({"parent_id": web_id, "handler_module": "web_ws.tools", "route": "/z_page_ws/ws", "callable_by": {"$parent": ["get_routes"]}, "id": "z_page_ws",
               "ingress_rule": "allow_all_ingress"}),
    ] {
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
    let r = kernel
        .send(vec![Item::new(
            AgentId::from(web_id),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["running"], true, "boot: {r}");
    let table = SERVERS
        .lock()
        .get(&AgentId::from(web_id))
        .map(|h| Arc::clone(&h.table))
        .expect("a running host");
    {
        let t = table.read().await;
        let (path, handler) = first_ws_match(&t, "/z_page_ws/ws").expect("a full match");
        assert_eq!(path, "/z_page_ws/ws");
        assert!(
            matches!(handler, Handler::Ws(ref leg) if leg.0 == "z_page_ws"),
            "{handler:?}"
        );
        let (path, handler) = first_ws_match(&t, "/user/ws").expect("a full match");
        assert_eq!(path, "/{host_id}/ws");
        assert!(
            matches!(handler, Handler::Ws(ref leg) if leg.0 == "a_ws"),
            "{handler:?}"
        );
        assert_eq!(t.iter().filter(|r| r.path == "/{host_id}/ws").count(), 1);
    }

    let reflect = |target: &str, id: &str| {
        json!({"type": "call", "target": target, "id": id, "payload": {"type": "reflect"}})
            .to_string()
    };
    // the literal leg answers as ITSELF
    let (mut ws, _) =
        tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/z_page_ws/ws"))
            .await
            .expect("ws connect");
    let (text, _) = round_trip(&mut ws, TMessage::Text(reflect("z_page_ws", "r1"))).await;
    let reply = text.iter().find(|v| v["id"] == "r1").expect("a reply");
    assert_eq!(reply["data"]["id"], "z_page_ws", "{reply}");
    assert_eq!(reply["data"]["path_pattern"], "/z_page_ws/ws", "{reply}");
    // the host label is the URL's first segment, as the catch-all would capture
    kernel
        .emit(
            &AgentId::from("z_page_ws"),
            json!({"type": "output", "data": "hi"}),
        )
        .await;
    let (text, _) = drain_frames(&mut ws, 600).await;
    assert!(
        text.iter()
            .any(|v| v["type"] == "event" && v["src"] == "z_page_ws"),
        "{text:?}"
    );
    // the catch-all leg still answers every other host
    let (mut ws2, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/a_ws/ws"))
        .await
        .expect("ws connect");
    let (text, _) = round_trip(&mut ws2, TMessage::Text(reflect("a_ws", "r2"))).await;
    let reply = text.iter().find(|v| v["id"] == "r2").expect("a reply");
    assert_eq!(reply["data"]["id"], "a_ws", "{reply}");
    assert_eq!(reply["data"]["path_pattern"], "/{host_id}/ws", "{reply}");
    stop(&kernel, web_id).await;
}
