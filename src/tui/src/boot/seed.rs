//! THE ONLY WRITE POINT of a first run: it materialises `blueprint.yaml`.
//!
//! The base tree is DATA, embedded in the binary, applied once at init. Init runs
//! in the LAUNCHER, before the kernel starts (`main.rs`: `starts_kernel(argv)`
//! then `seed_dir_with`). The kernel and `main_with` never seed, and a command
//! that starts no kernel writes nothing. Every later run rehydrates what is on
//! disk and never reads the blueprint again.
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
#[cfg(test)]
use crate::env::Paths;

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

/// Every id written so far, with where it sits on disk and which blueprint wrote
/// it. Ids are one namespace across the whole tree, so a second writer of an id
/// (other than as a parent entry, see [`write_blueprint`]) is a clash.
#[derive(Default)]
struct Written {
    ids: std::collections::HashMap<String, (std::path::PathBuf, String)>,
}

impl Written {
    fn claim(&mut self, id: &str, here: &std::path::Path, who: &str) -> Result<(), String> {
        match self.ids.get(id) {
            Some((_, first)) => Err(format!(
                "blueprint: `{id}` in {who} is already written by {first}"
            )),
            None => {
                self.ids.insert(id.to_string(), (here.to_path_buf(), who.to_string()));
                Ok(())
            }
        }
    }
}

/// Write one level of the tree under `dir` (which is some `…/agents`), then
/// recurse into each entry's own `agents:`.
fn write_level(dir: &std::path::Path, list: &[Value], seen: &mut Written, who: &str) -> Result<(), String> {
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
        seen.claim(id, &here, who)?;
        std::fs::create_dir_all(&here)
            .map_err(|e| format!("could not write {}: {e}", here.display()))?;

        let mut record: Map<String, Value> = obj.clone();
        let kids = record.remove("agents");
        resolve_root(&mut record);
        atomic::write_yaml(&here.join("agent.yaml"), &Value::Object(record))?;

        match kids {
            None => {}
            Some(Value::Array(k)) => write_level(&here.join("agents"), &k, seen, who)?,
            Some(_) => return Err(format!("blueprint: `{id}.agents` must be a list")),
        }
    }
    Ok(())
}

/// Stock agents CODE creates after the seed (not on disk at seed time): a
/// parent entry naming one declares grants for it, kept until it is made (see
/// [`crate::boot::declared`]). Each is read by the code that creates it.
const MADE_BY_CODE: [&str; 1] = [crate::kern::keys::STORE];

/// The grants a parent entry carries, `@root` resolved: only `callable_by` and
/// `can_call`, each a map.
fn declared_grants(obj: &Map<String, Value>, id: &str) -> Result<Map<String, Value>, String> {
    let mut g = Map::new();
    for f in crate::boot::declared::FIELDS {
        match obj.get(f) {
            None => {}
            Some(v @ Value::Object(_)) => {
                g.insert(f.to_string(), v.clone());
            }
            Some(_) => return Err(format!("blueprint: `{id}.{f}` must be a grant map")),
        }
    }
    resolve_root(&mut g);
    Ok(g)
}

/// Write one blueprint's top level. A top entry is a PARENT entry when it names
///
/// - an agent an EARLIER blueprint wrote, or
/// - the ROOT (`@root`, or the kernel's own root id), or
/// - a stock agent code creates later ([`MADE_BY_CODE`]).
///
/// It carries `id`, `agents:` (written beneath that agent) and the grants
/// `callable_by` / `can_call`, which are UNIONED into the agent's grants, never
/// removing or narrowing one. Any other field is refused. An agent written
/// earlier has its record rewritten with the union; the root and a code-made
/// agent have no record yet, so their grants are declared for when they are made
/// ([`crate::boot::declared`]). A code-made agent has no children dir to place
/// `agents:` under, so it takes grants only.
fn write_blueprint(cli: &std::path::Path, agents_dir: &std::path::Path, list: &[Value], seen: &mut Written, who: &str) -> Result<(), String> {
    let earlier: std::collections::HashSet<String> = seen.ids.keys().cloned().collect();
    let root_id = fantastic_kernel::bootstrap::DEFAULT_ROOT_ID;
    for entry in list {
        let id = entry
            .as_object()
            .and_then(|o| o.get("id"))
            .and_then(Value::as_str)
            .filter(|id| earlier.contains(*id) || *id == ROOT_PLACEHOLDER || *id == root_id || MADE_BY_CODE.contains(id));
        let Some(id) = id else {
            write_level(agents_dir, std::slice::from_ref(entry), seen, who)?;
            continue;
        };
        let obj = entry.as_object().expect("an entry with an id is a map");
        let is_root = id == ROOT_PLACEHOLDER || id == root_id;
        let first = seen.ids.get(id).map_or_else(|| "the kernel".to_string(), |(_, f)| f.clone());
        if let Some(extra) = obj.keys().find(|k| !matches!(k.as_str(), "id" | "agents" | "callable_by" | "can_call")) {
            return Err(format!(
                "blueprint: `{id}` in {who} is already written by {first}; as a parent it may carry only `id`, `agents`, `callable_by` and `can_call`, not `{extra}`"
            ));
        }
        let grants = declared_grants(obj, id)?;
        if !grants.is_empty() {
            match seen.ids.get(id) {
                Some((at, _)) => {
                    let f = at.join("agent.yaml");
                    let Value::Object(mut rec) = crate::atomic::read_yaml(&f)? else {
                        return Err(format!("{}: not a map", f.display()));
                    };
                    crate::boot::declared::union_record(&mut rec, &grants);
                    atomic::write_yaml(&f, &Value::Object(rec))?;
                }
                None => crate::boot::declared::declare(cli, if is_root { root_id } else { id }, &grants)?,
            }
        }
        let under = if is_root {
            agents_dir.to_path_buf()
        } else if let Some((at, _)) = seen.ids.get(id) {
            at.join("agents")
        } else {
            if obj.contains_key("agents") {
                return Err(format!("blueprint: `{id}` in {who} is made by code later; it takes grants, not `agents`"));
            }
            continue;
        };
        match obj.get("agents") {
            None => {}
            Some(Value::Array(k)) => write_level(&under, k, seen, who)?,
            Some(_) => return Err(format!("blueprint: `{id}.agents` must be a list")),
        }
    }
    Ok(())
}

/// Init: write every record the blueprint names.
///
/// A partial tree is left in place on failure, and the error says so: silently
/// removing a directory the user may have had other files in would be worse than
/// a clear message.
#[cfg(test)]
pub fn write_all(paths: &Paths) -> Result<(), String> {
    write_from(&paths.store, &[BLUEPRINT])
}

/// THE HOST ENTRY POINT: seed the state folder `state` the way the `fantastic` CLI does on a first
/// run. A launcher (fantastic's own, or an embedder's) calls this before `main_with` when `starts_kernel(argv)`.
///
/// Idempotent: when `state` already exists it writes nothing and
/// returns `Ok(false)`; otherwise it writes the blueprint tree (the `user`
/// record and its children) and returns `Ok(true)`. Refuses the fantastic home
/// itself, like the CLI. The error text is the writer's.
pub fn seed_dir(state: &std::path::Path) -> Result<bool, String> {
    seed_dir_with(state, &[])
}

/// [`seed_dir`] plus an embedder's own blueprints, written after the stock one,
/// in order, by the same writer and under the same rules (no `parent_id`, no
/// `mcp`, `@root` resolved, disk position is the parent). Ids land as written.
///
/// A top entry whose id an earlier blueprint wrote is a parent entry: `id`,
/// `agents:` (written beneath that agent) and the grants `callable_by` /
/// `can_call`, which are merged into the agent's as a union per key (a stock
/// grant is never removed or narrowed; `@root` is resolved). The root (`@root`)
/// and the stock agents code creates later (the credential store) take the same
/// entry: their grants are kept and applied when the agent is made, and stay
/// across later boots. Any other field on a parent entry is refused, and so is
/// any other reuse of an earlier id, naming both writers. On a refusal the tree
/// written so far stays on disk.
pub fn seed_dir_with(state: &std::path::Path, blueprints: &[&str]) -> Result<bool, String> {
    if state.exists() {
        return Ok(false);
    }
    fantastic_kernel::bootstrap::refuse_home_as_state_dir(state, fantastic_kernel::bootstrap::fantastic_home().as_deref())
        .map_err(|e| e.to_string())?;
    let mut texts = vec![BLUEPRINT];
    texts.extend_from_slice(blueprints);
    write_from(state, &texts)?;
    Ok(true)
}

/// `write_all` over explicit blueprint texts (the first is the stock one), so
/// tests can prove the writer's refusals without editing the shipped file.
fn write_from(state: &std::path::Path, texts: &[&str]) -> Result<(), String> {
    // Parse every blueprint before the first write: a text that does not parse
    // must not leave a half-seeded store behind.
    let lists = texts.iter().map(|t| entries(t)).collect::<Result<Vec<_>, _>>()?;
    let cli = state.join("cli");
    std::fs::create_dir_all(&cli).map_err(|e| format!("could not write {}: {e}", cli.display()))?;

    // NOT WRITTEN HERE: `.fantastic/agent.yaml`. The kernel creates the root
    // itself on first boot, with its own id and a freshly minted guid. Writing it
    // from here would be this shell guessing at a record the kernel owns.
    let agents = state.join("agents");
    let mut seen = Written::default();
    for (i, list) in lists.iter().enumerate() {
        let who = if i == 0 { "the stock blueprint".to_string() } else { format!("blueprint {i}") };
        write_blueprint(&cli, &agents, list, &mut seen, &who)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The root's id, as the kernel names it — never spelled in the tui.
    const ROOT_ID: &str = fantastic_kernel::bootstrap::DEFAULT_ROOT_ID;
    /// The one level-one node the blueprint writes: everything is under it.
    const USER: &str = "user";

    fn sandbox(tag: &str) -> Paths {
        let d = std::env::temp_dir().join(format!("fc-seed-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Paths::of(d.join(".fantastic"), d)
    }

    fn rec(p: &Paths, rel: &str) -> Value {
        let f = p.store.join("agents").join(rel);
        crate::atomic::read_yaml(&f).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn the_tree_on_disk_is_the_specified_tree() {
        let p = sandbox("tree");
        assert!(!p.store.exists());
        write_all(&p).unwrap();

        // Level one: exactly these nodes.
        let mut lvl1: Vec<String> = std::fs::read_dir(p.store.join("agents"))
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        lvl1.sort();
        assert_eq!(lvl1, vec!["kernel_memory", "sidebar", USER]);

        // Under `user`: exactly these, NESTED under it, which is what makes them
        // its children — the kernel reads the parent off the directory.
        let mut inset: Vec<String> = std::fs::read_dir(p.store.join("agents/user/agents"))
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        inset.sort();
        assert_eq!(inset, vec!["exec", "files", "memory", "store"]);

        assert!(
            p.store.exists(),
            "the directory is seeded, so the next run rehydrates"
        );
        assert!(p.cli.is_dir());
        let _ = std::fs::remove_dir_all(&p.work);
    }

    #[test]
    fn the_root_names_the_blueprints_memory_store() {
        let f = root_fields().unwrap();
        // the KERNEL'S store is RAM; the AI store is the sqlite file
        assert_eq!(f["memory_store_id"], json!("kernel_memory"));
        let list = entries(BLUEPRINT).unwrap();
        let km = list.iter().find(|e| e["id"] == json!("kernel_memory")).expect("the kernel's store is a root child");
        assert_eq!(km["handler_module"], json!(fantastic_ram_store::HANDLER_MODULE));
        let kids = list.iter().find(|e| e["id"] == json!(USER)).and_then(|u| u["agents"].as_array()).expect("user holds the tree");
        let mem = kids.iter().find(|e| e["id"] == json!("memory")).expect("the store is in the blueprint");
        assert_eq!(mem["handler_module"], json!(fantastic_sqlite_store::HANDLER_MODULE));
        assert!(mem.get("operator").is_none(), "the kernel no longer writes the AI store");
        assert_eq!(mem["admins"], json!(["kernel"]));
    }

    #[test]
    fn the_sidebar_is_seeded_once_under_the_root_as_a_sidebar_tools_agent() {
        let p = sandbox("sidebar");
        assert_eq!(seed_dir(&p.store), Ok(true));
        let r = rec(&p, "sidebar/agent.yaml");
        assert_eq!(r["handler_module"], json!(crate::kern::sidebar::HANDLER_MODULE));
        assert_eq!(r["callable_by"]["user"], "*");
        assert!(r.get("view").is_none(), "the view comes from its reflect, not a record field");
        let before = snapshot(&p.store);
        assert_eq!(seed_dir(&p.store), Ok(false));
        assert_eq!(snapshot(&p.store), before, "a second seed changes nothing");
        let _ = std::fs::remove_dir_all(&p.work);
    }

    #[test]
    fn the_root_record_is_left_to_the_kernel() {
        let p = sandbox("root");
        write_all(&p).unwrap();
        assert!(
            !p.store.join("agent.yaml").exists(),
            "the kernel creates its own root, with its own guid"
        );
        let _ = std::fs::remove_dir_all(&p.work);
    }

    #[test]
    fn no_record_carries_a_parent_id_because_the_directory_is_the_parent() {
        let p = sandbox("noparent");
        write_all(&p).unwrap();
        for r in [
            "user/agent.yaml",
            "user/agents/memory/agent.yaml",
            "user/agents/store/agent.yaml",
            "user/agents/exec/agent.yaml",
            "user/agents/files/agent.yaml",
        ] {
            assert!(
                rec(&p, r).get("parent_id").is_none(),
                "{r} names a parent twice"
            );
        }
        let _ = std::fs::remove_dir_all(&p.work);
    }

    #[test]
    fn exec_can_run_anything_and_is_not_sealed() {
        let p = sandbox("exec");
        write_all(&p).unwrap();
        let e = rec(&p, "user/agents/exec/agent.yaml");
        assert_eq!(e["allowed"], json!([".*"]));
        // Both halves are needed: an unsealed leg with no `allowed` runs nothing,
        // and `allowed` on a sealed leg never gets asked.
        assert_eq!(e["ingress_rule"], "allow_all_ingress");
        assert_eq!(e["callable_by"]["user"], "*");
        let _ = std::fs::remove_dir_all(&p.work);
    }

    #[test]
    fn files_is_clamped_to_the_running_directory() {
        let p = sandbox("files");
        write_all(&p).unwrap();
        let f = rec(&p, "user/agents/files/agent.yaml");
        assert_eq!(f["root"], ".");
        assert_eq!(f["ingress_rule"], "allow_all_ingress");
        let _ = std::fs::remove_dir_all(&p.work);
    }

    #[test]
    fn the_human_is_granted_every_agent_it_must_drive() {
        // The born-with baseline is {$self} alone: without these names the
        // human can call nothing, not even its own children.
        let p = sandbox("grants");
        write_all(&p).unwrap();
        let u = rec(&p, "user/agent.yaml");
        for id in ["exec", "files", "memory", "sidebar", "store", ROOT_ID] {
            assert_eq!(u["can_call"][id], "*", "the human cannot reach {id}");
        }
        let _ = std::fs::remove_dir_all(&p.work);
    }

    #[test]
    fn init_writes_no_roster_file() {
        let p = sandbox("noroster");
        write_all(&p).unwrap();
        assert!(!p.store.join("kernel.yaml").exists(), "every agent is a record under agents/");
        let _ = std::fs::remove_dir_all(&p.work);
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
        let err = write_from(&p.store, &["agents:\n  - id: mcp\n    handler_module: web_mcp.tools\n"])
            .unwrap_err();
        assert!(err.contains("mcp"), "{err}");
        let _ = std::fs::remove_dir_all(&p.work);
    }

    #[test]
    fn a_blueprint_naming_a_parent_is_refused() {
        let p = sandbox("parent");
        let err = write_from(&p.store, &["agents:\n  - id: x\n    parent_id: core\n"]).unwrap_err();
        assert!(err.contains("parent_id"), "{err}");
        let _ = std::fs::remove_dir_all(&p.work);
    }

    #[test]
    fn the_store_is_a_file_bridge_over_the_store_dir() {
        // Without it the kernel keeps the ledger and messages in RAM.
        let p = sandbox("store");
        write_all(&p).unwrap();
        let s = rec(&p, "user/agents/store/agent.yaml");
        assert_eq!(s["handler_module"], "file_bridge.tools");
        assert_eq!(
            s["allow"],
            json!(["."]),
            "a bridge is deny-all per path: the store exposes its whole dir"
        );
        assert_eq!(s["root"], "@state");
        let _ = std::fs::remove_dir_all(&p.work);
    }

    #[test]
    fn a_root_grant_is_written_as_the_kernels_own_root_id() {
        let p = sandbox("root_id");
        write_all(&p).unwrap();
        let u = rec(&p, "user/agent.yaml");
        assert!(u["can_call"].get(ROOT_PLACEHOLDER).is_none(), "the placeholder never reaches disk");
        assert_eq!(u["can_call"][ROOT_ID], "*");
        let _ = std::fs::remove_dir_all(&p.work);
    }

    fn snapshot(root: &std::path::Path) -> std::collections::BTreeMap<String, Vec<u8>> {
        fn walk(base: &std::path::Path, d: &std::path::Path, out: &mut std::collections::BTreeMap<String, Vec<u8>>) {
            for e in std::fs::read_dir(d).unwrap().filter_map(Result::ok) {
                let p = e.path();
                if p.is_dir() {
                    walk(base, &p, out);
                } else {
                    let rel = p.strip_prefix(base).unwrap().to_string_lossy().into_owned();
                    out.insert(rel, std::fs::read(&p).unwrap());
                }
            }
        }
        let mut out = std::collections::BTreeMap::new();
        walk(root, root, &mut out);
        out
    }

    #[test]
    fn seed_dir_on_a_fresh_dir_writes_the_same_tree_as_the_cli() {
        let a = sandbox("seed_a");
        let b = sandbox("seed_b");
        assert_eq!(seed_dir(&a.store), Ok(true));
        write_all(&b).unwrap();
        let (sa, sb) = (snapshot(&a.store), snapshot(&b.store));
        assert_eq!(sa, sb);
        for k in ["kernel_memory", "sidebar", "user", "user/agents/memory", "user/agents/store", "user/agents/exec", "user/agents/files"] {
            assert!(sa.contains_key(&format!("agents/{k}/agent.yaml")), "{k}");
        }
        assert!(!sa.contains_key("agent.yaml"), "the kernel owns the root record");
        let _ = std::fs::remove_dir_all(&a.work);
        let _ = std::fs::remove_dir_all(&b.work);
    }

    #[test]
    fn seed_dir_twice_changes_nothing() {
        let p = sandbox("seed_twice");
        assert_eq!(seed_dir(&p.store), Ok(true));
        let f = p.store.join("agents/user/agent.yaml");
        let mut text = std::fs::read_to_string(&f).unwrap();
        text.push_str("# marker\n");
        std::fs::write(&f, &text).unwrap();
        let before = snapshot(&p.store);
        assert_eq!(seed_dir(&p.store), Ok(false));
        assert_eq!(snapshot(&p.store), before);
        assert!(std::fs::read_to_string(&f).unwrap().contains("# marker"));
        let _ = std::fs::remove_dir_all(&p.work);
    }

    #[test]
    fn seed_dir_leaves_an_existing_store_alone() {
        let p = sandbox("seed_existing");
        std::fs::create_dir_all(&p.store).unwrap();
        assert_eq!(seed_dir(&p.store), Ok(false));
        assert!(!p.store.join("agents").exists());
        let _ = std::fs::remove_dir_all(&p.work);
    }

    #[test]
    fn seed_dir_reports_a_write_failure() {
        let p = sandbox("seed_fail");
        let file = p.work.join("not_a_dir");
        std::fs::write(&file, b"x").unwrap();
        let err = seed_dir(&file.join(".fantastic")).unwrap_err();
        assert!(err.contains("could not write"), "{err}");
        let _ = std::fs::remove_dir_all(&p.work);
    }

    fn seed_with(tag: &str, bp: &str) -> (Paths, Result<bool, String>) {
        let p = sandbox(tag);
        let r = seed_dir_with(&p.store, &[bp]);
        (p, r)
    }

    #[test]
    fn an_embedder_entry_is_written_with_its_id_and_the_stock_tree_too() {
        let (p, r) = seed_with("emb_top", "agents:\n  - id: probe\n    handler_module: ram_store.tools\n");
        assert_eq!(r, Ok(true));
        assert_eq!(rec(&p, "probe/agent.yaml")["id"], "probe");
        assert!(p.store.join("agents/user/agent.yaml").is_file());
        assert!(p.store.join("agents/user/agents/memory/agent.yaml").is_file());
        let _ = std::fs::remove_dir_all(&p.work);
    }

    #[test]
    fn a_parent_entry_places_children_under_a_stock_agent_and_leaves_its_record() {
        let plain = sandbox("emb_plain");
        write_all(&plain).unwrap();
        let (p, r) = seed_with(
            "emb_parent",
            "agents:\n  - id: user\n    agents:\n      - id: probe2\n        handler_module: ram_store.tools\n        can_call:\n          '@root': '*'\n",
        );
        assert_eq!(r, Ok(true));
        let kid = rec(&p, "user/agents/probe2/agent.yaml");
        assert_eq!(kid["id"], "probe2");
        assert!(kid["can_call"].get(ROOT_PLACEHOLDER).is_none());
        assert_eq!(kid["can_call"][ROOT_ID], "*");
        assert_eq!(rec(&p, "user/agent.yaml"), rec(&plain, "user/agent.yaml"));
        let _ = std::fs::remove_dir_all(&p.work);
        let _ = std::fs::remove_dir_all(&plain.work);
    }

    fn grants(p: &Paths) -> Value {
        crate::atomic::read_yaml(&p.cli.join("grants.yaml")).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn a_parent_entry_unions_its_grants_into_the_stock_record() {
        let plain = sandbox("emb_union_plain");
        write_all(&plain).unwrap();
        let (p, r) = seed_with(
            "emb_union",
            "agents:\n  - id: user\n    callable_by:\n      probe: '*'\n      exec: [run]\n      '@root': '*'\n    can_call:\n      memory: [get]\n      probe: [ping]\n",
        );
        assert_eq!(r, Ok(true));
        let (was, now) = (rec(&plain, "user/agent.yaml"), rec(&p, "user/agent.yaml"));
        // added
        assert_eq!(now["callable_by"]["probe"], "*");
        assert_eq!(now["callable_by"][ROOT_ID], "*");
        assert!(now["callable_by"].get(ROOT_PLACEHOLDER).is_none());
        assert_eq!(now["can_call"]["probe"], json!(["ping"]));
        // never narrowed or removed: stock `"*"` stays against a verb list
        assert_eq!(now["callable_by"]["exec"], "*");
        assert_eq!(now["callable_by"]["files"], "*");
        assert_eq!(now["can_call"]["memory"], "*");
        assert_eq!(now["can_call"]["store"], was["can_call"]["store"]);
        let _ = std::fs::remove_dir_all(&p.work);
        let _ = std::fs::remove_dir_all(&plain.work);
    }

    #[test]
    fn a_parent_entry_widens_a_verb_list_across_blueprints() {
        let p = sandbox("emb_widen");
        let first = "agents:\n  - id: probe\n    handler_module: ram_store.tools\n    callable_by:\n      user: [a]\n";
        let second = "agents:\n  - id: probe\n    callable_by:\n      user: [b]\n    agents:\n      - id: kid\n        handler_module: ram_store.tools\n";
        assert_eq!(seed_dir_with(&p.store, &[first, second]), Ok(true));
        assert_eq!(rec(&p, "probe/agent.yaml")["callable_by"]["user"], json!(["a", "b"]));
        assert_eq!(rec(&p, "probe/agent.yaml")["handler_module"], "ram_store.tools");
        assert_eq!(rec(&p, "probe/agents/kid/agent.yaml")["id"], "kid");
        let _ = std::fs::remove_dir_all(&p.work);
    }

    #[test]
    fn a_root_parent_entry_declares_its_grants_and_places_children_under_the_root() {
        let (p, r) = seed_with(
            "emb_root",
            "agents:\n  - id: '@root'\n    callable_by:\n      '@root': '*'\n      user: [get]\n    agents:\n      - id: probe\n        handler_module: ram_store.tools\n  - id: kernel_state\n    can_call:\n      user: '*'\n",
        );
        assert_eq!(r, Ok(true));
        assert_eq!(rec(&p, "probe/agent.yaml")["id"], "probe");
        let g = grants(&p);
        assert_eq!(g[ROOT_ID]["callable_by"], json!({ROOT_ID: "*", "user": ["get"]}));
        assert_eq!(g[ROOT_ID]["can_call"], json!({"user": "*"}));
        assert!(!p.store.join("agent.yaml").exists(), "the root record stays the kernel's");
        let _ = std::fs::remove_dir_all(&p.work);
    }

    #[test]
    fn grants_for_the_code_made_store_are_declared_not_written_as_an_agent() {
        let (p, r) = seed_with("emb_creds", "agents:\n  - id: credentials\n    callable_by:\n      probe: [get]\n");
        assert_eq!(r, Ok(true));
        assert_eq!(grants(&p)["credentials"]["callable_by"], json!({"probe": ["get"]}));
        assert!(!p.store.join("agents/credentials").exists());
        let _ = std::fs::remove_dir_all(&p.work);
        for bad in [
            "agents:\n  - id: credentials\n    handler_module: x\n",
            "agents:\n  - id: credentials\n    agents: []\n",
            "agents:\n  - id: user\n    callable_by: [x]\n",
        ] {
            let (p, r) = seed_with("emb_creds_bad", bad);
            assert!(r.is_err(), "{bad}");
            let _ = std::fs::remove_dir_all(&p.work);
        }
    }

    #[test]
    fn no_grants_declared_means_no_grants_file() {
        let (p, r) = seed_with("emb_nogrants", "agents:\n  - id: probe\n    handler_module: ram_store.tools\n");
        assert_eq!(r, Ok(true));
        assert!(!p.cli.join("grants.yaml").exists());
        let _ = std::fs::remove_dir_all(&p.work);
    }

    #[test]
    fn a_parent_entry_with_another_field_is_refused() {
        let (p, r) = seed_with("emb_field", "agents:\n  - id: user\n    handler_module: x\n");
        let err = r.unwrap_err();
        assert!(err.contains("user") && err.contains("handler_module"), "{err}");
        let _ = std::fs::remove_dir_all(&p.work);
    }

    #[test]
    fn an_id_clash_with_the_stock_tree_is_refused_naming_both() {
        let (p, r) = seed_with("emb_clash", "agents:\n  - id: probe\n    agents:\n      - id: memory\n");
        let err = r.unwrap_err();
        assert!(err.contains("memory") && err.contains("blueprint 1") && err.contains("stock"), "{err}");
        let (p2, r2) = seed_with("emb_clash_top", "agents:\n  - id: exec\n    handler_module: x\n");
        // `exec` is stock but nested, so a top entry naming it is a parent entry
        // carrying another field: refused too.
        assert!(r2.is_err());
        let _ = std::fs::remove_dir_all(&p.work);
        let _ = std::fs::remove_dir_all(&p2.work);
    }

    #[test]
    fn a_second_embedder_blueprint_clashing_with_the_first_is_refused() {
        let p = sandbox("emb_two");
        let a = "agents:\n  - id: probe\n";
        let b = "agents:\n  - id: other\n    agents:\n      - id: probe\n";
        let err = seed_dir_with(&p.store, &[a, b]).unwrap_err();
        assert!(err.contains("probe") && err.contains("blueprint 1") && err.contains("blueprint 2"), "{err}");
        let _ = std::fs::remove_dir_all(&p.work);
    }

    #[test]
    fn seed_dir_with_twice_changes_nothing() {
        let p = sandbox("emb_twice");
        let bp = "agents:\n  - id: probe\n";
        assert_eq!(seed_dir_with(&p.store, &[bp]), Ok(true));
        let before = snapshot(&p.store);
        assert_eq!(seed_dir_with(&p.store, &[bp]), Ok(false));
        assert_eq!(snapshot(&p.store), before);
        let _ = std::fs::remove_dir_all(&p.work);
    }

    #[test]
    fn an_embedder_blueprint_naming_parent_id_or_mcp_is_refused() {
        let (p, r) = seed_with("emb_pid", "agents:\n  - id: x\n    parent_id: core\n");
        assert!(r.unwrap_err().contains("parent_id"));
        let (q, r) = seed_with("emb_mcp", "agents:\n  - id: mcp\n");
        assert!(r.unwrap_err().contains("mcp"));
        let _ = std::fs::remove_dir_all(&p.work);
        let _ = std::fs::remove_dir_all(&q.work);
    }

    #[test]
    fn an_unparsable_embedder_blueprint_writes_nothing() {
        let (p, r) = seed_with("emb_bad", "not: agents\n");
        assert!(r.is_err());
        assert!(!p.store.exists());
        let _ = std::fs::remove_dir_all(&p.work);
    }

    #[test]
    fn seed_dir_is_seed_dir_with_nothing() {
        let a = sandbox("emb_eq_a");
        let b = sandbox("emb_eq_b");
        assert_eq!(seed_dir(&a.store), Ok(true));
        assert_eq!(seed_dir_with(&b.store, &[]), Ok(true));
        assert_eq!(snapshot(&a.store), snapshot(&b.store));
        let _ = std::fs::remove_dir_all(&a.work);
        let _ = std::fs::remove_dir_all(&b.work);
    }
}
