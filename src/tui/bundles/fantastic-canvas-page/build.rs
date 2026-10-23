//! PACK THE CANVAS, then embed it. `src/tui/web/canvas/scripts/pack.sh` builds
//! `dist/canvas.zip` with the ts library's vendored, sha-pinned esbuild (POSIX
//! sh, no npm, no node); the zip is copied into OUT_DIR and `include_bytes!`d, so
//! the binary carries the whole page and nothing is written beside it.
//!
//! A failed pack FAILS THE BUILD — a binary without its page is not a build.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets it"));
    let app = manifest.join("../../web/canvas").canonicalize().expect("the canvas app is in the tree");
    let lib = manifest.join("../../../lib/ts").canonicalize().expect("the ts library is in the tree");
    // Rebuild when the app or the library changes.
    for dir in [app.join("src"), app.join("scripts"), lib.join("src"), lib.join("scripts")] {
        println!("cargo:rerun-if-changed={}", dir.display());
    }
    println!("cargo:rerun-if-changed={}", app.join("readme.md").display());
    // Pack into OUT_DIR, never the source tree: parallel builds of one checkout must not race.
    let pack_dir = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets it")).join("canvas-pack");
    std::fs::create_dir_all(&pack_dir).expect("create the pack dir under OUT_DIR");
    let zip = pack_dir.join("canvas.zip");
    let out = Command::new("sh")
        .arg("scripts/pack.sh")
        .arg(&zip)
        .current_dir(&app)
        .output()
        .expect("sh runs");
    if !out.status.success() {
        panic!(
            "canvas pack failed:\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let dest = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets it")).join("canvas.zip");
    std::fs::copy(&zip, &dest).expect("copy canvas.zip into OUT_DIR");
}
