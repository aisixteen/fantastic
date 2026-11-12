//! The public in-process boot (issue #580): `seed_dir_with`, `boot_kernel`, `stop_kernel`.
//!
//! Its own integration binary and ONE test: the bundles clamp against the process cwd and
//! the user home is an env var, both process-global (see `seed_dir_with_boot.rs`).

use std::path::PathBuf;
use std::sync::Arc;

use fantastic_kernel::{AgentId, BundleRegistry, Kernel};
use fantastic_tui::{boot_kernel, seed_dir_with, stop_kernel, Extra};
use serde_json::{json, Value};

const PROBE: &str = "boot_probe.tools";

struct Probe;

#[async_trait::async_trait]
impl fantastic_kernel::bundle::Handler for Probe {
    type Call = fantastic_kernel::test_support::NoCall;

    fn name(&self) -> &str {
        "boot_probe"
    }

    fn man(&self) -> &'static str {
        "A test stand-in an embedder registers."
    }

    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        fantastic_kernel::test_support::no_verbs()
    }

    fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
        fantastic_kernel::test_support::no_dynamic(agent)
    }

    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        fantastic_kernel::fixture_record!()
    }

    async fn handle(
        &self,
        _id: &AgentId,
        _call: Self::Call,
        _kernel: &Arc<Kernel>,
    ) -> Result<Option<Value>, fantastic_kernel::bundle::BundleError> {
        Ok(Some(json!({"probe": "pong"})))
    }
}

fn probe_extra(reg: &mut BundleRegistry) {
    reg.register(PROBE, Probe);
}

const EXTRAS: &[Extra] = &[probe_extra];

fn folder(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fc-boot-kernel-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn seeded(name: &str, blueprint: &str) -> PathBuf {
    let d = folder(name);
    assert_eq!(seed_dir_with(&d.join(".fantastic"), &[blueprint]), Ok(true));
    d
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    l.local_addr().unwrap().port()
}

async fn accepts(port: u16) -> bool {
    for _ in 0..50 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    false
}

/// The port the running kernel's `web` record holds.
fn recorded_port(k: &Arc<Kernel>) -> Option<u64> {
    k.agents.get(&AgentId::from("web"))?.value().record().meta.get("port").and_then(Value::as_u64)
}

const WEB_NO_PORT: &str = "agents:\n  - id: web\n    handler_module: web.tools\n";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boot_kernel_boots_and_stops_a_folder_in_process() {
    let home = folder("home");
    std::env::set_var("FANTASTIC_HOME", &home);

    // 1. a seeded record naming an extra bundle is a live agent
    let a = seeded("a", "agents:\n  - id: probe\n    handler_module: boot_probe.tools\n");
    std::env::set_current_dir(&a).unwrap();
    let k = boot_kernel(&a.join(".fantastic"), &a, EXTRAS, None, None).await.expect("boots");
    assert!(k.agents.contains_key(&AgentId::from("probe")), "the extra's record is a live agent");

    // 4. a second boot of the same folder while the first runs is refused with the lock error
    let again = boot_kernel(&a.join(".fantastic"), &a, EXTRAS, None, None).await.err().expect("refused");
    assert!(again.to_lowercase().contains("lock"), "{again}");

    // 3. stop releases the lock; boot, stop, boot again works
    stop_kernel(&k).await;
    let k = boot_kernel(&a.join(".fantastic"), &a, EXTRAS, None, None).await.expect("boots again");
    stop_kernel(&k).await;

    // 2. a recorded `web` given a port listens on it; stop releases the port
    let b = seeded("b", WEB_NO_PORT);
    std::env::set_current_dir(&b).unwrap();
    let port = free_port();
    let k = boot_kernel(&b.join(".fantastic"), &b, &[], Some(port), None).await.expect("boots");
    assert!(accepts(port).await, "the web host listens on the given port");
    stop_kernel(&k).await;
    assert!(std::net::TcpListener::bind(("127.0.0.1", port)).is_ok(), "stop released the port");

    // 5. a seeded port-less `web`, no port given: it listens, the record holds the port, a
    // second boot reuses it
    let c = seeded("c", WEB_NO_PORT);
    std::env::set_current_dir(&c).unwrap();
    let k = boot_kernel(&c.join(".fantastic"), &c, &[], None, None).await.expect("boots");
    let picked = recorded_port(&k).expect("the record now holds a port");
    assert!(accepts(picked as u16).await, "it listens on the picked port");
    stop_kernel(&k).await;
    let k = boot_kernel(&c.join(".fantastic"), &c, &[], None, None).await.expect("boots again");
    assert_eq!(recorded_port(&k), Some(picked), "a later run reuses it");
    assert!(accepts(picked as u16).await);
    stop_kernel(&k).await;

    // 6. a port given and no `web` record: refused, and the folder boots again right after
    let d = seeded("d", "agents:\n  - id: probe\n    handler_module: boot_probe.tools\n");
    std::env::set_current_dir(&d).unwrap();
    let refused = boot_kernel(&d.join(".fantastic"), &d, EXTRAS, Some(free_port()), None).await.err().expect("refused");
    assert!(refused.contains("no `web` record"), "{refused}");
    let k = boot_kernel(&d.join(".fantastic"), &d, EXTRAS, None, None).await.expect("the lock was released");
    stop_kernel(&k).await;

    std::env::set_current_dir(std::env::temp_dir()).unwrap();
    for p in [&a, &b, &c, &d, &home] {
        let _ = std::fs::remove_dir_all(p);
    }
}
