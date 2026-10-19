//! EVERY OS CALL IS IN `src/home/sys`: no other source of the crate (the test
//! host in `src/bin/` aside) names the file system, processes, the network, the
//! environment, signals or the wall clock. The scan is of text, so a forbidden
//! word in a comment fails it too.

use std::fs;
use std::path::{Path, PathBuf};

const FORBIDDEN: [&str; 11] = [
    "std::fs",
    "std::process",
    "std::net",
    "std::env",
    "tokio::fs",
    "tokio::net",
    "tokio::process",
    "tokio::signal",
    "SystemTime",
    "libc",
    "windows_sys",
];

fn sources(dir: &Path, skip: &[PathBuf], out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
    {
        let path = entry
            .unwrap_or_else(|e| panic!("cannot read an entry of {}: {e}", dir.display()))
            .path();
        if skip.contains(&path) {
            continue;
        }
        if path.is_dir() {
            sources(&path, skip, out);
        } else if path.extension().is_some_and(|x| x == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn home_reaches_the_os_only_through_sys() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let skip = [src.join("home").join("sys"), src.join("bin")];
    let mut files = Vec::new();
    sources(&src, &skip, &mut files);
    assert!(
        files.len() >= 8,
        "the sources were found under {}: {files:?}",
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
        "every OS call goes through src/home/sys:\n{}",
        found.join("\n")
    );
}
