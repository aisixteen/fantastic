//! THE OS BOUNDARY, enforced: no file of the crate's `src/` outside
//! `src/sys/linux/` may name the D-Bus crates or reach a process or the file
//! system. A scan of text, like the pure crate's `purity.rs`; tests are not
//! scanned.

use std::fs;
use std::path::{Path, PathBuf};

const FORBIDDEN: [&str; 5] = [
    "zbus",
    "zvariant",
    "std::process",
    "tokio::process",
    "std::fs",
];

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
    {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|x| x == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn the_os_is_reached_only_from_sys_linux() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let linux = src.join("sys").join("linux");
    let mut files = Vec::new();
    sources(&src, &mut files);
    assert!(
        files.iter().any(|f| f.starts_with(&linux)),
        "the scan must see src/sys/linux"
    );
    let mut found = Vec::new();
    for file in files.iter().filter(|f| !f.starts_with(&linux)) {
        let text = fs::read_to_string(file).unwrap();
        for (n, line) in text.lines().enumerate() {
            for word in FORBIDDEN.iter().filter(|w| line.contains(*w)) {
                found.push(format!("{}:{}: `{word}`", file.display(), n + 1));
            }
        }
    }
    assert!(
        found.is_empty(),
        "the OS is reached only from src/sys/linux:\n{}",
        found.join("\n")
    );
}
