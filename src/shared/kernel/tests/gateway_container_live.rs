//! Live container gateway end-to-end: spawn a SOVEREIGN `fantastic_kernel` as a
//! podman/docker container in a temp workspace, drive it over loopback HTTP,
//! then ALWAYS stop+remove the container (even if an assertion fails). This
//! needs a container engine + a built `fantastic:arm64` image, so it is:
//!   * `#[ignore]` by default — `cargo test` stays fast/offline; and
//!   * self-skipping — no engine OR no image → print SKIP and return Ok (never
//!     fails) even when run with `--ignored`.
//!
//! Run it explicitly (build the image first):
//!   sh container/build.sh
//!   cargo test -p fantastic-host --test gateway_container_live -- --ignored

use std::process::Command;

use fantastic_host::gateway::{stop_container, Workspace};
use fantastic_host::Runtime;
use serde_json::{json, Value};

/// Resolve an engine path the same way the gateway does, for the skip-probe.
fn engine() -> Option<(String, bool)> {
    if let Ok(e) = std::env::var("FANTASTIC_CONTAINER_ENGINE") {
        if !e.is_empty() {
            let is_podman = !e.to_lowercase().contains("docker");
            return Some((e, is_podman));
        }
    }
    for name in ["podman", "docker"] {
        if Command::new(name)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            return Some((name.to_string(), name == "podman"));
        }
    }
    None
}

fn image() -> String {
    std::env::var("FANTASTIC_IMAGE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "fantastic:arm64".to_string())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "spawns a real container; needs a container engine + the fantastic:arm64 image"]
async fn container_spawns_drives_and_cleans_up() -> anyhow::Result<()> {
    let Some((eng, _)) = engine() else {
        eprintln!("SKIP: no engine/image");
        return Ok(());
    };
    let img = image();
    let present = Command::new(&eng)
        .args(["image", "inspect", &img])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !present {
        eprintln!("SKIP: no engine/image");
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let ws = Workspace {
        dir: dir.path().to_path_buf(),
    };

    let (handle, name) = ws.spawn_container(Runtime::Rust).await?;

    // Drop guard: stop+remove the container on EVERY exit path — including a
    // panicking `assert!`/`expect` below (which unwinds, dropping locals). No
    // container is ever left behind.
    struct Cleanup {
        engine: String,
        name: String,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            stop_container(&self.engine, &self.name);
        }
    }
    let _guard = Cleanup {
        engine: eng,
        name: name.clone(),
    };

    let reply = handle.send("kernel", json!({"type":"list_agents"})).await?;
    let agents = reply
        .get("agents")
        .and_then(Value::as_array)
        .expect("list_agents returns an `agents` array");
    let ids: Vec<&str> = agents
        .iter()
        .filter_map(|a| a.get("id").and_then(Value::as_str))
        .collect();
    assert!(ids.contains(&"core"), "live agents include `core`: {ids:?}");
    assert!(
        ids.iter().any(|id| id.starts_with("web")),
        "live agents include a `web` surface: {ids:?}"
    );

    Ok(())
}
