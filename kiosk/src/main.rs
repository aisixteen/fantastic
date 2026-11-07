//! `fantastic-kiosk` — the fantastic shell with the machine's agents added. The shell
//! is the base's `fantastic_tui` library; this binary is that library with the
//! kiosk's bundles ([`fantastic_kiosk::register`]), installed on the image as
//! `/usr/bin/fantastic`.

/// The entry point, and nothing else. A PLAIN `fn main`, not `#[tokio::main]`: some
/// modes rearrange the process (stdio, env) before any runtime thread exists.
fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // First run of a canvas folder: the kiosk's agents are written beside the base's,
    // ids as written, before the kernel boots. Every later run rehydrates from disk.
    if args.iter().any(|a| a == "canvas") {
        fantastic_tui::seed_dir_with(&std::env::current_dir()?, &[fantastic_kiosk::BLUEPRINT])
            .map_err(|e| format!("kiosk seed: {e}"))?;
    }
    Ok(fantastic_tui::main_with(
        args,
        &[fantastic_kiosk::register],
    )?)
}
