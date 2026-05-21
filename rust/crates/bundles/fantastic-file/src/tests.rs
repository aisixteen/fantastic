//! Unit tests for this bundle crate.

use super::*;

#[test]
fn readme_present_and_titled() {
    assert!(!README.is_empty());
    assert!(README.contains("file — filesystem as an agent"));
    assert!(README.contains("Verbs: read, write, list, delete, rename, mkdir"));
}

#[test]
fn resolve_safe_refuses_escape() {
    let tmp = tempfile::TempDir::new().unwrap();
    let err = resolve_safe(tmp.path(), "../escape").unwrap_err();
    assert!(err.contains("escapes root"));
}

#[test]
fn resolve_safe_keeps_subpath() {
    let tmp = tempfile::TempDir::new().unwrap();
    let p = resolve_safe(tmp.path(), "sub/inner.txt").unwrap();
    assert!(p.starts_with(tmp.path()));
    assert!(p.ends_with("sub/inner.txt"));
}

#[test]
fn resolve_safe_refuses_absolute_path() {
    let tmp = tempfile::TempDir::new().unwrap();
    let err = resolve_safe(tmp.path(), "/etc/passwd").unwrap_err();
    assert!(err.contains("escapes root"));
}

#[test]
fn resolve_safe_dot_dot_after_descent_is_ok() {
    // a/b/.. resolves to root/a — still inside.
    let tmp = tempfile::TempDir::new().unwrap();
    let p = resolve_safe(tmp.path(), "a/b/../leaf.txt").unwrap();
    assert!(p.starts_with(tmp.path()));
}
