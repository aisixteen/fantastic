//! THE SYSTEM AGENTS (`fantastic [canvas] system`, Linux): the machine's basics
//! as agents under `user` — `power`, `network`, `audio`, `displays`, `notifications` and
//! `lock` (`fantastic-system-agents`; `lock` is `user`'s alone, `kern/reach`). Made
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
    use fantastic_system::{audio, displays, lock, network, notifications, power};
    use fantastic_system_agents::sys::{AudioOs, DisplaysOs, LockOs, NetworkOs, NotificationsOs, OsError, PowerOs};
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

    /// An audio port with nothing behind it.
    struct NothingAudio;

    #[async_trait::async_trait]
    impl AudioOs for NothingAudio {
        async fn read(&self) -> Result<Value, OsError> {
            Err(OsError::Unavailable("wpctl is not installed".into()))
        }
        async fn call(&self, _c: audio::Call) -> Result<(), OsError> {
            Err(OsError::Unavailable("no".into()))
        }
        fn changes(&self) -> tokio::sync::mpsc::Receiver<audio::Event> {
            tokio::sync::mpsc::channel(1).1
        }
    }

    /// A network port with nothing behind it.
    struct NothingNetwork;

    #[async_trait::async_trait]
    impl NetworkOs for NothingNetwork {
        async fn read(&self) -> Result<network::Read, OsError> {
            Err(OsError::Unavailable("org.freedesktop.NetworkManager is not on the system bus".into()))
        }
        async fn call(&self, _c: network::Call) -> Result<(), OsError> {
            Err(OsError::Unavailable("no".into()))
        }
        fn changes(&self) -> tokio::sync::mpsc::Receiver<network::Event> {
            tokio::sync::mpsc::channel(1).1
        }
    }

    /// A notifications port with nothing behind it.
    struct NothingNotifications;

    #[async_trait::async_trait]
    impl NotificationsOs for NothingNotifications {
        async fn request(&self) -> Result<notifications::Ownership, OsError> {
            Err(OsError::Unavailable("the session bus is not reachable".into()))
        }
        async fn call(&self, _c: notifications::Call) -> Result<(), OsError> {
            Err(OsError::Unavailable("no".into()))
        }
        fn changes(&self) -> tokio::sync::mpsc::Receiver<notifications::Event> {
            tokio::sync::mpsc::channel(1).1
        }
    }

    /// A displays port with nothing behind it.
    struct NothingDisplays;

    #[async_trait::async_trait]
    impl DisplaysOs for NothingDisplays {
        async fn read(&self) -> Result<displays::Read, OsError> {
            Err(OsError::Unavailable("wlr-randr is not installed".into()))
        }
        async fn call(&self, _c: displays::Call) -> Result<(), OsError> {
            Err(OsError::Unavailable("no".into()))
        }
        fn changes(&self) -> tokio::sync::mpsc::Receiver<displays::Event> {
            tokio::sync::mpsc::channel(1).1
        }
    }

    /// A lock port with nothing behind it.
    struct NothingLock;

    #[async_trait::async_trait]
    impl LockOs for NothingLock {
        async fn read(&self) -> Result<lock::Read, OsError> {
            Err(OsError::Unavailable("org.freedesktop.login1 is not on the system bus".into()))
        }
        async fn check_pin(&self, _pin: lock::Pin) -> Result<bool, OsError> {
            Err(OsError::Unavailable("no".into()))
        }
        async fn call(&self, _c: lock::Call) -> Result<(), OsError> {
            Err(OsError::Unavailable("no".into()))
        }
        fn changes(&self) -> tokio::sync::mpsc::Receiver<lock::Event> {
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
        kernel.bundles.register(
            fantastic_system_agents::audio::HANDLER_MODULE,
            fantastic_system_agents::audio::AudioBundle::with_port(Arc::new(NothingAudio)),
        );
        kernel.bundles.register(
            fantastic_system_agents::network::HANDLER_MODULE,
            fantastic_system_agents::network::NetworkBundle::with_port(Arc::new(NothingNetwork)),
        );
        kernel.bundles.register(
            fantastic_system_agents::notifications::HANDLER_MODULE,
            fantastic_system_agents::notifications::NotificationsBundle::with_port(Arc::new(NothingNotifications)),
        );
        kernel.bundles.register(
            fantastic_system_agents::displays::HANDLER_MODULE,
            fantastic_system_agents::displays::DisplaysBundle::with_port(Arc::new(NothingDisplays)),
        );
        kernel.bundles.register(
            fantastic_system_agents::lock::HANDLER_MODULE,
            fantastic_system_agents::lock::LockBundle::with_port(Arc::new(NothingLock)),
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

    #[tokio::test(flavor = "multi_thread")]
    async fn ensure_makes_audio_under_user_with_its_view_and_one_instance() {
        assert_eq!(
            ids(),
            ["power", "network", "audio", "displays", "notifications", "lock"],
            "the tiles the presets list"
        );
        let dir = std::env::temp_dir().join(format!("fc-system-ensure-audio-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let h = host(&dir).await;
        ensure(&h).await.unwrap();
        let audio = h.kernel.agents.get(&AgentId::from("audio")).expect("audio exists").value().record();
        assert_eq!(audio.parent_id.as_deref(), Some(h.cfg.user.as_str()));
        assert_eq!(audio.handler_module.as_deref(), Some("audio.tools"));
        let view = meta(&h, "audio", "view").unwrap();
        assert_eq!(view["kind"], "html");
        assert!(view["bytes"].as_str().unwrap().contains("set_volume"), "the page's html");
        assert!(view["calls"].as_array().unwrap().iter().any(|v| v == "set_volume"));
        let r = h
            .operator("kernel", json!({"type": "update_agent", "id": "audio", "view": {"kind": "html", "bytes": "old", "calls": []}}))
            .await
            .result;
        assert!(r.get("error").is_none(), "{r}");
        ensure(&h).await.unwrap();
        assert_ne!(meta(&h, "audio", "view").unwrap()["bytes"], "old", "the view is reset");
        assert_eq!(
            h.kernel.agents.iter().filter(|a| a.value().record().handler_module.as_deref() == Some("audio.tools")).count(),
            1,
            "one audio agent, not two"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ensure_makes_all_six_agents_each_with_its_view_and_one_instance() {
        let dir = std::env::temp_dir().join(format!("fc-system-ensure-six-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let h = host(&dir).await;
        ensure(&h).await.unwrap();
        ensure(&h).await.unwrap();
        assert_eq!(fantastic_system_agents::IDS.len(), 6);
        for (id, module) in fantastic_system_agents::IDS.iter().zip(fantastic_system_agents::HANDLER_MODULES) {
            let rec = h.kernel.agents.get(&AgentId::from(*id)).unwrap_or_else(|| panic!("{id} exists")).value().record();
            assert_eq!(rec.parent_id.as_deref(), Some(h.cfg.user.as_str()), "{id} is under user");
            assert_eq!(rec.handler_module.as_deref(), Some(module), "{id}");
            let view = meta(&h, id, "view").unwrap_or_else(|| panic!("{id} has a view"));
            assert_eq!(view["kind"], "html", "{id}");
            assert!(view["bytes"].as_str().unwrap().contains("fantastic.watch"), "{id}: the page's html");
            assert_eq!(view["calls"], json!(fantastic_system_agents::verbs(id)), "{id}: the view may call its verbs");
            assert_eq!(meta(&h, id, "callable_by").unwrap()[h.cfg.user.as_str()], json!(fantastic_system_agents::verbs(id)), "{id}: the user's grant");
            assert_eq!(meta(&h, id, "display_name"), Some(json!(id)));
            assert_eq!(
                h.kernel.agents.iter().filter(|a| a.value().record().handler_module.as_deref() == Some(module)).count(),
                1,
                "one {id}, not two"
            );
        }
        // each view draws state and sends intents: it names no host call of its own
        for id in fantastic_system_agents::IDS {
            let html = fantastic_system_agents::view(id).unwrap();
            assert!(html.contains("fantastic.watch(ID"), "{id} watches its state");
            assert!(html.contains("fantastic.send(ID"), "{id} sends to itself only");
            assert!(!html.contains("fantastic.send(\"") && !html.contains("fantastic.send('"), "{id} sends only to its own id");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
