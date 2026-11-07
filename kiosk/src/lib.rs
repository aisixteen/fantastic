//! fantastic OS, the kiosk, as a library: the fantastic base ([`fantastic_tui`]) plus the
//! machine's agents, built from the base's public crates only.
//!
//! - [`register`] is the base's `Extra`: it adds the six system agents' bundles
//!   (`power.tools`, `network.tools`, `audio.tools`, `displays.tools`,
//!   `notifications.tools`, `lock.tools`) to the registry, after the stock set.
//! - [`ensure`] makes each agent exist under `user` with its view and its grant,
//!   re-set on every boot so a new build's view replaces the old one. A view is
//!   inline in the record (`view: {kind: html, bytes, calls}`), no bundle file.
//!
//! The OS connections are the bundles': made once per process on first use, and an
//! agent whose service is missing says so (`available: false`), it does not fail
//! the boot.

use std::sync::Arc;

use fantastic_kernel::{AgentId, Item, Kernel};
use serde_json::{json, Value};

pub use fantastic_system_agents::{HANDLER_MODULES, IDS};

/// The id of the human's own agent (`user`): the parent of every system agent and
/// the one holder of their grants. The base's own constant for it is private.
pub const USER: &str = "user";

/// The base's `Extra` for the kiosk binary: the system agents' bundles, on the
/// machine's real OS ports (Linux only; elsewhere they answer `available: false`).
pub fn register(reg: &mut fantastic_kernel::BundleRegistry) {
    fantastic_system_agents::register(reg);
}

/// Make each system agent exist under `user`, its view and the human's grant re-set.
pub async fn ensure(kernel: &Arc<Kernel>) -> Result<(), String> {
    for (id, module) in IDS.iter().zip(HANDLER_MODULES) {
        let verbs = fantastic_system_agents::verbs(id);
        let view = json!({"kind": "html", "bytes": fantastic_system_agents::view(id).unwrap_or(""), "calls": verbs});
        let grant = json!({ USER: verbs });
        let payload = if kernel.agents.contains_key(&AgentId::from(*id)) {
            json!({"type": "update_agent", "id": id, "view": view, "callable_by": grant, "display_name": id})
        } else {
            json!({"type": "create_agent", "parent_id": USER, "id": id, "handler_module": module,
                   "view": view, "callable_by": grant, "display_name": id})
        };
        let r = act(kernel, "kernel", payload).await;
        if let Some(e) = r.get("error").and_then(Value::as_str) {
            return Err(format!("could not make `{id}`: {e}"));
        }
    }
    Ok(())
}

/// One call to `target`, made by the human (`user` forwards it, so the ledger names a person).
async fn act(kernel: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    let item = Item::new(
        AgentId::from(USER),
        json!({"type": "forward", "target": target, "payload": payload}),
    );
    kernel.send(vec![item]).await.remove(0).result
}

#[cfg(test)]
mod tests {
    use super::*;
    use fantastic_kernel::Agent;
    use fantastic_system::{audio, displays, lock, network, notifications, power};
    use fantastic_system_agents::sys::{
        AudioOs, DisplaysOs, LockOs, NetworkOs, NotificationsOs, OsError, PowerOs,
    };
    use serde_json::Map;

    /// A port with nothing behind it.
    struct Nothing;

    #[async_trait::async_trait]
    impl PowerOs for Nothing {
        async fn read(&self) -> Result<power::Read, OsError> {
            Err(OsError::Unavailable(
                "org.freedesktop.UPower is not on the system bus".into(),
            ))
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
            Err(OsError::Unavailable(
                "org.freedesktop.NetworkManager is not on the system bus".into(),
            ))
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
            Err(OsError::Unavailable(
                "the session bus is not reachable".into(),
            ))
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
            Err(OsError::Unavailable(
                "org.freedesktop.login1 is not on the system bus".into(),
            ))
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

    /// A kernel with the base's own two bundles and the six agents' bundles on ports with nothing behind them.
    async fn kernel(dir: &std::path::Path) -> Arc<Kernel> {
        let mut kernel = Kernel::new();
        kernel.bundles.register(
            fantastic_core::HANDLER_MODULE,
            fantastic_core::KernelStateBundle::default(),
        );
        kernel.bundles.register(
            fantastic_user::HANDLER_MODULE,
            fantastic_user::UserBundle::default(),
        );
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
            fantastic_system_agents::notifications::NotificationsBundle::with_port(Arc::new(
                NothingNotifications,
            )),
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
        let r = operator(
            &kernel,
            "kernel",
            json!({"type": "create_agent", "parent_id": "kernel", "id": USER,
                   "handler_module": fantastic_user::HANDLER_MODULE, "can_call": {"kernel": "*"}}),
        )
        .await;
        assert!(r.get("error").is_none(), "{r}");
        // what the base's reach gives the human on the kernel
        let r = operator(
            &kernel,
            "kernel",
            json!({"type": "update_agent", "id": "kernel", "callable_by": {USER: "*"}}),
        )
        .await;
        assert!(r.get("error").is_none(), "{r}");
        kernel
    }

    /// A plain send from outside every handler: the operator's, ungated.
    async fn operator(kernel: &Arc<Kernel>, target: &str, payload: Value) -> Value {
        kernel
            .send(vec![Item::new(AgentId::from(target), payload)])
            .await
            .remove(0)
            .result
    }

    fn meta(k: &Kernel, id: &str, key: &str) -> Option<Value> {
        k.agents
            .get(&AgentId::from(id))
            .and_then(|a| a.value().record().meta.get(key).cloned())
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("fantastic-kiosk-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ensure_makes_all_six_agents_each_with_its_view_and_one_instance() {
        let dir = scratch("six");
        let k = kernel(&dir).await;
        ensure(&k).await.unwrap();
        ensure(&k).await.unwrap();
        assert_eq!(
            IDS,
            [
                "power",
                "network",
                "audio",
                "displays",
                "notifications",
                "lock"
            ]
        );
        for (id, module) in IDS.iter().zip(HANDLER_MODULES) {
            let rec = k
                .agents
                .get(&AgentId::from(*id))
                .unwrap_or_else(|| panic!("{id} exists"))
                .value()
                .record();
            assert_eq!(rec.parent_id.as_deref(), Some(USER), "{id} is under user");
            assert_eq!(rec.handler_module.as_deref(), Some(module), "{id}");
            let view = meta(&k, id, "view").unwrap_or_else(|| panic!("{id} has a view"));
            assert_eq!(view["kind"], "html", "{id}");
            assert!(
                view["bytes"].as_str().unwrap().contains("fantastic.watch"),
                "{id}: the page's html, inline in the record"
            );
            assert!(view.get("bytes_file").is_none(), "{id}: no bundle file");
            assert_eq!(
                view["calls"],
                json!(fantastic_system_agents::verbs(id)),
                "{id}: the view may call its verbs"
            );
            assert_eq!(
                meta(&k, id, "callable_by").unwrap()[USER],
                json!(fantastic_system_agents::verbs(id)),
                "{id}: the user's grant"
            );
            assert_eq!(meta(&k, id, "display_name"), Some(json!(id)));
            assert_eq!(
                k.agents
                    .iter()
                    .filter(|a| a.value().record().handler_module.as_deref() == Some(module))
                    .count(),
                1,
                "one {id}, not two"
            );
        }
        // each view draws state and sends intents: it names no host call of its own
        for id in IDS {
            let html = fantastic_system_agents::view(id).unwrap();
            assert!(
                html.contains("fantastic.watch(ID"),
                "{id} watches its state"
            );
            assert!(
                html.contains("fantastic.send(ID"),
                "{id} sends to itself only"
            );
            assert!(
                !html.contains("fantastic.send(\"") && !html.contains("fantastic.send('"),
                "{id} sends only to its own id"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stale_view_is_reset_on_the_next_boot() {
        let dir = scratch("stale");
        let k = kernel(&dir).await;
        ensure(&k).await.unwrap();
        let r = operator(&k, "kernel", json!({"type": "update_agent", "id": "power", "view": {"kind": "html", "bytes": "old", "calls": []}})).await;
        assert!(r.get("error").is_none(), "{r}");
        assert_eq!(meta(&k, "power", "view").unwrap()["bytes"], "old");
        ensure(&k).await.unwrap();
        assert_ne!(
            meta(&k, "power", "view").unwrap()["bytes"],
            "old",
            "the view is reset"
        );
        assert!(meta(&k, "power", "view").unwrap()["calls"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "suspend"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn register_adds_the_six_bundles_to_a_registry() {
        let mut reg = fantastic_kernel::BundleRegistry::new();
        for module in HANDLER_MODULES {
            assert!(reg.get(module).is_none(), "{module}: not there before");
        }
        register(&mut reg);
        for module in HANDLER_MODULES {
            assert!(
                reg.get(module).is_some(),
                "{module}: registered by the kiosk"
            );
        }
    }
}
