//! PURITY, enforced: no source file of the crate may reach the OS, the clock,
//! the environment or a runtime, nor step outside safe Rust. The scan is of
//! text, so a forbidden word in a comment fails it too.

use std::fs;
use std::path::{Path, PathBuf};

/// What no source file may contain.
const FORBIDDEN: [&str; 7] = [
    "std::fs",
    "std::process",
    "std::net",
    "std::time",
    "std::env",
    "tokio",
    "unsafe",
];

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries =
        fs::read_dir(dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
    for entry in entries {
        let path = entry
            .unwrap_or_else(|e| panic!("cannot read an entry of {}: {e}", dir.display()))
            .path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|x| x == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn the_sources_are_pure() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    sources(&src, &mut files);
    assert!(
        !files.is_empty(),
        "no sources found under {}",
        src.display()
    );

    let mut found = Vec::new();
    for file in &files {
        let text = fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", file.display()));
        for (n, line) in text.lines().enumerate() {
            for word in FORBIDDEN.iter().filter(|w| line.contains(*w)) {
                found.push(format!("{}:{}: `{word}`", file.display(), n + 1));
            }
        }
    }
    assert!(
        found.is_empty(),
        "the crate must stay pure:\n{}",
        found.join("\n")
    );
}
