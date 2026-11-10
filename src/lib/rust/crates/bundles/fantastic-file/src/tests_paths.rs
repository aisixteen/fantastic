//! Issue #305: a refused or not-found path names the root and the relative form.

use super::*;
use fantastic_kernel::{Agent, Item};
use serde_json::json;

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

/// A disk kernel whose state folder is `tmp/s` and work folder `tmp/w`, with a
/// bridge `b_<n>` per `(name, root)`.
async fn two_folder_kernel(tmp: &tempfile::TempDir, roots: &[(&str, &str)]) -> Arc<Kernel> {
    let (s, w) = (tmp.path().join("s"), tmp.path().join("w"));
    std::fs::create_dir_all(&s).unwrap();
    std::fs::create_dir_all(&w).unwrap();
    let mut kernel = Kernel::with_storage(fantastic_kernel::StorageMode::Disk {
        state_dir: s.clone(),
        work_dir: w,
    });
    kernel
        .bundles
        .register(HANDLER_MODULE, FileBundle::default());
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        s,
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(root);
    for (name, root) in roots {
        let r = kernel
            .send(vec![Item::new(
                AgentId::from("kernel_state"),
                json!({"type": "create_agent", "parent_id": "kernel",
                       "handler_module": HANDLER_MODULE, "id": name, "root": root,
                       "ingress_rule": "allow_all_ingress", "allow": ["."]}),
            )])
            .await
            .remove(0)
            .result;
        assert_eq!(r["id"], *name, "{r}");
    }
    kernel
}

async fn ask(kernel: &Arc<Kernel>, to: &str, payload: Value) -> Value {
    kernel
        .send(vec![Item::new(AgentId::from(to), payload)])
        .await
        .remove(0)
        .result
}

fn first_line(r: &Value) -> Option<String> {
    r["lines"][0]["text"].as_str().map(str::to_string)
}

#[tokio::test]
async fn state_self_and_work_roots_resolve_by_the_rule() {
    let tmp = tempfile::TempDir::new().unwrap();
    let kernel = two_folder_kernel(
        &tmp,
        &[("in_state", "@state"), ("in_work", "."), ("mine", "@self")],
    )
    .await;
    std::fs::write(tmp.path().join("s/a.txt"), "state side\n").unwrap();
    std::fs::write(tmp.path().join("w/b.txt"), "work side\n").unwrap();
    let own = kernel
        .agents
        .get(&AgentId::from("mine"))
        .unwrap()
        .root_path
        .clone();
    std::fs::create_dir_all(&own).unwrap();
    std::fs::write(own.join("x.txt"), "own side\n").unwrap();

    let r = ask(
        &kernel,
        "in_state",
        json!({"type": "read", "path": "a.txt"}),
    )
    .await;
    assert_eq!(first_line(&r).as_deref(), Some("state side"), "{r}");
    let r = ask(&kernel, "in_work", json!({"type": "read", "path": "b.txt"})).await;
    assert_eq!(first_line(&r).as_deref(), Some("work side"), "{r}");
    let r = ask(&kernel, "in_work", json!({"type": "read", "path": "a.txt"})).await;
    assert!(
        first_line(&r).is_none(),
        "the work folder does not hold a.txt: {r}"
    );
    let r = ask(&kernel, "mine", json!({"type": "read", "path": "x.txt"})).await;
    assert_eq!(first_line(&r).as_deref(), Some("own side"), "{r}");

    let reflect = ask(&kernel, "in_state", json!({"type": "reflect"})).await;
    assert_eq!(
        reflect["root"],
        json!(fantastic_bundle::realpath(&tmp.path().join("s"))
            .unwrap()
            .display()
            .to_string())
    );
}

#[tokio::test]
async fn a_state_root_cannot_escape_the_state_folder() {
    let tmp = tempfile::TempDir::new().unwrap();
    let kernel = two_folder_kernel(&tmp, &[("out", "@state/../w")]).await;
    let reflect = ask(&kernel, "out", json!({"type": "reflect"})).await;
    let why = reflect["root_error"].as_str().unwrap_or_default();
    assert!(why.contains("escapes"), "{reflect}");
}
