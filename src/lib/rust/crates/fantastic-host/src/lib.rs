//! The default bundle set of a fantastic host.
//!
//! A host is a kernel plus the bundles it can dispatch. `default_bundles()`
//! builds that set once, so every binary (the `fantastic` CLI, a shift host,
//! any future binary) registers the same bundles from one place.
//!
//! Tiers: the always-available bundles compile in every build. The
//! subprocess-spawning bundles (exec_bridge, py_agent) gate on the `full`
//! feature (default on); `embedded` builds without them. `scenario-probes`
//! compiles the test-only fixture agents the shared e2e scenarios name.

#![deny(missing_docs)]

use fantastic_kernel::BundleRegistry;

/// The default bundle set, registered under each bundle's `handler_module`.
pub fn default_bundles() -> BundleRegistry {
    let mut reg = BundleRegistry::new();

    // ── Always-available bundles (work under both `desktop` and `embedded`).
    //
    // None of these spawn subprocesses, fork, or load dynamic libraries,
    // so they all compile + run on iOS. The Bundle trait is the only
    // dependency the kernel cares about; everything below is pure-Rust
    // async over axum/tokio.
    // THE ROOT LOADER. `kernel_state.tools` is the root's own handler_module —
    // without it the tree root has a bundle that is not installed, so its boot
    // (the guid, the record writer) never runs.
    reg.register(
        fantastic_core::HANDLER_MODULE,
        fantastic_core::KernelStateBundle::default(),
    );
    reg.register("file_bridge.tools", fantastic_file::FileBundle::default());
    // THE MEMORY STORE in RAM — the root's `memory_store_id` and every AI agent's
    // name one. Ephemeral: gone with the process.
    reg.register(
        fantastic_ram_store::HANDLER_MODULE,
        fantastic_ram_store::RamStoreBundle::default(),
    );
    // SECRETS BY REF — every AI backend reads its key here (`credentials:
    // {store, ref}` on its record), before each request. The OS keychain is
    // reached only by a store whose record chooses `backend: keychain`.
    reg.register(
        fantastic_credential_store::HANDLER_MODULE,
        fantastic_credential_store::CredentialStoreBundle::default(),
    );
    // The render sink AI backends stream their reply to. Without it that
    // stream had nowhere to land on rust.
    reg.register(
        fantastic_cli_bundle::HANDLER_MODULE,
        fantastic_cli_bundle::CliBundle,
    );
    // TEST-ONLY. The shared e2e scenarios declare stateful fixture roles
    // (`dispenser`, `flaky_plant`, `purchase_desk`) that python and js register
    // in process. A compiled kernel can only run what is compiled in, and the
    // e2e driver reaches this binary over REST, so without these three the
    // scenarios naming them are SKIPPED. Behind a feature: a default build has
    // none of it. UNLISTED: python registers these from `e2e/probes` with
    // `register_unlisted` (no `fantastic.bundles` entry point), so they dispatch
    // but never appear in the `bundles` catalog.
    #[cfg(feature = "scenario-probes")]
    {
        use fantastic_scenario_probes as probes;
        reg.register_unlisted(probes::DISPENSER_MODULE, probes::DispenserProbe::default());
        reg.register_unlisted(probes::PLANT_MODULE, probes::PlantProbe::default());
        reg.register_unlisted(probes::PURCHASE_MODULE, probes::PurchaseProbe::default());
        // THE HUMAN, as an agent. Every AI probe tasks its model through this
        // one, so the task is a gated `user -> ai` send with a ledger row
        // instead of an operator injection from outside the kernel.
        reg.register_unlisted(probes::USER_MODULE, probes::UserProbe::default());
        reg.register_unlisted(probes::ORACLE_MODULE, probes::OracleProbe::default());
        // In-flight, failing and timed work: what a timeout, a stop or a
        // reboot lands on.
        reg.register_unlisted(probes::SLOW_MODULE, probes::SlowProbe::default());
        // The `docs_mechanics` vault: opens with a code only its detailed docs teach.
        reg.register_unlisted(probes::VAULT_MODULE, probes::VaultProbe::default());
    }
    reg.register("web.tools", fantastic_web::WebBundle);
    reg.register("web_ws.tools", fantastic_web_ws::WebWsBundle);
    reg.register("web_rest.tools", fantastic_web_rest::WebRestBundle);
    reg.register("scheduler.tools", fantastic_scheduler::SchedulerBundle);
    reg.register(
        fantastic_cabinet::HANDLER_MODULE,
        fantastic_cabinet::CabinetBundle,
    );
    reg.register(
        fantastic_static_loader::HANDLER_MODULE,
        fantastic_static_loader::StaticLoaderBundle,
    );
    reg.register(
        "ollama_backend.tools",
        fantastic_ollama_backend::OllamaBackendBundle,
    );
    // The bridge ships TWO derivations of io_bridge (mirrors py's separate
    // ws_bridge + relay_connector bundles): ws/ssh transports vs the relay-kernel router.
    reg.register("ws_bridge.tools", fantastic_bridge::WsBridgeBundle);
    reg.register(
        "relay_connector.tools",
        fantastic_bridge::RelayConnectorBundle,
    );
    reg.register(
        fantastic_responses_backend::HANDLER_MODULE,
        fantastic_responses_backend::ResponsesBundle,
    );
    reg.register(
        fantastic_vllm_backend::HANDLER_MODULE,
        fantastic_vllm_backend::VllmBundle,
    );
    reg.register(
        fantastic_anthropic_backend::HANDLER_MODULE,
        fantastic_anthropic_backend::AnthropicBundle,
    );
    // a remote MCP server (protocol 2026-07-28) as an agent: its tools are the
    // agent's verbs
    reg.register(
        fantastic_mcp_client::HANDLER_MODULE,
        fantastic_mcp_client::McpClientBundle::default(),
    );
    // Apple's on-device model — the swift FMCore through its C ABI on macOS.
    // Registered in both tiers and on every target, as swift's registry does:
    // where the model cannot run, a send fails naming why. Registering it calls
    // nothing; the model is asked only when an agent is used.
    reg.register(
        fantastic_foundation_models_backend::HANDLER_MODULE,
        fantastic_foundation_models_backend::FoundationModelsBackendBundle::system(),
    );
    // ── Full-tier-only bundles (subprocess / dynamic-loading / etc.).
    //
    // These spawn child processes (`std::process::Command` /
    // `tokio::process::Command`). The `embedded` build (iOS Lite,
    // visionOS, sandboxed macOS) compiles cleanly without them — the
    // `optional = true` dep + `#[cfg(feature = "full")]` gate enforces
    // that at compile time.
    #[cfg(feature = "full")]
    {
        // Via the exported const: the root reflect's `self_extension`
        // advertises executors by intersecting its table with THIS registry,
        // so a typo'd literal here would silently drop the one executor from
        // what a model is told it has.
        reg.register(
            fantastic_exec_bridge::HANDLER_MODULE,
            fantastic_exec_bridge::ExecBridgeBundle,
        );
        // the base a folder bundle (`<root>/bundles/<name>/`, `base: py_agent`)
        // extends — it runs the folder's program
        reg.register(
            fantastic_py_agent::HANDLER_MODULE,
            fantastic_py_agent::PyAgentBundle,
        );
    }

    reg
}

#[cfg(test)]
mod tests {
    use super::default_bundles;

    #[test]
    fn the_default_set_lists_the_known_bundles() {
        let reg = default_bundles();
        let listed: Vec<&str> = reg.catalog().map(|(hm, _)| hm).collect();
        for hm in [
            "kernel_state.tools",
            "file_bridge.tools",
            "web.tools",
            "scheduler.tools",
        ] {
            assert!(listed.contains(&hm), "{hm} missing from {listed:?}");
        }
        #[cfg(feature = "full")]
        {
            assert!(listed.contains(&fantastic_exec_bridge::HANDLER_MODULE));
            assert!(listed.contains(&fantastic_py_agent::HANDLER_MODULE));
        }
    }

    #[test]
    fn every_listed_bundle_dispatches() {
        let reg = default_bundles();
        for (hm, _) in reg.catalog() {
            assert!(reg.get(hm).is_some(), "{hm} listed but does not dispatch");
        }
    }

    #[cfg(feature = "scenario-probes")]
    #[test]
    fn scenario_probes_dispatch_but_are_not_in_the_catalog() {
        use fantastic_scenario_probes as probes;
        let reg = default_bundles();
        let listed: Vec<&str> = reg.catalog().map(|(hm, _)| hm).collect();
        for hm in [
            probes::DISPENSER_MODULE,
            probes::PLANT_MODULE,
            probes::PURCHASE_MODULE,
            probes::USER_MODULE,
            probes::ORACLE_MODULE,
            probes::SLOW_MODULE,
        ] {
            assert!(reg.get(hm).is_some(), "{hm} must still dispatch");
            assert!(
                !listed.contains(&hm),
                "{hm} is a fixture, not a bundle: {listed:?}"
            );
        }
        assert!(listed.contains(&"file_bridge.tools"), "{listed:?}");
    }
}
