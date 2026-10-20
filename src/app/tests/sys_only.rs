//! PLATFORM CODE STAYS IN `src/sys`: no other source of the shell names a
//! platform or a platform crate. The scan is of text, so a forbidden word in a
//! comment fails it too.

use std::fs;
use std::path::{Path, PathBuf};

const FORBIDDEN: [&str; 6] = [
    "cfg(target_os",
    "cfg(windows)",
    "cfg(unix)",
    "objc2",
    "windows_version",
    "window_vibrancy",
];

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
        let path = entry.expect("an entry").path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|x| x == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn platform_code_stays_in_src_sys() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let sys = src.join("sys");
    let mut files = Vec::new();
    sources(&src, &mut files);
    assert!(
        files.iter().any(|f| f.ends_with("main.rs")),
        "the scan sees the sources"
    );
    let mut found = Vec::new();
    for file in files.iter().filter(|f| !f.starts_with(&sys)) {
        let text = fs::read_to_string(file).expect("a source");
        for (n, line) in text.lines().enumerate() {
            for word in FORBIDDEN.iter().filter(|w| line.contains(*w)) {
                found.push(format!("{}:{}: `{word}`", file.display(), n + 1));
            }
        }
    }
    assert!(
        found.is_empty(),
        "platform code outside src/sys:\n{}",
        found.join("\n")
    );
}
