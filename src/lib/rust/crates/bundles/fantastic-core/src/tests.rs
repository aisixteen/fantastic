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
