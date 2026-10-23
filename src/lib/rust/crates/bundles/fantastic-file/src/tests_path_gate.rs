//! Binary-channel path gate, per verb (#155). Every path-carrying verb sent over
//! the BINARY channel with a path outside the exposed view is refused, and nothing
//! reaches the disk.

use super::*;
use fantastic_kernel::{Agent, Item};
use serde_json::json;

/// A bridge `fb` rooted at `tmp/served`, exposing only `sub/`. Outside the view:
/// `other.txt` (a file) and `priv/p.txt` (a directory). `extra` is merged into
/// the create_agent payload (e.g. `readonly`).
async fn bridge(tmp: &tempfile::TempDir, extra: Value) -> Arc<Kernel> {
    let served = tmp.path().join("served");
    std::fs::create_dir_all(served.join("sub")).unwrap();
    std::fs::write(served.join("sub/in.txt"), "old\n").unwrap();
    std::fs::write(served.join("other.txt"), "secret\n").unwrap();
    std::fs::create_dir_all(served.join("priv")).unwrap();
    std::fs::write(served.join("priv/p.txt"), "p\n").unwrap();
    let mut kernel = Kernel::new();
    kernel
        .bundles
        .register(HANDLER_MODULE, FileBundle::default());
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        serde_json::Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(root);
    let mut cfg = json!({
        "type": "create_agent",
        "parent_id": "kernel",
        "handler_module": HANDLER_MODULE,
        "id": "fb",
        "root": served.display().to_string(),
        "ingress_rule": "allow_all_ingress",
        "allow": ["sub"],
    });
    cfg.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    let r = kernel
        .send(vec![Item::new(AgentId::from("kernel_state"), cfg)])
        .await
        .remove(0)
        .result;
    assert_eq!(r["id"], "fb", "{r}");
    kernel
}

/// A verb over the BINARY channel: the header plus the body at `bytes`.
async fn bin(k: &Arc<Kernel>, header: Value, blob: &[u8]) -> Value {
    k.send(vec![
        Item::new(AgentId::from("fb"), header).with_binary_key("bytes", blob.to_vec())
    ])
    .await
    .remove(0)
    .result
}

/// The same verb over the TEXT channel.
async fn txt(k: &Arc<Kernel>, header: Value) -> Value {
    k.send(vec![Item::new(AgentId::from("fb"), header)])
        .await
        .remove(0)
        .result
}

fn refused(r: &Value) {
    assert!(r.get("error").is_some(), "expected a refusal, got {r}");
}

fn served(tmp: &tempfile::TempDir, rel: &str) -> std::path::PathBuf {
    tmp.path().join("served").join(rel)
}

#[tokio::test]
async fn list_outside_is_refused() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = bridge(&tmp, json!({})).await;
    refused(&bin(&k, json!({"type": "list", "path": "priv"}), b"").await);
}

#[tokio::test]
async fn read_outside_is_refused() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = bridge(&tmp, json!({})).await;
    refused(&bin(&k, json!({"type": "read", "path": "other.txt"}), b"").await);
}

#[tokio::test]
async fn read_stream_outside_is_refused() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = bridge(&tmp, json!({})).await;
    refused(&bin(&k, json!({"type": "read_stream", "path": "other.txt"}), b"").await);
}

#[tokio::test]
async fn watch_path_outside_is_refused() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = bridge(&tmp, json!({})).await;
    refused(&bin(&k, json!({"type": "watch_path", "path": "other.txt"}), b"").await);
}

#[tokio::test]
async fn write_outside_is_refused_and_creates_nothing() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = bridge(&tmp, json!({})).await;
    refused(&bin(&k, json!({"type": "write", "path": "new.txt"}), b"x").await);
    assert!(!served(&tmp, "new.txt").exists());
}

#[tokio::test]
async fn append_outside_is_refused() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = bridge(&tmp, json!({})).await;
    refused(&bin(&k, json!({"type": "append", "path": "other.txt"}), b"x").await);
    assert_eq!(
        std::fs::read(served(&tmp, "other.txt")).unwrap(),
        b"secret\n"
    );
}

#[tokio::test]
async fn write_stream_outside_is_refused() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = bridge(&tmp, json!({})).await;
    refused(&bin(&k, json!({"type": "write_stream", "path": "new.bin"}), b"x").await);
    assert!(!served(&tmp, "new.bin").exists());
}

#[tokio::test]
async fn delete_outside_is_refused_and_the_file_stays() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = bridge(&tmp, json!({})).await;
    refused(&bin(&k, json!({"type": "delete", "path": "other.txt"}), b"").await);
    assert!(served(&tmp, "other.txt").exists());
}

#[tokio::test]
async fn mkdir_outside_is_refused() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = bridge(&tmp, json!({})).await;
    refused(&bin(&k, json!({"type": "mkdir", "path": "newdir"}), b"").await);
    assert!(!served(&tmp, "newdir").exists());
}

#[tokio::test]
async fn undo_outside_is_refused() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = bridge(&tmp, json!({})).await;
    refused(&bin(&k, json!({"type": "undo", "path": "other.txt"}), b"").await);
    assert_eq!(
        std::fs::read(served(&tmp, "other.txt")).unwrap(),
        b"secret\n"
    );
}

#[tokio::test]
async fn edit_with_one_path_outside_writes_nothing() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = bridge(&tmp, json!({})).await;
    let patch = "<<<<<<< SEARCH sub/in.txt\nold\n=======\nnew\n>>>>>>> REPLACE\n\
                 <<<<<<< SEARCH other.txt\nsecret\n=======\nchanged\n>>>>>>> REPLACE\n";
    let header = json!({"type": "edit", "edits": [{"path": "sub/in.txt"}, {"path": "other.txt"}]});
    refused(&bin(&k, header, patch.as_bytes()).await);
    assert_eq!(std::fs::read(served(&tmp, "sub/in.txt")).unwrap(), b"old\n");
    assert_eq!(
        std::fs::read(served(&tmp, "other.txt")).unwrap(),
        b"secret\n"
    );
}

#[tokio::test]
async fn rename_out_of_the_view_is_refused() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = bridge(&tmp, json!({})).await;
    let header = json!({"type": "rename", "old_path": "other.txt", "new_path": "sub/moved.txt"});
    refused(&bin(&k, header, b"").await);
    assert!(served(&tmp, "other.txt").exists());
    assert!(!served(&tmp, "sub/moved.txt").exists());
}

#[tokio::test]
async fn rename_into_the_view_from_outside_is_refused() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = bridge(&tmp, json!({})).await;
    let header =
        json!({"type": "rename", "old_path": "priv/p.txt", "new_path": "sub/imported.txt"});
    refused(&bin(&k, header, b"").await);
    assert!(served(&tmp, "priv/p.txt").exists());
    assert!(!served(&tmp, "sub/imported.txt").exists());
}

#[tokio::test]
async fn dotdot_and_absolute_paths_are_refused() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = bridge(&tmp, json!({})).await;
    refused(&bin(&k, json!({"type": "read", "path": "../other.txt"}), b"").await);
    refused(&bin(&k, json!({"type": "read", "path": "/etc/passwd"}), b"").await);
    refused(&bin(&k, json!({"type": "write", "path": "../escape.txt"}), b"x").await);
}

#[tokio::test]
async fn inside_paths_work_for_every_verb() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = bridge(&tmp, json!({})).await;
    let r = bin(&k, json!({"type": "read", "path": "sub/in.txt"}), b"").await;
    assert!(r.get("error").is_none(), "{r}");
    let w = bin(&k, json!({"type": "write", "path": "sub/in.txt"}), b"new\n").await;
    assert!(w.get("error").is_none(), "{w}");
    assert_eq!(std::fs::read(served(&tmp, "sub/in.txt")).unwrap(), b"new\n");
}

#[tokio::test]
async fn a_readonly_view_refuses_writes_inside() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = bridge(&tmp, json!({"readonly": true})).await;
    refused(&bin(&k, json!({"type": "write", "path": "sub/in.txt"}), b"new\n").await);
    assert_eq!(std::fs::read(served(&tmp, "sub/in.txt")).unwrap(), b"old\n");
}

#[tokio::test]
async fn binary_and_json_channels_give_the_same_refusal() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = bridge(&tmp, json!({})).await;
    let header = json!({"type": "read", "path": "other.txt"});
    let b = bin(&k, header.clone(), b"").await;
    let t = txt(&k, header).await;
    assert_eq!(b, t);
}
