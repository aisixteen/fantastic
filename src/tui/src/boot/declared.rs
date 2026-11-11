//! GRANTS A PRODUCT DECLARES FOR A STOCK AGENT, and the one rule for merging them.
//!
//! A parent entry in an embedder's blueprint ([`super::seed::seed_dir_with`])
//! may carry `callable_by` and `can_call` for an agent the stock tree owns. Two
//! cases have no record on disk at seed time, so the grants cannot be written
//! into one:
//!
//! - the ROOT: the kernel creates its own record on first boot;
//! - a stock agent CODE creates later (the credential store, once a key mode is
//!   chosen).
//!
//! For those the seed keeps the declaration in ONE small file of the shell's own
//! corner, `.fantastic/cli/grants.yaml`: `{<id>: {callable_by, can_call}}`. The
//! code that makes the agent reads its entry ([`pending`]), merges it into what
//! it creates, and drops the entry once the agent carries the grants
//! ([`done`]). No verb and no record field: the file is the shell's, the
//! agent's record carries only ordinary grants.
//!
//! MERGE IS A UNION PER KEY, never a removal or a narrowing ([`fantastic_kernel::grants::union`]).

use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// The two grant fields a product may declare.
pub const FIELDS: [&str; 2] = ["callable_by", "can_call"];

fn file(cli: &Path) -> PathBuf {
    cli.join("grants.yaml")
}

/// Merge the `callable_by` / `can_call` of `from` into the record `into`.
pub fn union_record(into: &mut Map<String, Value>, from: &Map<String, Value>) {
    for f in FIELDS {
        if let Some(Value::Object(g)) = from.get(f) {
            let mut have = into
                .get(f)
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            fantastic_kernel::grants::union(&mut have, g);
            into.insert(f.to_string(), Value::Object(have));
        }
    }
}

fn read(cli: &Path) -> Result<Map<String, Value>, String> {
    let p = file(cli);
    match std::fs::read_to_string(&p) {
        Ok(t) => match fantastic_kernel::yaml_io::load(&t)
            .map_err(|e| format!("{}: {e}", p.display()))?
        {
            Value::Object(m) => Ok(m),
            _ => Err(format!("{}: not a map", p.display())),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(e) => Err(format!("{}: {e}", p.display())),
    }
}

fn write(cli: &Path, all: &Map<String, Value>) -> Result<(), String> {
    let p = file(cli);
    if all.is_empty() {
        return match std::fs::remove_file(&p) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(format!("{}: {e}", p.display()))
            }
            _ => Ok(()),
        };
    }
    crate::atomic::write_yaml(&p, &Value::Object(all.clone()))
}

/// Seed time: remember `grants` (its `callable_by` / `can_call`) for `id`,
/// unioned with what an earlier blueprint declared for it.
pub fn declare(cli: &Path, id: &str, grants: &Map<String, Value>) -> Result<(), String> {
    let mut all = read(cli)?;
    let mut rec = all
        .get(id)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    union_record(&mut rec, grants);
    all.insert(id.to_string(), Value::Object(rec));
    write(cli, &all)
}

/// The grants declared for `id` and not yet carried by its agent, if any.
pub fn pending(cli: &Path, id: &str) -> Result<Option<Map<String, Value>>, String> {
    Ok(read(cli)?.get(id).and_then(Value::as_object).cloned())
}

/// `id`'s agent carries its declared grants: forget the declaration.
pub fn done(cli: &Path, id: &str) -> Result<(), String> {
    let mut all = read(cli)?;
    if all.remove(id).is_some() {
        write(cli, &all)?;
    }
    Ok(())
}

/// Boot, once the kernel runs: the grants declared for the ROOT, merged into the
/// live root (a union, through the kernel's own grant editor), written to disk,
/// then forgotten. The root record is
/// the kernel's, so this is where a declaration for it first has a record to
/// meet; what the root carries from an earlier boot is on disk and only grows.
pub async fn apply_to_root(
    kernel: &std::sync::Arc<fantastic_kernel::Kernel>,
) -> Result<(), String> {
    let cli = kernel.state_dir()?.join("cli");
    let key = fantastic_kernel::bootstrap::DEFAULT_ROOT_ID;
    let Some(g) = pending(&cli, key)? else {
        return Ok(());
    };
    let id = kernel
        .root()
        .ok_or("the kernel booted with no root")?
        .id
        .clone();
    if let Some(Value::Object(cb)) = g.get("callable_by") {
        kernel.edit_callable_by(&id, |m| {
            let before = m.clone();
            fantastic_kernel::grants::union(m, cb);
            *m != before
        })?;
    }
    if let Some(Value::Object(cc)) = g.get("can_call") {
        kernel.edit_can_call(&id, |m| {
            let before = m.clone();
            fantastic_kernel::grants::union(m, cc);
            *m != before
        })?;
    }
    // the declaration goes only after the root's record is on disk
    kernel.flush_records().await;
    done(&cli, key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn m(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn a_declaration_is_kept_merged_and_forgotten() {
        let d = std::env::temp_dir().join(format!("fc-declared-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        assert_eq!(pending(&d, "a").unwrap(), None);
        declare(&d, "a", &m(json!({"callable_by": {"p": ["x"]}}))).unwrap();
        declare(
            &d,
            "a",
            &m(json!({"callable_by": {"p": ["y"]}, "can_call": {"q": "*"}})),
        )
        .unwrap();
        declare(&d, "b", &m(json!({"can_call": {"q": "*"}}))).unwrap();
        assert_eq!(
            pending(&d, "a").unwrap().unwrap(),
            m(json!({"callable_by": {"p": ["x", "y"]}, "can_call": {"q": "*"}}))
        );
        done(&d, "a").unwrap();
        assert_eq!(pending(&d, "a").unwrap(), None);
        assert!(pending(&d, "b").unwrap().is_some());
        done(&d, "b").unwrap();
        assert!(
            !file(&d).exists(),
            "the last declaration takes the file with it"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    fn registry() -> fantastic_kernel::BundleRegistry {
        let mut reg = fantastic_kernel::BundleRegistry::new();
        reg.register(
            fantastic_core::HANDLER_MODULE,
            fantastic_core::KernelStateBundle::default(),
        );
        reg.register("file_bridge.tools", fantastic_file::FileBundle::default());
        reg.register(
            fantastic_ram_store::HANDLER_MODULE,
            fantastic_ram_store::RamStoreBundle::default(),
        );
        reg.register(
            fantastic_sqlite_store::HANDLER_MODULE,
            fantastic_sqlite_store::SqliteStoreBundle,
        );
        reg.register(
            fantastic_exec_bridge::HANDLER_MODULE,
            fantastic_exec_bridge::ExecBridgeBundle,
        );
        reg.register(
            fantastic_user::HANDLER_MODULE,
            fantastic_user::UserBundle::default(),
        );
        reg
    }

    /// What `Host::boot` does before the kernel's start: the root's memory store.
    async fn start(k: &std::sync::Arc<fantastic_kernel::Kernel>) {
        let root = k.root().unwrap().id.clone();
        let have = k
            .root()
            .unwrap()
            .meta
            .read()
            .unwrap()
            .contains_key("memory_store_id");
        if !have {
            let p =
                json!({"type": "update_agent", "id": root.0, "memory_store_id": "kernel_memory"});
            let r = k
                .send(vec![fantastic_kernel::Item::new(
                    fantastic_kernel::AgentId::from("kernel"),
                    p,
                )])
                .await
                .remove(0)
                .result;
            assert!(r.get("error").is_none(), "{r}");
        }
        let st = k.start(false).await;
        assert_eq!(st["started"], json!(true), "{st}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_roots_declared_grants_are_unioned_in_once_and_survive_a_reboot() {
        use fantastic_kernel::bootstrap::{self, BootstrapOptions};
        let dir = std::env::temp_dir().join(format!("fc-declared-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bp = "agents:\n  - id: '@root'\n    callable_by:\n      user: [get]\n    can_call:\n      user: '*'\n";
        assert_eq!(crate::boot::seed::seed_dir_with(&dir.join(".fantastic"), &[bp]), Ok(true));
        let cli = crate::env::Paths::of(dir.join(".fantastic"), dir.clone()).cli;
        let carries = |k: &fantastic_kernel::Kernel| {
            let r = k.root().unwrap();
            let meta = r.meta.read().unwrap();
            (
                meta["callable_by"]["user"].clone(),
                meta["can_call"]["user"].clone(),
                meta["callable_by"]["$self"].clone(),
            )
        };

        let booted = bootstrap::bootstrap(
            registry(),
            BootstrapOptions::daemon(dir.join(".fantastic"), &dir),
        )
        .expect("boots");
        start(&booted.kernel).await;
        apply_to_root(&booted.kernel).await.unwrap();
        assert_eq!(
            carries(&booted.kernel),
            (json!(["get"]), json!("*"), json!("*")),
            "unioned onto the root's own `$self` grant"
        );
        assert_eq!(
            pending(&cli, fantastic_kernel::bootstrap::DEFAULT_ROOT_ID).unwrap(),
            None,
            "forgotten once carried"
        );
        bootstrap::shutdown(&dir.join(".fantastic")).unwrap();
        drop(booted);

        let again = bootstrap::bootstrap(
            registry(),
            BootstrapOptions::daemon(dir.join(".fantastic"), &dir),
        )
        .expect("boots again");
        start(&again.kernel).await;
        apply_to_root(&again.kernel).await.unwrap();
        assert_eq!(
            carries(&again.kernel),
            (json!(["get"]), json!("*"), json!("*")),
            "on disk, so a later boot still has them"
        );
        bootstrap::shutdown(&dir.join(".fantastic")).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
