//! FOLDER BUNDLES — `<root>/bundles/<name>/bundle.yaml` (owner, 2026-10-08): code
//! that EXTENDS a bundled agent (`base`), loaded from this kernel's own root at
//! boot (before the tree loads, so an instance's handler resolves) and by the
//! root's `reload_bundle`. The base builds the bundle ([`Bundle::folder_bundle`]).
//! Python `kernel_state.load_bundle` / `load_bundles`, text for text.

use std::path::Path;

use serde_json::{json, Value};

use crate::kernel::Kernel;

/// The bundles dir under a kernel root.
pub const BUNDLES_DIR: &str = "bundles";
/// A folder bundle's manifest.
pub const BUNDLE_FILE: &str = "bundle.yaml";

/// A folder name is the handler_module of its instances: letters, digits, `_`, `-`,
/// starting with a letter or `_`. Python `_FOLDER_NAME`.
#[must_use]
pub fn is_folder_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Load ONE folder bundle `<root>/bundles/<name>/` into `kernel`'s registry. →
/// `None`, or the error — recorded on its catalog row too, never loaded.
#[must_use]
pub fn load_bundle(kernel: &Kernel, root: &Path, name: &str) -> Option<String> {
    let shown = root.file_name().map_or_else(
        || root.display().to_string(),
        |n| n.to_string_lossy().to_string(),
    );
    let row = json!({
        "name": name,
        "handler_module": name,
        "folder": format!("{shown}/{BUNDLES_DIR}/{name}"),
    });
    let refuse = |why: String| -> Option<String> {
        let err = format!("bundles/{name}: {why}");
        let mut r = row.clone();
        r["error"] = json!(err);
        kernel.bundles.refuse_folder(name, r);
        Some(err)
    };
    if !is_folder_name(name) {
        return refuse(
            "a bundle folder is named with letters, digits, _ and - (the name is its \
             instances' handler_module)"
                .to_string(),
        );
    }
    if kernel.bundles.clashes(name) {
        return refuse(format!(
            "{} is a built-in bundle — a folder may not shadow it",
            fantastic_bundle::py_repr(name)
        ));
    }
    let folder = root.join(BUNDLES_DIR).join(name);
    let Ok(text) = std::fs::read_to_string(folder.join(BUNDLE_FILE)) else {
        return refuse("no bundle.yaml".to_string());
    };
    let spec = match crate::yaml_io::load(&text) {
        Ok(v) => v,
        Err(e) => return refuse(format!("bundle.yaml does not parse: {e}")),
    };
    if !spec.is_object() {
        return refuse("bundle.yaml is not a mapping".to_string());
    }
    let bases = kernel.bundles.folder_bases().join(", ");
    let known = if bases.is_empty() {
        "none".to_string()
    } else {
        bases
    };
    let Some(base) = spec
        .get("base")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return refuse(format!(
            "base: required — the bundled agent it extends (here: {known})"
        ));
    };
    let Some(sentence) = spec
        .get("sentence")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return refuse("sentence: required — one line saying what it is".to_string());
    };
    let Some(base_bundle) = kernel.bundles.by_name(base).filter(|b| b.folder_base()) else {
        return refuse(format!(
            "base: {} is no bundled agent a folder can extend in this kernel — known: {known}",
            fantastic_bundle::py_repr(base)
        ));
    };
    let abs = folder.canonicalize().unwrap_or(folder);
    match base_bundle.folder_bundle(name, &abs, &spec) {
        Ok(built) => {
            let mut r = row;
            r["base"] = json!(base);
            r["sentence"] = json!(sentence);
            kernel.bundles.register_folder(name, built, r);
            None
        }
        Err(why) => refuse(why),
    }
}

/// Load every folder under `<root>/bundles/` — the cold read at boot.
pub fn load_bundles(kernel: &Kernel, root: &Path) {
    let Ok(entries) = std::fs::read_dir(root.join(BUNDLES_DIR)) else {
        return;
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    for n in names {
        let _ = load_bundle(kernel, root, &n);
    }
}
