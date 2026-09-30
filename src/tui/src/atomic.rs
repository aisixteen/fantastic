//! `tmp → fsync → rename`, in the destination's OWN directory.
//!
//! Every record this shell seeds goes through here. The reason is specific: the
//! loader treats an unparsable root record as an EMPTY TREE, so a half-written
//! `agent.yaml` does not fail loudly — it makes the kernel look empty. A rename
//! within one directory is atomic, so a reader sees either the old file or the
//! whole new one, never a prefix.

use std::io::Write;
use std::path::Path;

pub fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| format!("{}: no parent directory", path.display()))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    // The temp file MUST share the directory: a rename across filesystems is not
    // atomic, and /tmp is frequently a different one.
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("f"),
        std::process::id()
    ));
    {
        let mut f = std::fs::File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
        f.write_all(bytes)
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
        f.sync_all()
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })
}

/// Write a YAML record through the KERNEL'S OWN emitter
/// (`fantastic_kernel::yaml_io::dump`), so a record this shell seeds is
/// byte-identical to one the kernel writes itself.
pub fn write_yaml(path: &Path, v: &serde_json::Value) -> Result<(), String> {
    write(path, fantastic_kernel::yaml_io::dump(v).as_bytes())
}

/// Read one YAML document the way the kernel does (tests read the seed back).
#[cfg(test)]
pub fn read_yaml(path: &Path) -> Result<serde_json::Value, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    fantastic_kernel::yaml_io::load(&text).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_temp_file_never_survives_a_successful_write() {
        let d = std::env::temp_dir().join(format!("fc-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let p = d.join("deep/agent.yaml");
        write_yaml(&p, &serde_json::json!({"id":"x"})).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "id: x\n");
        assert_eq!(read_yaml(&p).unwrap(), serde_json::json!({"id":"x"}));
        let leftovers: Vec<_> = std::fs::read_dir(p.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_rewrite_replaces_rather_than_appends() {
        let d = std::env::temp_dir().join(format!("fc-atomic2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let p = d.join("f");
        write(&p, b"aaaa").unwrap();
        write(&p, b"b").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"b");
        let _ = std::fs::remove_dir_all(&d);
    }
}
