//! `fantastic` — the kernel as a terminal shell. The shell itself is the
//! `fantastic_tui` library (`src/lib.rs`); this binary is that library with no
//! extra bundles. Another binary embeds the same shell with its own (#248).

/// The entry point, and nothing else (`fantastic_tui::main_with`). A PLAIN
/// `fn main`, not `#[tokio::main]`: some modes must rearrange the process
/// (stdio, env) before any runtime thread exists.
fn main() -> anyhow::Result<()> {
    fantastic_tui::main_with(std::env::args().skip(1).collect(), &[])
}
