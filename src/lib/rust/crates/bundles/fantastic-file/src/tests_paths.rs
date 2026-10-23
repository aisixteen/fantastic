//! Issue #305: a refused or not-found path names the root and the relative form.

use super::*;

#[test]
fn absolute_path_inside_root_refusal_names_relative_form() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = fantastic_bundle::realpath(tmp.path()).unwrap().join("wt-x");
    std::fs::create_dir_all(&root).unwrap();
    let abs = root.join("a.rs").display().to_string();
    let err = fs::resolve(&root, &abs).unwrap_err();
    assert!(err.contains("escapes root"), "{err}");
    assert!(
        err.contains(&format!(
            "paths are relative to the root {}",
            root.display()
        )),
        "{err}"
    );
    assert!(err.ends_with("e.g. a.rs"), "{err}");
}

#[test]
fn redundant_leading_root_segment_not_found_suggests_stripped_path() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = fantastic_bundle::realpath(tmp.path()).unwrap().join("wt-x");
    std::fs::create_dir_all(&root).unwrap();
    let hint = fs::not_found_hint(&root, "wt-x/a.rs");
    assert!(
        hint.contains(&format!(
            "paths are relative to the root {}",
            root.display()
        )),
        "{hint}"
    );
    assert!(hint.ends_with("e.g. a.rs"), "{hint}");
}

#[test]
fn outside_path_refusal_names_root_without_suggestion() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = fantastic_bundle::realpath(tmp.path()).unwrap().join("wt-x");
    std::fs::create_dir_all(&root).unwrap();
    let err = fs::resolve(&root, "../escape").unwrap_err();
    assert!(
        err.contains(&format!(
            "paths are relative to the root {}",
            root.display()
        )),
        "{err}"
    );
    assert!(!err.contains("e.g."), "{err}");
}
