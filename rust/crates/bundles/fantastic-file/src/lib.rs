//! Filesystem-as-agent.
//!
//! Each agent owns a `root` directory and a `readonly` flag. Path
//! safety: every verb resolves `root/path` and refuses if the result
//! escapes `root`.
//!
//! ## Verbs
//!
//! | verb     | args                       | reply                                 |
//! |----------|----------------------------|---------------------------------------|
//! | `reflect`| _none_                     | `{id, sentence, root, readonly, hidden_count, verbs}` |
//! | `boot`   | _none_                     | `null`                                |
//! | `list`   | `path?`                    | `{path, files: [{name, path, type, size?}]}` |
//! | `read`   | `path`                     | `{path, content}` (text) or `{path, image_base64, mime}` (image) |
//! | `write`  | `path`, `content`          | `{path, written: true}`               |
//! | `delete` | `path`                     | `{path, deleted: true}`               |
//! | `rename` | `old_path`, `new_path`     | `{old_path, new_path}`                |
//! | `mkdir`  | `path`                     | `{path, created: true}`               |
//!
//! `readonly: true` on the agent record refuses every mutating verb
//! with `{"error": "agent is readonly"}`.

#![deny(missing_docs)]

use async_trait::async_trait;
use base64::Engine;
use fantastic_bundle as _; // dep keeps the bundle ↔ kernel link explicit
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "file.tools";

/// readme.md auto-seeded into the agent's dir on creation.
pub const README: &str = include_str!("readme.md");

/// Default hidden-file names hidden from `list`. Matches the Python
/// bundle so cross-runtime workdirs stay symmetric.
pub const DEFAULT_HIDDEN: &[&str] = &[".git", ".env", ".fantastic", "node_modules", "__pycache__"];

const IMAGE_EXT: &[&str] = &[".png", ".jpg", ".jpeg", ".gif", ".webp", ".svg"];

/// The filesystem bundle.
pub struct FileBundle;

#[async_trait]
impl Bundle for FileBundle {
    fn name(&self) -> &str {
        "file"
    }

    fn readme(&self) -> Option<&'static str> {
        Some(README)
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &std::sync::Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        // Snapshot meta so concurrent updates don't race.
        let agent = match kernel.agents.get(agent_id) {
            Some(e) => std::sync::Arc::clone(&e),
            None => return Ok(Some(json!({"error": format!("no agent {agent_id}")}))),
        };
        let meta = agent.meta.read().expect("meta poisoned").clone();
        let root = root_of(&meta);
        let readonly = meta
            .get("readonly")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let hidden: Vec<String> = match meta.get("hidden").and_then(Value::as_array) {
            Some(arr) => arr
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            None => DEFAULT_HIDDEN.iter().map(|s| s.to_string()).collect(),
        };

        let reply = match verb {
            "reflect" => reflect_reply(agent_id, &root, readonly, &hidden),
            "boot" | "shutdown" => Value::Null,
            "list" => list_reply(&root, payload, &hidden),
            "read" => read_reply(&root, payload),
            "write" => {
                if readonly {
                    json!({"error": "agent is readonly"})
                } else {
                    write_reply(&root, payload)
                }
            }
            "delete" => {
                if readonly {
                    json!({"error": "agent is readonly"})
                } else {
                    delete_reply(&root, payload)
                }
            }
            "rename" => {
                if readonly {
                    json!({"error": "agent is readonly"})
                } else {
                    rename_reply(&root, payload)
                }
            }
            "mkdir" => {
                if readonly {
                    json!({"error": "agent is readonly"})
                } else {
                    mkdir_reply(&root, payload)
                }
            }
            other => json!({"error": format!("unknown verb {other:?}")}),
        };
        Ok(Some(reply))
    }
}

fn root_of(meta: &serde_json::Map<String, Value>) -> PathBuf {
    match meta.get("root").and_then(Value::as_str) {
        Some(s) if !s.is_empty() => expanduser(s),
        _ => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    }
}

/// Tilde expansion so `root=~/projects/foo` works the same as Python's
/// `Path("...").expanduser()`. Only handles a leading `~` (no `~user`).
fn expanduser(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    } else if s == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home);
        }
    }
    PathBuf::from(s)
}

/// Resolve `path` relative to `root`, refusing escapes.
///
/// Walks the components of the caller-supplied `path` (NOT the
/// already-joined absolute form) so we can refuse any `..` that
/// would climb above the root. Returns the joined-and-resolved
/// absolute path on success. Symlink-following is left to the OS
/// at access time — we only guarantee the LITERAL path can't escape.
fn resolve_safe(root: &Path, path: &str) -> Result<PathBuf, String> {
    // An absolute path is a hard escape — refuse, don't silently
    // strip the leading slash.
    if Path::new(path).is_absolute() {
        return Err(format!("path {path:?} escapes root"));
    }
    let rel = path.trim_start_matches('/');
    let mut depth: i32 = 0;
    for c in Path::new(rel).components() {
        match c {
            std::path::Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return Err(format!("path {path:?} escapes root"));
                }
            }
            std::path::Component::CurDir => {}
            std::path::Component::Normal(_) => {
                depth += 1;
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                // An absolute component inside `path` is a hard
                // escape attempt — refuse rather than silently
                // anchoring to filesystem root.
                return Err(format!("path {path:?} escapes root"));
            }
        }
    }
    Ok(root.join(rel))
}

fn reflect_reply(agent_id: &AgentId, root: &Path, readonly: bool, hidden: &[String]) -> Value {
    json!({
        "id": agent_id.as_str(),
        "sentence": "Filesystem root.",
        "root": root.display().to_string(),
        "readonly": readonly,
        "hidden_count": hidden.len(),
        "verbs": {
            "reflect": "Identity + filesystem root + readonly flag. No args.",
            "boot": "No-op. Returns None.",
            "list": "args: path:str (default '').",
            "read": "args: path:str (req).",
            "write": "args: path:str (req), content:str (req).",
            "delete": "args: path:str (req).",
            "rename": "args: old_path:str (req), new_path:str (req).",
            "mkdir": "args: path:str (req).",
        }
    })
}

fn list_reply(root: &Path, payload: &Value, hidden: &[String]) -> Value {
    let path_str = payload.get("path").and_then(Value::as_str).unwrap_or("");
    let target = match resolve_safe(root, path_str) {
        Ok(p) => p,
        Err(e) => return json!({"error": e}),
    };
    let dir = if target.is_dir() {
        target.clone()
    } else {
        return json!({"error": format!("path {path_str:?} is not a directory")});
    };
    let mut files: Vec<Value> = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) => return json!({"error": format!("list {}: {e}", dir.display())}),
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().to_string();
        if hidden.contains(&name) {
            continue;
        }
        let p = entry.path();
        let rel = match p.strip_prefix(root) {
            Ok(r) => r.display().to_string(),
            Err(_) => p.display().to_string(),
        };
        let mut obj = serde_json::Map::new();
        obj.insert("name".to_string(), json!(name));
        obj.insert("path".to_string(), json!(rel));
        if p.is_dir() {
            obj.insert("type".to_string(), json!("dir"));
        } else {
            obj.insert("type".to_string(), json!("file"));
            if let Ok(meta) = p.metadata() {
                obj.insert("size".to_string(), json!(meta.len()));
            }
        }
        files.push(Value::Object(obj));
    }
    files.sort_by(|a, b| {
        a.get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .cmp(b.get("name").and_then(Value::as_str).unwrap_or(""))
    });
    json!({
        "path": path_str,
        "files": files,
    })
}

fn read_reply(root: &Path, payload: &Value) -> Value {
    let Some(path_str) = payload.get("path").and_then(Value::as_str) else {
        return json!({"error": "read requires path"});
    };
    let target = match resolve_safe(root, path_str) {
        Ok(p) => p,
        Err(e) => return json!({"error": e}),
    };
    if !target.exists() {
        return json!({"error": format!("path {path_str:?} not found")});
    }
    let ext_lower = target
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{}", e.to_lowercase()))
        .unwrap_or_default();
    // Three return shapes (Python parity, file/tools.py:122):
    //   1. image extension      → {path, image_base64, mime}
    //   2. UTF-8-readable file  → {path, content}
    //   3. any other binary     → {path, image_base64, mime}
    //
    // The third branch handles videos (.mp4), PDFs, fonts, archives —
    // anything that isn't text. Reusing `image_base64` as the carry
    // field keeps the wire compatible with the existing web proxy
    // (which already base64-decodes that field and serves with the
    // reply's `mime`). Mime comes from the extension lookup; falls
    // back to `application/octet-stream` for the unrecognised tail.
    if IMAGE_EXT.contains(&ext_lower.as_str()) {
        return read_binary(&target, path_str, &ext_lower);
    }
    match std::fs::read_to_string(&target) {
        Ok(content) => json!({"path": path_str, "content": content}),
        // ENOENT / EACCES / etc. genuinely failed — propagate.
        Err(e) if e.kind() != std::io::ErrorKind::InvalidData => {
            json!({"error": format!("read {}: {e}", target.display())})
        }
        // InvalidData = non-UTF-8 file. Fall through to binary path.
        Err(_) => read_binary(&target, path_str, &ext_lower),
    }
}

/// Read `target` as raw bytes + return base64 + mime. Used for any
/// non-UTF-8 file (videos, PDFs, fonts, archives) and explicitly for
/// known image extensions. Mime guess comes from the extension; falls
/// back to `application/octet-stream` for unknown extensions.
fn read_binary(target: &Path, path_str: &str, ext_lower: &str) -> Value {
    match std::fs::read(target) {
        Ok(bytes) => {
            let mime = mime_for_ext(ext_lower);
            json!({
                "path": path_str,
                "image_base64": base64::engine::general_purpose::STANDARD.encode(&bytes),
                "mime": mime,
            })
        }
        Err(e) => json!({"error": format!("read {}: {e}", target.display())}),
    }
}

/// Mime lookup for the (small) set of extensions consumers actually
/// see. Goal: enough coverage that the web proxy serves the correct
/// `Content-Type` for `<img>` / `<video>` / `<audio>` / `<embed>`
/// elements without the browser sniffing.
fn mime_for_ext(ext_lower: &str) -> &'static str {
    match ext_lower {
        ".png" => "image/png",
        ".jpg" | ".jpeg" => "image/jpeg",
        ".gif" => "image/gif",
        ".webp" => "image/webp",
        ".svg" => "image/svg+xml",
        ".mp4" => "video/mp4",
        ".webm" => "video/webm",
        ".mov" => "video/quicktime",
        ".m4v" => "video/x-m4v",
        ".mp3" => "audio/mpeg",
        ".wav" => "audio/wav",
        ".ogg" => "audio/ogg",
        ".m4a" => "audio/mp4",
        ".pdf" => "application/pdf",
        ".woff" => "font/woff",
        ".woff2" => "font/woff2",
        ".ttf" => "font/ttf",
        ".otf" => "font/otf",
        ".zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

fn write_reply(root: &Path, payload: &Value) -> Value {
    let Some(path_str) = payload.get("path").and_then(Value::as_str) else {
        return json!({"error": "write requires path"});
    };
    let Some(content) = payload.get("content").and_then(Value::as_str) else {
        return json!({"error": "write requires content"});
    };
    let target = match resolve_safe(root, path_str) {
        Ok(p) => p,
        Err(e) => return json!({"error": e}),
    };
    if let Some(parent) = target.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return json!({"error": format!("mkdir parent: {e}")});
        }
    }
    match std::fs::write(&target, content) {
        Ok(_) => json!({"path": path_str, "written": true}),
        Err(e) => json!({"error": format!("write {}: {e}", target.display())}),
    }
}

fn delete_reply(root: &Path, payload: &Value) -> Value {
    let Some(path_str) = payload.get("path").and_then(Value::as_str) else {
        return json!({"error": "delete requires path"});
    };
    let target = match resolve_safe(root, path_str) {
        Ok(p) => p,
        Err(e) => return json!({"error": e}),
    };
    if !target.exists() {
        return json!({"error": format!("path {path_str:?} not found")});
    }
    let result = if target.is_dir() {
        std::fs::remove_dir_all(&target)
    } else {
        std::fs::remove_file(&target)
    };
    match result {
        Ok(_) => json!({"path": path_str, "deleted": true}),
        Err(e) => json!({"error": format!("delete {}: {e}", target.display())}),
    }
}

fn rename_reply(root: &Path, payload: &Value) -> Value {
    let old = payload.get("old_path").and_then(Value::as_str);
    let new = payload.get("new_path").and_then(Value::as_str);
    let (Some(old), Some(new)) = (old, new) else {
        return json!({"error": "rename requires old_path + new_path"});
    };
    let src = match resolve_safe(root, old) {
        Ok(p) => p,
        Err(e) => return json!({"error": e}),
    };
    let dst = match resolve_safe(root, new) {
        Ok(p) => p,
        Err(e) => return json!({"error": e}),
    };
    if let Some(parent) = dst.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return json!({"error": format!("mkdir parent: {e}")});
        }
    }
    match std::fs::rename(&src, &dst) {
        Ok(_) => json!({"old_path": old, "new_path": new}),
        Err(e) => json!({"error": format!("rename: {e}")}),
    }
}

fn mkdir_reply(root: &Path, payload: &Value) -> Value {
    let Some(path_str) = payload.get("path").and_then(Value::as_str) else {
        return json!({"error": "mkdir requires path"});
    };
    let target = match resolve_safe(root, path_str) {
        Ok(p) => p,
        Err(e) => return json!({"error": e}),
    };
    match std::fs::create_dir_all(&target) {
        Ok(_) => json!({"path": path_str, "created": true}),
        Err(e) => json!({"error": format!("mkdir {}: {e}", target.display())}),
    }
}

#[cfg(test)]
mod tests;
