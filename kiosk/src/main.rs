//! `fantastic-kiosk` — the fantastic shell with the machine's agents added. The shell
//! is the base's `fantastic_tui` library; this binary is that library with the
//! kiosk's bundles ([`fantastic_kiosk::register`]), installed on the image as
//! `/usr/bin/fantastic`.

/// The entry point, and nothing else. A PLAIN `fn main`, not `#[tokio::main]`: some
/// modes rearrange the process (stdio, env) before any runtime thread exists.
fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    Ok(fantastic_tui::main_with(
        std::env::args().skip(1).collect(),
        &[fantastic_kiosk::register],
    )?)
}
