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
