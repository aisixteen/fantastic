//! THE HOME KERNEL'S TREE, and every grant in it, written whole on every boot.
//!
//! ```text
//! kernel_state          kernel_state.tools   the root: guid, next_id, memory_store_id: home_memory
//! ├── home_memory       sqlite_store.tools   path: home.db                               (seed)
//! ├── store             file_bridge.tools    root: ., allow: ["."]                       (seed)
//! └── user              user.tools           every page call goes out through it
//!     ├── home          home.tools           display_name: home; pages, selection
//!     └── web           web.tools            the HTTP host on 127.0.0.1, port
//!         ├── home_ws       web_ws.tools        the socket leg, ingress_rule: allow_all_ingress
//!         └── canvas_page   canvas_page.tools   serves home.html (and the canvas bundle)
//!             └── web_loader kernel_state.tools root: web, watch: false
//! ```
//!
//! WHAT THE PAGE'S SOCKET REACHES. The leg `home_ws` holds `forward` on `user`
//! (every page call is `user.forward`) and `watch` on `home` (`/home/ws`, the
//! state stream) — no `watch` on `user`, which would mirror every client's
//! calls to every page. Through `user`: `home`'s verbs, the root's `reflect` and
//! `docs` (no tree verb, so no page can widen a grant), and `web_loader`'s
//! record verbs. `home_memory`, `store`, `web` and `canvas_page` are reached by
//! no page. There is no god mode here.

use std::sync::Arc;

use fantastic_kernel::bootstrap::DEFAULT_ROOT_ID;
use fantastic_kernel::{AgentId, Item, Kernel};
use serde_json::{json, Map, Value};

use crate::home;
use crate::home::sys;

pub const ROOT: &str = DEFAULT_ROOT_ID;
pub const HOME_MEMORY: &str = "home_memory";
pub const STORE: &str = "store";
pub const USER: &str = "user";
pub const HOME: &str = home::ID;
pub const WEB: &str = "web";
pub const HOME_WS: &str = "home_ws";
pub const CANVAS_PAGE: &str = "canvas_page";
pub const WEB_LOADER: &str = "web_loader";

/// The record verbs the page loader sends (`src/lib/ts/src/bundles/loader/proxy_loader.ts`).
pub const LOADER_VERBS: [&str; 3] = ["load_tree", "persist_record", "forget_record"];

/// Every grant of the tree: `(agent, can_call, callable_by)`, the `$self`
/// baseline left to the kernel.
pub fn grants() -> Vec<(&'static str, Value, Value)> {
    let verbs = json!(home::VERBS);
    let kernel_verbs = json!(["reflect", "docs"]);
    let loader = json!(LOADER_VERBS);
    vec![
        (ROOT, json!({}), json!({USER: kernel_verbs})),
        (HOME_MEMORY, json!({}), json!({})),
        (STORE, json!({}), json!({})),
        (
            USER,
            json!({HOME: verbs, ROOT: kernel_verbs, WEB_LOADER: loader}),
            json!({HOME_WS: ["forward"]}),
        ),
        (HOME, json!({}), json!({USER: verbs, HOME_WS: ["watch"]})),
        (
            WEB,
            json!({HOME_WS: ["get_routes"], CANVAS_PAGE: ["read_stream"]}),
            json!({}),
        ),
        (
            HOME_WS,
            json!({USER: ["forward"], HOME: ["watch"]}),
            json!({WEB: ["get_routes"]}),
        ),
        (CANVAS_PAGE, json!({}), json!({WEB: ["read_stream"]})),
        (WEB_LOADER, json!({}), json!({USER: loader})),
    ]
}

fn exists(kernel: &Kernel, id: &str) -> bool {
    kernel.agents.contains_key(&AgentId::from(id))
}

/// `id`'s record meta (empty when there is no such agent).
pub fn meta(kernel: &Kernel, id: &str) -> Map<String, Value> {
    kernel
        .agents
        .get(&AgentId::from(id))
        .map(|a| a.value().record().meta.clone())
        .unwrap_or_default()
}

/// The port `web`'s record names.
pub fn recorded_port(kernel: &Kernel) -> Option<u16> {
    meta(kernel, WEB)
        .get("port")
        .and_then(Value::as_u64)
        .and_then(|p| u16::try_from(p).ok())
}

/// One operator send; its reply, or the refusal's text.
pub async fn operator(kernel: &Arc<Kernel>, target: &str, payload: Value) -> Result<Value, String> {
    let out = Arc::clone(kernel)
        .send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0);
    match out.result.get("error").and_then(Value::as_str) {
        Some(e) => Err(sys::with_hint(e, &out.result)),
        None => Ok(out.result),
    }
}

async fn create(kernel: &Arc<Kernel>, mut spec: Value) -> Result<(), String> {
    let id = spec["id"].as_str().unwrap_or_default().to_string();
    spec["type"] = json!("create_agent");
    operator(kernel, "kernel", spec)
        .await
        .map(|_| ())
        .map_err(|e| format!("could not create `{id}`: {e}"))
}

/// Create what is missing, write every grant whole, boot, mount. `port` is the
/// host's `--port`: a new `web` takes it, or a free port when none is given; a
/// recorded one was already set to it before `web` booted. The port served.
pub async fn ensure(kernel: &Arc<Kernel>, port: Option<u16>) -> Result<u16, String> {
    let fresh_web = !exists(kernel, WEB);
    let port = if fresh_web {
        match port {
            Some(p) => p,
            None => sys::free_port()?,
        }
    } else {
        let recorded = recorded_port(kernel).ok_or("the `web` record has no port")?;
        if let Some(p) = port.filter(|p| *p != recorded) {
            return Err(format!("port {p}: the `web` leg still serves port {recorded} (its record refused the new port)"));
        }
        recorded
    };
    let fresh_ws = !exists(kernel, HOME_WS);
    let made = [
        (
            USER,
            json!({"parent_id": ROOT, "handler_module": fantastic_user::HANDLER_MODULE}),
        ),
        (
            HOME,
            json!({"parent_id": USER, "handler_module": home::HANDLER_MODULE, "display_name": "home"}),
        ),
        (
            WEB,
            json!({"parent_id": USER, "handler_module": fantastic_web::HANDLER_MODULE, "port": port}),
        ),
        (
            HOME_WS,
            json!({"parent_id": WEB, "handler_module": fantastic_web_ws::HANDLER_MODULE, "ingress_rule": "allow_all_ingress"}),
        ),
        (
            CANVAS_PAGE,
            json!({"parent_id": WEB, "handler_module": fantastic_canvas_page::HANDLER_MODULE,
                   "verbs": {"read_stream": fantastic_canvas_page::READ_STREAM_DOC}}),
        ),
        (
            WEB_LOADER,
            json!({"parent_id": CANVAS_PAGE, "handler_module": fantastic_core::HANDLER_MODULE,
                   "root": "web", "watch": false}),
        ),
    ];
    for (id, mut spec) in made {
        if !exists(kernel, id) {
            spec["id"] = json!(id);
            create(kernel, spec).await?;
        }
    }
    for (id, can_call, callable_by) in grants() {
        operator(kernel, "kernel", json!({"type": "update_agent", "id": id, "can_call": can_call, "callable_by": callable_by}))
            .await
            .map_err(|e| format!("could not write the grants of `{id}`: {e}"))?;
    }
    // `web`'s boot mounts its children and then listens: last, and only when it
    // was made here (a hydrated host is listening already).
    let mut boots = vec![CANVAS_PAGE, HOME_WS, WEB_LOADER, HOME];
    if fresh_web {
        boots.push(WEB);
    }
    for id in boots {
        operator(kernel, id, json!({"type": "boot"}))
            .await
            .map_err(|e| format!("`{id}` did not boot: {e}"))?;
    }
    if fresh_ws && !fresh_web {
        operator(kernel, WEB, json!({"type": "mount", "child_id": HOME_WS}))
            .await
            .map_err(|e| format!("could not mount `{HOME_WS}`: {e}"))?;
    }
    // The host logs a bind failure and goes on; a home whose page cannot be
    // served is not booted.
    let web = operator(kernel, WEB, json!({"type": "reflect"})).await?;
    if web.get("running") != Some(&json!(true)) {
        return Err(format!(
            "`{WEB}` is not listening on 127.0.0.1:{port} (is the port in use?)"
        ));
    }
    Ok(port)
}
