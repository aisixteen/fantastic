//! THE HOME PAGE'S ORIGIN, LET INTO THE SYSTEM CANVAS (`canvas system`).
//!
//! The lock screen is a state the Home page obeys, and the `lock` agent lives in
//! the system canvas, which the Home page does not share a port with: it dials
//! that canvas's `/user/ws` (the lock channel the maintainer chose, issue #55,
//! (b)). A canvas's web host refuses a websocket whose `Origin` is neither its
//! own nor in its `web` record's `allowed_origins`, so this writes the Home's
//! origin there: the home kernel's port is only in `<home>/app/home.yaml`
//! (`{pid, port, url}`, written by the app while it serves), read here.
//!
//! The app may start after the canvas or restart on another port, so the file is
//! read again every few seconds and the record written only when an origin is
//! missing. Origins are only ever added: the two loopback spellings of the
//! Home's port.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::kern::host::Host;

/// How often `<home>/app/home.yaml` is read again.
const EVERY: Duration = Duration::from_secs(5);

/// The port `<home>/app/home.yaml` names: `port` in 1..=65535 and `url` an
/// `http://127.0.0.1:<port>/…` string. Anything else is none.
pub fn home_port(text: &str) -> Option<u16> {
    let doc: serde_yaml::Value = serde_yaml::from_str(text).ok()?;
    let map = doc.as_mapping()?;
    let port = u16::try_from(map.get("port")?.as_u64()?).ok().filter(|p| *p != 0)?;
    map.get("url")?.as_str()?.starts_with(&format!("http://127.0.0.1:{port}/")).then_some(port)
}

/// The origins a page served on `port` sends.
pub fn origins(port: u16) -> [String; 2] {
    [format!("http://127.0.0.1:{port}"), format!("http://localhost:{port}")]
}

/// Add the Home's origins to the `web` record's `allowed_origins`. True when a
/// write was made, false when there was nothing to add (no run file, no `web`).
pub async fn sync(h: &Host, home: &Path) -> Result<bool, String> {
    let Ok(text) = std::fs::read_to_string(home.join("app").join("home.yaml")) else {
        return Ok(false);
    };
    let Some(port) = home_port(&text) else { return Ok(false) };
    let Some(web) = h.kernel.agents.get(&fantastic_kernel::AgentId::from(crate::kern::web::WEB_ID)) else {
        return Ok(false);
    };
    // the record holds a list, or a bare string (`fantastic-web` reads both)
    let mut listed: Vec<String> = match web.value().record().meta.get("allowed_origins") {
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        Some(Value::String(s)) => vec![s.clone()],
        _ => vec![],
    };
    drop(web);
    let before = listed.len();
    for o in origins(port) {
        if !listed.contains(&o) {
            listed.push(o);
        }
    }
    if listed.len() == before {
        return Ok(false);
    }
    let r = h
        .operator("kernel", json!({"type": "update_agent", "id": crate::kern::web::WEB_ID, "allowed_origins": listed}))
        .await;
    match r.result.get("error").and_then(Value::as_str) {
        Some(e) => Err(format!("could not let the Home's origin into `web`: {e}")),
        None => Ok(true),
    }
}

/// Keep the Home's origin in the system canvas's `web` record, for as long as the host runs.
pub fn spawn(h: Arc<Host>) {
    tokio::spawn(async move {
        let Ok(home) = fantastic_user_home_store::home_dir_from_env() else { return };
        let mut tick = tokio::time::interval(EVERY);
        loop {
            tick.tick().await;
            if let Err(e) = sync(&h, &home).await {
                tracing::warn!("{e}");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::ClientCfg;
    use fantastic_kernel::{Agent, AgentId, Kernel};
    use serde_json::Map;

    #[test]
    fn home_port_reads_the_run_file_and_refuses_anything_else() {
        assert_eq!(home_port("pid: 4\nport: 41234\nurl: http://127.0.0.1:41234/\n"), Some(41234));
        for bad in [
            "",
            "port: 80",
            "port: 0\nurl: http://127.0.0.1:0/",
            "port: 70000\nurl: http://127.0.0.1:70000/",
            "port: 41234\nurl: http://127.0.0.1:9999/",
            "port: 41234\nurl: http://example.com:41234/",
            "- not\n- a map",
            "port: [",
        ] {
            assert_eq!(home_port(bad), None, "{bad:?}");
        }
        assert_eq!(origins(41234), ["http://127.0.0.1:41234", "http://localhost:41234"]);
    }

    /// A host with a `web` agent whose record is `meta`, and a home dir with a run file `run`.
    async fn host(name: &str, web_meta: Value, run: Option<&str>) -> (Host, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("fc-home-origin-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("app")).unwrap();
        if let Some(run) = run {
            std::fs::write(dir.join("app").join("home.yaml"), run).unwrap();
        }
        let mut kernel = Kernel::new();
        kernel.bundles.register(fantastic_core::HANDLER_MODULE, fantastic_core::KernelStateBundle::default());
        let kernel = Arc::new(kernel);
        let root = Agent::new(
            AgentId::from("kernel"),
            Some(fantastic_core::HANDLER_MODULE.to_string()),
            None,
            Map::new(),
            dir.join(".fantastic"),
            false,
        );
        kernel.register(Arc::clone(&root));
        kernel.set_root(root);
        let h = Host { kernel, cfg: ClientCfg::here(), root: "kernel".to_string() };
        if !web_meta.is_null() {
            let mut payload = web_meta.as_object().unwrap().clone();
            payload.extend([
                ("type".into(), json!("create_agent")),
                ("parent_id".into(), json!("kernel")),
                ("id".into(), json!("web")),
                ("handler_module".into(), json!(fantastic_core::HANDLER_MODULE)),
            ]);
            let r = h.operator("kernel", Value::Object(payload)).await.result;
            assert!(r.get("error").is_none(), "{r}");
        }
        (h, dir)
    }

    fn allowed(h: &Host) -> Value {
        h.kernel.agents.get(&AgentId::from("web")).unwrap().value().record().meta.get("allowed_origins").cloned().unwrap_or(Value::Null)
    }

    const RUN: &str = "pid: 7\nport: 41234\nurl: http://127.0.0.1:41234/\n";

    #[tokio::test(flavor = "multi_thread")]
    async fn the_homes_origin_is_added_to_the_web_record_once_and_a_new_port_adds_more() {
        let (h, dir) = host("add", json!({"allowed_origins": ["https://ui.example"]}), Some(RUN)).await;
        assert!(sync(&h, &dir).await.unwrap(), "written");
        assert_eq!(
            allowed(&h),
            json!(["https://ui.example", "http://127.0.0.1:41234", "http://localhost:41234"])
        );
        assert!(!sync(&h, &dir).await.unwrap(), "nothing to add the second time");
        // the app came back on another port: its origins are added, the others kept
        std::fs::write(dir.join("app").join("home.yaml"), "pid: 8\nport: 41300\nurl: http://127.0.0.1:41300/\n").unwrap();
        assert!(sync(&h, &dir).await.unwrap());
        assert_eq!(allowed(&h).as_array().unwrap().len(), 5);
        assert!(allowed(&h).as_array().unwrap().contains(&json!("http://localhost:41300")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bare_string_record_is_kept_and_no_run_file_or_no_web_changes_nothing() {
        let (h, dir) = host("bare", json!({"allowed_origins": "https://ui.example"}), Some(RUN)).await;
        assert!(sync(&h, &dir).await.unwrap());
        assert_eq!(allowed(&h)[0], "https://ui.example");
        assert_eq!(allowed(&h).as_array().unwrap().len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
        // no allowed_origins yet: the list is the Home's two
        let (h, dir) = host("none", json!({}), Some(RUN)).await;
        assert!(sync(&h, &dir).await.unwrap());
        assert_eq!(allowed(&h), json!(["http://127.0.0.1:41234", "http://localhost:41234"]));
        let _ = std::fs::remove_dir_all(&dir);
        // the app is not running: no file
        let (h, dir) = host("nofile", json!({}), None).await;
        assert!(!sync(&h, &dir).await.unwrap());
        assert_eq!(allowed(&h), Value::Null);
        let _ = std::fs::remove_dir_all(&dir);
        // a garbled file
        let (h, dir) = host("garbled", json!({}), Some("port: nope")).await;
        assert!(!sync(&h, &dir).await.unwrap());
        let _ = std::fs::remove_dir_all(&dir);
        // no web host here (not a canvas): nothing to write
        let (h, dir) = host("noweb", Value::Null, Some(RUN)).await;
        assert!(!sync(&h, &dir).await.unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
