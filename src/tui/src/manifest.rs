//! `fs_loader`'s ROSTER — the file where ui agents are declared.
//!
//! It is NOT where the specified set lives. A loader forces `parent_id` to itself
//! on create and strips it on re-apply, so a manifest entry is ALWAYS a child of
//! the loader; `user` and the agents under it are therefore records on disk
//! (see `boot::seed`). This file starts empty and grows when ui logic adds to it.
//!
//! Order is preserved on rewrite (`serde_yaml` over an index map), so an entry
//! added by hand keeps its place. Comments do not survive a rewrite, which the
//! header says out loud.

use std::path::Path;

use serde_yaml::{Mapping, Value};

pub const HEADER: &str = "\
# .fantastic/kernel.yaml — fs_loader's roster. Where UI AGENTS GO.
#
# Every entry here becomes a CHILD OF fs_loader: the loader overwrites parent_id
# with its own, so an entry cannot choose its parent. That is why the specified
# set (user, and memory, store, exec, files under it) lives on disk under
# .fantastic/agents/ and not in this file.
#
# Each entry must carry an `id` and a `handler_module`; boot stops LOUD at the
# first entry missing either. Every other key is passed through as a record field.
#
# `fantastic` preserves key and entry order when it rewrites this file; comments
# you add below are not preserved.
";

/// Read the manifest. A MISSING FILE IS NOT AN ERROR — it is an empty roster.
pub fn read(path: &Path) -> Result<Vec<Mapping>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let doc: Value = serde_yaml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let Some(list) = doc.get("agents").and_then(Value::as_sequence) else {
        return Ok(Vec::new());
    };
    Ok(list
        .iter()
        .filter_map(|v| v.as_mapping().cloned())
        .collect())
}

pub fn id_of(entry: &Mapping) -> Option<String> {
    entry
        .get(Value::from("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Insert or REPLACE one entry by id, keeping its position.
pub fn put(entries: &mut Vec<Mapping>, entry: Mapping) -> Result<(), String> {
    let id = id_of(&entry).ok_or_else(|| "manifest entry has no `id`".to_string())?;
    match entries
        .iter()
        .position(|e| id_of(e).as_deref() == Some(&id))
    {
        Some(i) => entries[i] = entry,
        None => entries.push(entry),
    }
    Ok(())
}

/// Serialize and write, `tmp → fsync → rename` in the same directory. A half
/// written manifest would produce a partial roster, which looks like lost agents
/// rather than a failed write.
pub fn write(path: &Path, entries: &[Mapping]) -> Result<(), String> {
    let mut doc = Mapping::new();
    doc.insert(
        Value::from("agents"),
        Value::Sequence(entries.iter().cloned().map(Value::Mapping).collect()),
    );
    let body = serde_yaml::to_string(&Value::Mapping(doc))
        .map_err(|e| format!("serializing the manifest: {e}"))?;
    crate::atomic::write(path, format!("{HEADER}{body}").as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("fc-man-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.join("kernel.yaml")
    }

    #[test]
    fn a_missing_manifest_is_an_empty_roster_not_an_error() {
        assert_eq!(
            read(&std::env::temp_dir().join("fc-man-nope/kernel.yaml"))
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn a_round_trip_preserves_order_and_replaces_by_id() {
        let p = tmp("rt");
        let mut e = Vec::new();
        // How ui logic adds an agent: append an entry with an id and a handler.
        for id in ["ui_a", "ui_b"] {
            let mut m = Mapping::new();
            m.insert(Value::from("id"), Value::from(id));
            m.insert(Value::from("handler_module"), Value::from("x.tools"));
            put(&mut e, m).unwrap();
        }
        write(&p, &e).unwrap();
        let back = read(&p).unwrap();
        assert_eq!(
            back.iter().filter_map(id_of).collect::<Vec<_>>(),
            vec!["ui_a", "ui_b"]
        );

        // Replacing an entry must not move it.
        let mut first = back[0].clone();
        first.insert(Value::from("mode"), Value::from("live"));
        e = back;
        put(&mut e, first).unwrap();
        assert_eq!(id_of(&e[0]).as_deref(), Some("ui_a"));
        assert_eq!(e.len(), 2, "replace, not append");
        assert!(e.iter().any(|x| id_of(x).as_deref() == Some("ui_b")));

        write(&p, &e).unwrap();
        assert!(std::fs::read_to_string(&p)
            .unwrap()
            .starts_with("# .fantastic/kernel.yaml"));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn an_entry_without_an_id_is_refused() {
        assert!(put(&mut Vec::new(), Mapping::new()).is_err());
    }
}
