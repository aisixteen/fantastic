//! THE BOUNDARY, proved on the dependency graph: the base binary alone has none of
//! the six system agents, and the kiosk binary has all six.
//!
//! The base binary is `fantastic_tui::main_with(args, &[])`: the `fantastic-tui`
//! crate and what it depends on. An agent's bundle can only be in that binary if
//! the crate that holds it is in that dependency closure. `Cargo.lock` records the
//! closure of every package, so these tests walk it. Nothing here names a base
//! source file.

use std::collections::{BTreeMap, BTreeSet};

/// The crates that hold the six agents: the bundles, and their pure models.
const AGENT_CRATES: [&str; 2] = ["fantastic-system-agents", "fantastic-system"];

/// `Cargo.lock` as `package name -> the names it depends on`, every version of a name merged.
fn graph() -> BTreeMap<String, BTreeSet<String>> {
    let lock = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.lock"))
        .expect("Cargo.lock: run `cargo build` first");
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut name = String::new();
    let mut in_deps = false;
    for line in lock.lines() {
        if line == "[[package]]" {
            in_deps = false;
        } else if let Some(n) = line.strip_prefix("name = \"") {
            name = n.trim_end_matches('"').to_string();
            out.entry(name.clone()).or_default();
        } else if line == "dependencies = [" {
            in_deps = true;
        } else if in_deps && line == "]" {
            in_deps = false;
        } else if in_deps {
            let dep = line.trim().trim_matches(|c| c == ',' || c == '"');
            out.get_mut(&name)
                .unwrap()
                .insert(dep.split(' ').next().unwrap().to_string());
        }
    }
    out
}

/// Every crate `root` reaches, itself included.
fn closure(g: &BTreeMap<String, BTreeSet<String>>, root: &str) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut todo = vec![root.to_string()];
    while let Some(n) = todo.pop() {
        if seen.insert(n.clone()) {
            todo.extend(g.get(&n).into_iter().flatten().cloned());
        }
    }
    seen
}

#[test]
fn the_base_binary_alone_has_none_of_the_six_agents() {
    let base = closure(&graph(), "fantastic-tui");
    assert!(
        base.contains("fantastic-kernel"),
        "the walk reaches the base's own crates: {base:?}"
    );
    for c in AGENT_CRATES {
        assert!(!base.contains(c), "the base's closure holds `{c}`");
    }
}

#[test]
fn the_kiosk_binary_has_all_six_agents() {
    let kiosk = closure(&graph(), "fantastic-kiosk");
    assert!(
        kiosk.contains("fantastic-tui"),
        "the kiosk is built on the base"
    );
    for c in AGENT_CRATES {
        assert!(kiosk.contains(c), "the kiosk's closure lacks `{c}`");
    }
    // and its bundles register under every one of the six handler modules
    let mut reg = fantastic_kernel::BundleRegistry::new();
    fantastic_kiosk::register(&mut reg);
    let got: BTreeSet<&str> = reg.catalog().map(|(m, _)| m).collect();
    let want: BTreeSet<&str> = [
        "power.tools",
        "network.tools",
        "audio.tools",
        "displays.tools",
        "notifications.tools",
        "lock.tools",
    ]
    .into();
    assert_eq!(got, want);
    assert_eq!(
        fantastic_kiosk::IDS,
        [
            "power",
            "network",
            "audio",
            "displays",
            "notifications",
            "lock"
        ]
    );
}
