//! `fantastic` — the kernel as a terminal shell. The shell itself is the
//! `fantastic_tui` library (`src/lib.rs`); this binary is that library with no
//! extra bundles and no readme sections. Another binary embeds the same shell with its own (#248).

/// THE LAUNCHER (#555): seeds a fresh folder (the stock tree; `BLUEPRINTS = []`)
/// before the kernel starts, only when `starts_kernel(argv)`, then calls
/// `main_with(argv, &[], &[])` (no extras, no readme sections). A PLAIN `fn main`, not `#[tokio::main]`: some modes
/// must rearrange the process (stdio, env) before any runtime thread exists.
fn main() -> anyhow::Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if fantastic_tui::starts_kernel(&argv) {
        fantastic_tui::seed_dir_with(&std::env::current_dir()?, &[])
            .map_err(|e| anyhow::anyhow!("init failed: {e}"))?;
    }
    fantastic_tui::main_with(argv, &[], &[])
}
