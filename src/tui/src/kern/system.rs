//! THE SYSTEM AGENTS (`fantastic [canvas] system`, Linux): the machine's basics
//! as agents under `user` — `power` so far (`fantastic-system-agents`). Made
//! when the recipe is named, each with its own html view, re-set on every such
//! boot so a new build's view replaces the old one. The OS connections are the
//! bundle's: made once per process on first use (`sys/linux`), and an agent
//! whose service is missing says so (`available: false`), it does not fail the
//! boot.

use serde_json::{json, Value};

use crate::kern::host::Host;

/// The agents' ids.
pub fn ids() -> &'static [&'static str] {
    &fantastic_system_agents::IDS
}

/// Make each system agent exist under `user`, its view and the human's grant re-set.
pub async fn ensure(host: &Host) -> Result<(), String> {
    let user = host.cfg.user.clone();
    for (id, module) in fantastic_system_agents::IDS.iter().zip(fantastic_system_agents::HANDLER_MODULES) {
        let verbs = fantastic_system_agents::verbs(id);
        let view = json!({"kind": "html", "bytes": fantastic_system_agents::view(id).unwrap_or(""), "calls": verbs});
        let grant = json!({ user.as_str(): verbs });
        let payload = if host.kernel.agents.contains_key(&fantastic_kernel::AgentId::from(*id)) {
            json!({"type": "update_agent", "id": id, "view": view, "callable_by": grant, "display_name": id})
        } else {
            json!({"type": "create_agent", "parent_id": user.as_str(), "id": id, "handler_module": module,
                   "view": view, "callable_by": grant, "display_name": id})
        };
        let r = host.act("kernel", payload).await.result;
        if let Some(e) = r.get("error").and_then(Value::as_str) {
            return Err(format!("could not make `{id}`: {e}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::ClientCfg;
    use fantastic_kernel::{Agent, AgentId, Kernel};
    use fantastic_system::power;
    use fantastic_system_agents::sys::{OsError, PowerOs};
    use serde_json::Map;
    use std::sync::Arc;

    /// A port with nothing behind it.
    struct Nothing;

    #[async_trait::async_trait]
    impl PowerOs for Nothing {
        async fn read(&self) -> Result<power::Read, OsError> {
            Err(OsError::Unavailable("org.freedesktop.UPower is not on the system bus".into()))
        }
        async fn call(&self, _c: power::Call) -> Result<(), OsError> {
            Err(OsError::Unavailable("no".into()))
        }
        fn changes(&self) -> tokio::sync::mpsc::Receiver<power::Event> {
            tokio::sync::mpsc::channel(1).1
        }
    }

    async fn host(dir: &std::path::Path) -> Host {
        let mut kernel = Kernel::new();
        kernel.bundles.register(fantastic_core::HANDLER_MODULE, fantastic_core::KernelStateBundle::default());
        kernel.bundles.register(fantastic_user::HANDLER_MODULE, fantastic_user::UserBundle::default());
        kernel.bundles.register(
            fantastic_system_agents::power::HANDLER_MODULE,
            fantastic_system_agents::power::PowerBundle::with_port(Arc::new(Nothing)),
        );
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
        let r = h
            .operator(
                "kernel",
                json!({"type": "create_agent", "parent_id": "kernel", "id": h.cfg.user.as_str(),
                       "handler_module": fantastic_user::HANDLER_MODULE, "can_call": {"kernel": "*"}}),
            )
            .await
            .result;
        assert!(r.get("error").is_none(), "{r}");
        // what `reach::grant_all` gives the human on the kernel
        let r = h.operator("kernel", json!({"type": "update_agent", "id": "kernel", "callable_by": {h.cfg.user.as_str(): "*"}})).await.result;
        assert!(r.get("error").is_none(), "{r}");
        h
    }

    fn meta(h: &Host, id: &str, key: &str) -> Option<Value> {
        h.kernel.agents.get(&AgentId::from(id)).and_then(|a| a.value().record().meta.get(key).cloned())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ensure_makes_power_under_user_and_resets_view_on_second_call() {
        let dir = std::env::temp_dir().join(format!("fc-system-ensure-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let h = host(&dir).await;
        ensure(&h).await.unwrap();
        let power = h.kernel.agents.get(&AgentId::from("power")).expect("power exists").value().record();
        assert_eq!(power.parent_id.as_deref(), Some(h.cfg.user.as_str()));
        assert_eq!(power.handler_module.as_deref(), Some("power.tools"));
        let view = meta(&h, "power", "view").unwrap();
        assert_eq!(view["kind"], "html");
        assert!(view["bytes"].as_str().unwrap().contains("fantastic.watch"), "the page's html");
        assert!(view["calls"].as_array().unwrap().iter().any(|v| v == "suspend"));
        assert_eq!(meta(&h, "power", "display_name"), Some(json!("power")));
        // a stale view and grant on disk are put right on the next boot
        let r = h
            .operator("kernel", json!({"type": "update_agent", "id": "power", "view": {"kind": "html", "bytes": "old", "calls": []}}))
            .await
            .result;
        assert!(r.get("error").is_none(), "{r}");
        assert_eq!(meta(&h, "power", "view").unwrap()["bytes"], "old");
        ensure(&h).await.unwrap();
        assert_ne!(meta(&h, "power", "view").unwrap()["bytes"], "old", "the view is reset");
        assert_eq!(
            h.kernel.agents.iter().filter(|a| a.value().record().handler_module.as_deref() == Some("power.tools")).count(),
            1,
            "one power agent, not two"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
