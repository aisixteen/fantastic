//! THE ONLY WRITE POINT of a first run: it materialises `blueprint.yaml`.
//!
//! The base tree is DATA, embedded in the binary, applied once at init. Init is
//! explicit, never a default: it runs only when `fantastic` or `fantastic mcp`
//! is run in the directory. A directory neither ran in gets nothing written to
//! it. Every later run rehydrates what is on disk and never
//! reads the blueprint again.
//!
//! DISK POSITION IS THE PARENT. The kernel takes a child's parent from the
//! directory it sits in and ignores any `parent_id` field, so the blueprint may
//! not carry one — the writer refuses it rather than write two truths that can
//! disagree.
//!
//! `mcp` is never in the blueprint: it is added by code on the `mcp` command. The
//! writer refuses it too, so the two can never meet.

use serde_json::{Map, Value};

use crate::atomic;
use crate::env::Paths;
use crate::manifest;

/// The level-one loader. Its manifest is where ui agents arrive.
pub const LOADER: &str = "fs_loader";

/// The base tree, compiled in so the binary is self-contained.
pub const BLUEPRINT: &str = include_str!("../../blueprint.yaml");

/// Ids the blueprint may never contain. `mcp` is added by code, on its command.
const NOT_IN_BLUEPRINT: [&str; 1] = ["mcp"];

/// How a grant in the blueprint names the root. The root's id is the kernel's to
/// choose (it changed once already, `core` → `kernel_state`), so the blueprint
/// never spells it: the writer puts the kernel's own id in its place.
pub const ROOT_PLACEHOLDER: &str = "@root";

/// Rewrite `@root` to the kernel's root id in the two grant maps.
fn resolve_root(record: &mut Map<String, Value>) {
    for field in ["can_call", "callable_by"] {
        if let Some(Value::Object(g)) = record.get_mut(field) {
            if let Some(v) = g.remove(ROOT_PLACEHOLDER) {
                g.insert(fantastic_kernel::bootstrap::DEFAULT_ROOT_ID.to_string(), v);
            }
        }
    }
}

/// Parse the blueprint into its top-level entries. Every error names what is
/// wrong: a blueprint that does not say what it means must not be applied.
fn entries(text: &str) -> Result<Vec<Value>, String> {
    let doc: Value = serde_yaml::from_str(text).map_err(|e| format!("blueprint: {e}"))?;
    match doc.get("agents") {
        Some(Value::Array(a)) => Ok(a.clone()),
        _ => Err("blueprint: top level must be `agents: [...]`".into()),
    }
}

/// The blueprint's `root:` fields — set on the kernel's own root at boot, each
/// only while the root does not carry it (see `Host::boot`).
pub fn root_fields() -> Result<Map<String, Value>, String> {
    let doc: Value = serde_yaml::from_str(BLUEPRINT).map_err(|e| format!("blueprint: {e}"))?;
    match doc.get("root") {
        Some(Value::Object(m)) => Ok(m.clone()),
        _ => Err("blueprint: `root:` must be a map of the root's fields".into()),
    }
}

/// Write one level of the tree under `dir` (which is some `…/agents`), then
/// recurse into each entry's own `agents:`.
fn write_level(dir: &std::path::Path, list: &[Value]) -> Result<(), String> {
    for entry in list {
        let obj = entry
            .as_object()
            .ok_or_else(|| format!("blueprint: an entry under {} is not a map", dir.display()))?;
        let id = obj
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("blueprint: an entry under {} has no `id`", dir.display()))?;
        if obj.contains_key("parent_id") {
            return Err(format!(
                "blueprint: `{id}` names a parent_id; the directory is the parent"
            ));
        }
        if NOT_IN_BLUEPRINT.contains(&id) {
            return Err(format!(
                "blueprint: `{id}` may not be in the blueprint; it is added by its own command"
            ));
        }
        let here = dir.join(id);
        std::fs::create_dir_all(&here)
            .map_err(|e| format!("could not write {}: {e}", here.display()))?;

        let mut record: Map<String, Value> = obj.clone();
        let kids = record.remove("agents");
        resolve_root(&mut record);
        atomic::write_yaml(&here.join("agent.yaml"), &Value::Object(record))?;

        match kids {
            None => {}
            Some(Value::Array(k)) => write_level(&here.join("agents"), &k)?,
            Some(_) => return Err(format!("blueprint: `{id}.agents` must be a list")),
        }
    }
    Ok(())
}

/// Init: write every record the blueprint names, plus the loader's manifest.
///
/// A partial tree is left in place on failure, and the error says so: silently
/// removing a directory the user may have had other files in would be worse than
/// a clear message.
pub fn write_all(paths: &Paths) -> Result<(), String> {
    write_from(paths, BLUEPRINT)
}

/// `write_all` over an explicit blueprint text, so tests can prove the writer's
/// refusals without editing the shipped file.
fn write_from(paths: &Paths, text: &str) -> Result<(), String> {
    let list = entries(text)?;
    std::fs::create_dir_all(&paths.cli)
        .map_err(|e| format!("could not write {}: {e}", paths.cli.display()))?;

    // NOT WRITTEN HERE: `.fantastic/agent.yaml`. The kernel creates the root
    // itself on first boot, with its own id and a freshly minted guid. Writing it
    // from here would be this shell guessing at a record the kernel owns.
    write_level(&paths.store.join("agents"), &list)?;

    // The loader's manifest, ONLY IF ABSENT — a human's roster is never
    // overwritten. It starts empty: every entry a loader creates lands under the
    // loader, so the specified set lives on disk above and this file is reserved
    // for the ui agents that arrive later.
    let man = paths.manifest();
    if !man.exists() {
        manifest::write(&man, &[])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The root's id, as the kernel names it — never spelled in the tui.
    const ROOT_ID: &str = fantastic_kernel::bootstrap::DEFAULT_ROOT_ID;
    /// The level-one grouping node the blueprint writes.
    const SET: &str = "set_of_agents";

    fn sandbox(tag: &str) -> Paths {
        let d = std::env::temp_dir().join(format!("fc-seed-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Paths::at(&d)
    }

    fn rec(p: &Paths, rel: &str) -> Value {
        let f = p.store.join("agents").join(rel);
        crate::atomic::read_yaml(&f).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn the_tree_on_disk_is_the_specified_tree() {
        let p = sandbox("tree");
        assert!(p.is_fresh());
        write_all(&p).unwrap();

        // Level one: exactly these nodes.
        let mut lvl1: Vec<String> = std::fs::read_dir(p.store.join("agents"))
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        lvl1.sort();
        assert_eq!(lvl1, vec!["fs_loader", "memory", "set_of_agents", "store"]);

        // The set: exactly three, and NESTED under it, which is what makes them
        // its children — the kernel reads the parent off the directory.
        let mut inset: Vec<String> = std::fs::read_dir(p.store.join("agents/set_of_agents/agents"))
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        inset.sort();
        assert_eq!(inset, vec!["exec", "files", "user"]);

        assert!(
            !p.is_fresh(),
            "the directory is seeded, so the next run rehydrates"
        );
        assert!(p.cli.is_dir());
        let _ = std::fs::remove_dir_all(&p.cwd);
    }

    #[test]
    fn the_root_names_the_blueprints_memory_store() {
        let f = root_fields().unwrap();
        assert_eq!(f["memory_store_id"], json!("memory"));
        let list = entries(BLUEPRINT).unwrap();
        let mem = list.iter().find(|e| e["id"] == json!("memory")).expect("the store is in the blueprint");
        assert_eq!(mem["handler_module"], json!(fantastic_sqlite_store::HANDLER_MODULE));
        assert_eq!(mem["operator"], json!(true));
        assert_eq!(mem["admins"], json!(["kernel"]));
    }

    #[test]
    fn the_root_record_is_left_to_the_kernel() {
        let p = sandbox("root");
        write_all(&p).unwrap();
        assert!(
            !p.store.join("agent.yaml").exists(),
            "the kernel creates its own root, with its own guid"
        );
        let _ = std::fs::remove_dir_all(&p.cwd);
    }

    #[test]
    fn no_record_carries_a_parent_id_because_the_directory_is_the_parent() {
        let p = sandbox("noparent");
        write_all(&p).unwrap();
        for r in [
            "set_of_agents/agent.yaml",
            "fs_loader/agent.yaml",
            "set_of_agents/agents/exec/agent.yaml",
            "set_of_agents/agents/files/agent.yaml",
            "set_of_agents/agents/user/agent.yaml",
        ] {
            assert!(
                rec(&p, r).get("parent_id").is_none(),
                "{r} names a parent twice"
            );
        }
        let _ = std::fs::remove_dir_all(&p.cwd);
    }

    #[test]
    fn the_grouping_node_dispatches_nothing() {
        let p = sandbox("bare");
        write_all(&p).unwrap();
        let set = rec(&p, "set_of_agents/agent.yaml");
        assert_eq!(set["id"], "set_of_agents");
        assert!(
            set.get("handler_module").is_none(),
            "a bare node has no handler — and the kernel still loads it, so its \
             children are reachable"
        );
        let _ = std::fs::remove_dir_all(&p.cwd);
    }

    #[test]
    fn exec_can_run_anything_and_is_not_sealed() {
        let p = sandbox("exec");
        write_all(&p).unwrap();
        let e = rec(&p, "set_of_agents/agents/exec/agent.yaml");
        assert_eq!(e["allowed"], json!([".*"]));
        // Both halves are needed: an unsealed leg with no `allowed` runs nothing,
        // and `allowed` on a sealed leg never gets asked.
        assert_eq!(e["ingress_rule"], "allow_all_ingress");
        assert_eq!(e["callable_by"]["user"], "*");
        let _ = std::fs::remove_dir_all(&p.cwd);
    }

    #[test]
    fn files_is_clamped_to_the_running_directory() {
        let p = sandbox("files");
        write_all(&p).unwrap();
        let f = rec(&p, "set_of_agents/agents/files/agent.yaml");
        assert_eq!(f["root"], ".");
        assert_eq!(f["ingress_rule"], "allow_all_ingress");
        let _ = std::fs::remove_dir_all(&p.cwd);
    }

    #[test]
    fn the_human_is_granted_every_sibling_it_must_drive() {
        // The born-with baseline is {$self, $parent, $children}, which does NOT
        // reach a sibling. Without these names the human can call nothing.
        let p = sandbox("grants");
        write_all(&p).unwrap();
        let u = rec(&p, "set_of_agents/agents/user/agent.yaml");
        for id in ["exec", "files", SET, LOADER, ROOT_ID] {
            assert_eq!(u["can_call"][id], "*", "the human cannot reach {id}");
        }
        let _ = std::fs::remove_dir_all(&p.cwd);
    }

    #[test]
    fn the_manifest_starts_empty_because_loader_entries_land_under_the_loader() {
        let p = sandbox("man");
        write_all(&p).unwrap();
        let text = std::fs::read_to_string(p.manifest()).unwrap();
        assert!(text.contains("agents:"));
        assert!(manifest::read(&p.manifest()).unwrap().is_empty());
        for invented in ["web", "rest", "port", "notes"] {
            assert!(!text.contains(invented), "{invented} was never asked for");
        }
        let _ = std::fs::remove_dir_all(&p.cwd);
    }

    #[test]
    fn a_second_seed_never_overwrites_a_humans_manifest() {
        let p = sandbox("keep");
        write_all(&p).unwrap();
        let mine = "agents:\n  - id: ui\n    handler_module: x.tools\n";
        std::fs::write(p.manifest(), mine).unwrap();
        write_all(&p).unwrap();
        assert_eq!(std::fs::read_to_string(p.manifest()).unwrap(), mine);
        let _ = std::fs::remove_dir_all(&p.cwd);
    }

    #[test]
    fn the_shipped_blueprint_never_contains_mcp() {
        let text = BLUEPRINT;
        let list = entries(text).unwrap();
        fn ids(v: &[Value], out: &mut Vec<String>) {
            for e in v {
                out.push(e["id"].as_str().unwrap_or("").to_string());
                if let Some(Value::Array(k)) = e.get("agents") {
                    ids(k, out);
                }
            }
        }
        let mut all = Vec::new();
        ids(&list, &mut all);
        assert!(!all.iter().any(|i| i == "mcp"), "mcp is added by code, not the blueprint");
    }

    #[test]
    fn a_blueprint_naming_mcp_is_refused() {
        let p = sandbox("nomcp");
        let err = write_from(&p, "agents:\n  - id: mcp\n    handler_module: web_mcp.tools\n")
            .unwrap_err();
        assert!(err.contains("mcp"), "{err}");
        let _ = std::fs::remove_dir_all(&p.cwd);
    }

    #[test]
    fn a_blueprint_naming_a_parent_is_refused() {
        let p = sandbox("parent");
        let err = write_from(&p, "agents:\n  - id: x\n    parent_id: core\n").unwrap_err();
        assert!(err.contains("parent_id"), "{err}");
        let _ = std::fs::remove_dir_all(&p.cwd);
    }

    #[test]
    fn the_store_is_a_file_bridge_over_the_store_dir() {
        // Without it the kernel keeps the ledger and messages in RAM.
        let p = sandbox("store");
        write_all(&p).unwrap();
        let s = rec(&p, "store/agent.yaml");
        assert_eq!(s["handler_module"], "file_bridge.tools");
        assert_eq!(s["allow"], json!(["."]), "a bridge is deny-all per path: the store exposes its whole dir");
        assert_eq!(s["root"], ".fantastic");
        let _ = std::fs::remove_dir_all(&p.cwd);
    }

    #[test]
    fn a_root_grant_is_written_as_the_kernels_own_root_id() {
        let p = sandbox("root_id");
        write_all(&p).unwrap();
        let u = rec(&p, "set_of_agents/agents/user/agent.yaml");
        assert!(u["can_call"].get(ROOT_PLACEHOLDER).is_none(), "the placeholder never reaches disk");
        assert_eq!(u["can_call"][ROOT_ID], "*");
        let _ = std::fs::remove_dir_all(&p.cwd);
    }
}
