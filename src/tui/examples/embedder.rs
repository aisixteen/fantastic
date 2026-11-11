//! A minimal embedder, for the e2e tests only (#555, #580): another binary built on the public
//! crate API alone. It is the launcher of `src/main.rs` plus ONE extra blueprint: it asks
//! `starts_kernel`, seeds the stock tree and its blueprint into the state folder the command
//! line names (`state_dir_of`), then calls `main_with`. The blueprint is YAML, not boot code.

/// The embedder's one blueprint: a store agent under `user`, next to the stock ones.
const BLUEPRINT: &str = "\
agents:
  - id: user
    agents:
      - id: embedder_store
        handler_module: ram_store.tools
";

fn main() -> anyhow::Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if fantastic_tui::starts_kernel(&argv) {
        let state = fantastic_tui::state_dir_of(&argv).map_err(|e| anyhow::anyhow!("{e}"))?;
        fantastic_tui::seed_dir_with(&state, &[BLUEPRINT])
            .map_err(|e| anyhow::anyhow!("init failed: {e}"))?;
    }
    fantastic_tui::main_with(argv, &[], &[])
}
