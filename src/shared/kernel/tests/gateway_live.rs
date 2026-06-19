//! Live gateway end-to-end: spawn a SOVEREIGN `fantastic_kernel` daemon in a
//! temp workspace, drive it over loopback HTTP, then shut it down. This needs a
//! built kernel binary, so it is:
//!   * `#[ignore]` by default — `cargo test` stays fast/offline; and
//!   * self-skipping — if no binary resolves it `eprintln!`s a SKIP and returns
//!     (never fails) even when run with `--ignored`.
//!
//! Run it explicitly (build the kernel first if needed):
//!   cargo build --release -p fantastic-cli   # in src/lib/rust
//!   cargo test -p fantastic-host --test gateway_live -- --ignored

use fantastic_host::gateway::{resolve_kernel_bin, Workspace};
use fantastic_host::Runtime;
use serde_json::{json, Value};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "spawns a real kernel daemon; needs a built fantastic_kernel binary"]
async fn gateway_spawns_drives_and_shuts_down() -> anyhow::Result<()> {
    // CI-safe skip: no binary → print SKIP and return Ok. `FANTASTIC_KERNEL_BIN`
    // is honored by `resolve_kernel_bin`, so an explicit override also works.
    if resolve_kernel_bin(Runtime::Rust).is_none() {
        eprintln!("SKIP: kernel binary not built (set FANTASTIC_KERNEL_BIN or build src/lib/rust)");
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let ws = Workspace {
        dir: dir.path().to_path_buf(),
    };

    let handle = ws.spawn(Runtime::Rust).await?;

    // Drive the live REST surface: list_agents must include the root + web.
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

    // Clean up: ask the kernel to shut itself down (best-effort — the daemon may
    // drop the connection as it exits, which is fine).
    let _ = handle.send("core", json!({"type":"shutdown_kernel"})).await;

    Ok(())
}
