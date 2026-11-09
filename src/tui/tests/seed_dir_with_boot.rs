//! An embedder's blueprint, seeded once, hydrates under exactly the written ids
//! with their parents read off the disk position (issue #480).
//!
//! Its own integration binary: the bundles clamp against the process cwd, and
//! chdir is process-global (see `inproc.rs`).

use fantastic_kernel::{
    bootstrap::{self, BootstrapOptions},
    BundleRegistry,
};

const EMBEDDER: &str = "\
agents:
  - id: probe
    handler_module: ram_store.tools
  - id: user
    agents:
      - id: probe2
        handler_module: ram_store.tools
";

#[tokio::test(flavor = "multi_thread")]
async fn embedder_agents_hydrate_under_their_written_ids_and_disk_parents() {
    let dir = std::env::temp_dir().join(format!("fc-seed-with-boot-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    assert_eq!(fantastic_tui::seed_dir_with(&dir, &[EMBEDDER]), Ok(true));
    std::env::set_current_dir(&dir).unwrap();

    let mut reg = BundleRegistry::new();
    reg.register(fantastic_core::HANDLER_MODULE, fantastic_core::KernelStateBundle::default());
    reg.register("file_bridge.tools", fantastic_file::FileBundle::default());
    reg.register(fantastic_ram_store::HANDLER_MODULE, fantastic_ram_store::RamStoreBundle::default());
    reg.register(fantastic_user::HANDLER_MODULE, fantastic_user::UserBundle::default());

    let booted = bootstrap::bootstrap(reg, BootstrapOptions::daemon(dir.join(".fantastic"), &dir))
        .expect("boots over the seeded dir");
    let kernel = std::sync::Arc::clone(&booted.kernel);
    let parent_of = |id: &str| {
        kernel
            .agents
            .get(&fantastic_kernel::AgentId::from(id))
            .unwrap_or_else(|| panic!("`{id}` hydrated under its written id"))
            .parent_id
            .as_ref()
            .map(|p| p.0.clone())
    };
    assert_eq!(parent_of("probe"), Some(bootstrap::DEFAULT_ROOT_ID.to_string()));
    assert_eq!(parent_of("probe2"), Some("user".to_string()));
    assert_eq!(parent_of("user"), Some(bootstrap::DEFAULT_ROOT_ID.to_string()));

    let _ = bootstrap::shutdown(&dir.join(".fantastic"));
    std::env::set_current_dir(std::env::temp_dir()).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
