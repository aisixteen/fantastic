//! Unit tests for this bundle crate.

use super::*;
use tempfile::TempDir;

#[test]
fn readme_present_and_titled() {
    assert!(!README.is_empty());
    assert!(README.contains("This is a Fantastic kernel"));
    assert!(README.contains("send(target_id, payload)"));
}

#[test]
fn seed_root_readme_writes_when_missing() {
    let tmp = TempDir::new().unwrap();
    seed_root_readme(tmp.path()).unwrap();
    let path = tmp.path().join(".fantastic/readme.md");
    assert!(path.exists());
    let read = std::fs::read_to_string(&path).unwrap();
    assert_eq!(read, README);
}

#[test]
fn seed_root_readme_is_idempotent_and_preserves_edits() {
    let tmp = TempDir::new().unwrap();
    seed_root_readme(tmp.path()).unwrap();
    let path = tmp.path().join(".fantastic/readme.md");
    std::fs::write(&path, "USER EDIT").unwrap();
    seed_root_readme(tmp.path()).unwrap();
    let read = std::fs::read_to_string(&path).unwrap();
    assert_eq!(read, "USER EDIT");
}

#[test]
fn docs_tree_present_keyed_and_descriptored() {
    assert!(!DOCS.is_empty());
    assert!(DOCS.iter().any(|(k, _)| *k == "self-extension"));
    // A12.2 — `minds` is not a topic on python, so it is not one here.
    assert!(!DOCS.iter().any(|(k, _)| *k == "minds"), "minds is gone");
    let mut keys: Vec<_> = DOCS.iter().map(|(k, _)| *k).collect();
    let n = keys.len();
    keys.sort_unstable();
    keys.dedup();
    assert_eq!(keys.len(), n, "duplicate doc key");
    for (k, body) in DOCS {
        assert!(!body.is_empty(), "empty doc {k}");
        assert!(
            body.starts_with("> "),
            "doc {k} must open with a `> ` descriptor line"
        );
    }
}

#[test]
fn seed_root_readme_writes_docs_tree() {
    let tmp = TempDir::new().unwrap();
    seed_root_readme(tmp.path()).unwrap();
    let se = tmp.path().join(".fantastic/docs/self-extension.md");
    assert!(se.exists(), "docs/self-extension.md not seeded");
    let want = DOCS.iter().find(|(k, _)| *k == "self-extension").unwrap().1;
    assert_eq!(std::fs::read_to_string(&se).unwrap(), want);
}

/// A12.1 + A12.2 + A12.3 — the keyed doc tree IS python's: the same keys (no
/// more `minds`, and the six topics rust never had), and every body byte for
/// byte. The rust index was 9 top-level entries against python's 12.
#[test]
fn docs_equal_python_verbatim() {
    let py = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(4)
        .expect("repo's src/lib above the crate")
        .join("python/bundled_agents/loader/kernel_state/src/kernel_state");
    let docs = py.join("docs");

    // Every `.md` under python's docs/, keyed the way a nested tier is keyed.
    let mut want: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&docs).expect("python docs/") {
        let path = entry.expect("dir entry").path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if path.is_dir() {
            for sub in std::fs::read_dir(&path).expect("nested tier") {
                let sub = sub.expect("dir entry").path();
                if sub.extension().and_then(|e| e.to_str()) == Some("md") {
                    let leaf = sub.file_stem().and_then(|s| s.to_str()).expect("leaf");
                    want.push(format!("{stem}/{leaf}"));
                }
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            want.push(stem.to_string());
        }
    }
    want.sort();
    let mut got: Vec<String> = DOCS.iter().map(|(k, _)| (*k).to_string()).collect();
    got.sort();
    assert_eq!(got, want, "the doc key set must be python's");

    for (key, body) in DOCS {
        let path = docs.join(format!("{key}.md"));
        assert_eq!(
            *body,
            std::fs::read_to_string(&path).expect("python doc"),
            "{key} has drifted from {}",
            path.display()
        );
    }
}

/// A12.4 — the root orientation readme is python's, byte for byte (rust was
/// missing the whole `kernel_verbs` paragraph).
#[test]
fn root_readme_is_pythons_verbatim() {
    let py = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(4)
        .expect("repo's src/lib above the crate")
        .join("python/bundled_agents/loader/kernel_state/src/kernel_state/readme.md");
    assert_eq!(README, std::fs::read_to_string(&py).expect("python readme"));
    assert!(README.contains("kernel_verbs"));
}

/// A12.1 — a nested tier seeds into its own directory, so `reflect
/// readme=frontend/routing` has a file to read.
#[test]
fn seed_root_readme_writes_nested_tiers() {
    let tmp = TempDir::new().unwrap();
    seed_root_readme(tmp.path()).unwrap();
    let docs = tmp.path().join(".fantastic/docs");
    for key in [
        "frontend",
        "frontend/panel-recipe",
        "frontend/routing",
        "orchestration",
        "system-verbs",
        "terminal",
    ] {
        let path = docs.join(format!("{key}.md"));
        assert!(path.exists(), "{key} not seeded");
        let want = DOCS.iter().find(|(k, _)| k == &key).unwrap().1;
        assert_eq!(std::fs::read_to_string(&path).unwrap(), want);
    }
    assert!(!docs.join("minds.md").exists(), "minds is gone");
}
