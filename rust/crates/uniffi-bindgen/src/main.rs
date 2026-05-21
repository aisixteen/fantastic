//! `uniffi-bindgen` CLI — re-export of uniffi's standard entrypoint.
//!
//! Pinned to the same uniffi version the workspace uses (see Cargo.toml).
//! `cargo install --path rust/crates/uniffi-bindgen --locked` puts the
//! binary on PATH for `scripts/build-xcframework.sh`.

fn main() {
    uniffi::uniffi_bindgen_main()
}
