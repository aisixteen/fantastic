//! `export_subtree` against a real in-memory kernel: the folder it writes.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use fantastic_kernel::{
    bootstrap::{bootstrap, BootstrapOptions},
    bundle::BundleError,
    AgentId, Bundle, BundleRegistry, Item, Kernel, Reply,
};
use fantastic_user::{UserBundle, HANDLER_MODULE, VERBS};
use serde_json::{json, Value};

/// Answers with what it was given.
struct Echo;

#[async_trait]
impl Bundle for Echo {
    fn name(&self) -> &str {
        "echo"
    }
    async fn handle(
        &self,
        _id: &AgentId,
        p: &Value,
        _k: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        Ok(Some(json!({"echo": p})))
    }
}

/// The shell: a send from outside every agent.
async fn op(k: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    Arc::clone(k)
        .send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
        .result
}

const KEY: &str = "sk-planted0123456789";
const ROW: &str = "the planted conversation row";
const ABS: &str = "/private/planted/abs/path";

/// root ─ user {memory (a store), exec} · a {b {c}}, with a key, a row and an
/// absolute path planted on the records.
async fn setup() -> Arc<Kernel> {
    let mut reg = BundleRegistry::new();
    reg.register(HANDLER_MODULE, UserBundle::default());
    reg.register("sqlite_store.tools", Echo);
    reg.register("exec_bridge.tools", Echo);
    reg.register("echo.tools", Echo);
    let k = Arc::clone(
        &bootstrap(reg, BootstrapOptions::in_memory())
            .expect("boots")
            .kernel,
    );
    let root = k.root().unwrap().id.0.clone();
    for (id, parent, hm, extra) in [
        ("user", root.as_str(), HANDLER_MODULE, json!({})),
        ("memory", "user", "sqlite_store.tools", json!({})),
        ("exec", "user", "exec_bridge.tools", json!({})),
        (
            "a",
            "user",
            "echo.tools",
            json!({"can_call": {"memory": "*", "b": "*"}, "memory_store_id": "memory",
            "credentials": {"store": "memory", "ref": "r1", "api_key": KEY}, "history": [ROW]}),
        ),
        (
            "b",
            "a",
            "echo.tools",
            json!({"can_call": {"exec": ["run"]}, "callable_by": {"a": "*"}, "cwd": ABS,
            "rows": [{"text": ROW}], "last_reply": ROW}),
        ),
        (
            "c",
            "b",
            "echo.tools",
            json!({"token": KEY, "notes": "plain"}),
        ),
    ] {
        let mut p =
            json!({"type": "create_agent", "parent_id": parent, "id": id, "handler_module": hm});
        p.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let r = op(&k, &root, p.clone()).await;
        assert!(r.get("error").is_none(), "{p}: {r}");
    }
    k
}

fn out_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fu-export-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn every_file(d: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(d).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            every_file(&p, out);
        } else {
            out.push(p);
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn export_subtree_verb_writes_folder_and_reports_ports() {
    let k = setup().await;
    let d = out_dir("verb");
    let r = op(
        &k,
        "user",
        json!({"type": "export_subtree", "agent_id": "a", "path": d.to_str().unwrap()}),
    )
    .await;
    assert_eq!(r["exported"], 3, "{r}");
    assert_eq!(r["ports"], json!(["@exec", "@memory", "@path:cwd"]), "{r}");
    assert_eq!(r["files"], json!(["blueprint.yaml"]), "{r}");
    let text = std::fs::read_to_string(d.join("blueprint.yaml")).unwrap();
    let v: Value = serde_yaml::from_str(&text).unwrap();
    assert_eq!(v["agents"][0]["id"], "a");
    assert_eq!(v["agents"][0]["agents"][0]["agents"][0]["id"], "c");
    assert_eq!(v["agents"][0]["credentials"]["ref"], "r1");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&d), 0o700);
        assert_eq!(mode(&d.join("blueprint.yaml")), 0o600);
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test(flavor = "multi_thread")]
async fn secrets_scan_over_output_folder_finds_nothing() {
    let k = setup().await;
    let d = out_dir("scan");
    let r = op(
        &k,
        "user",
        json!({"type": "export_subtree", "agent_id": "a", "path": d.to_str().unwrap()}),
    )
    .await;
    assert!(r.get("error").is_none(), "{r}");
    let mut files = Vec::new();
    every_file(&d, &mut files);
    assert!(!files.is_empty());
    let mut hits = Vec::new();
    for f in &files {
        let t = std::fs::read_to_string(f).unwrap();
        for planted in [KEY, ROW, ABS, "api_key", "/private"] {
            if t.contains(planted) {
                hits.push(format!("{}: {planted}", f.display()));
            }
        }
    }
    assert!(hits.is_empty(), "{hits:?}");
    assert!(fantastic_user::export::scan_secrets(&d).is_empty());
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test(flavor = "multi_thread")]
async fn export_subtree_refuses_existing_dir_and_dotdot() {
    let k = setup().await;
    let d = out_dir("exists");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("keep.txt"), "mine").unwrap();
    let r = op(
        &k,
        "user",
        json!({"type": "export_subtree", "agent_id": "a", "path": d.to_str().unwrap()}),
    )
    .await;
    assert_eq!(r["reason"], "exists", "{r}");
    assert_eq!(
        std::fs::read_to_string(d.join("keep.txt")).unwrap(),
        "mine",
        "left as it was"
    );
    let r = op(
        &k,
        "user",
        json!({"type": "export_subtree", "agent_id": "a", "path": "x/../../y"}),
    )
    .await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    // an EMPTY folder is taken
    let e = out_dir("empty");
    std::fs::create_dir_all(&e).unwrap();
    let r = op(
        &k,
        "user",
        json!({"type": "export_subtree", "agent_id": "b", "path": e.to_str().unwrap()}),
    )
    .await;
    assert_eq!(r["exported"], 2, "{r}");
    // the root is refused, and nothing is written
    let n = out_dir("root");
    let root = k.root().unwrap().id.0.clone();
    let r = op(
        &k,
        "user",
        json!({"type": "export_subtree", "agent_id": root, "path": n.to_str().unwrap()}),
    )
    .await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert!(!n.exists());
    for x in [d, e] {
        let _ = std::fs::remove_dir_all(x);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn export_subtree_requires_agent_id_and_path() {
    let k = setup().await;
    for p in [
        json!({"type": "export_subtree", "path": "x"}),
        json!({"type": "export_subtree", "agent_id": "a"}),
        json!({"type": "export_subtree", "agent_id": 3, "path": "x"}),
        json!({"type": "export_subtree", "agent_id": "a", "path": ""}),
    ] {
        let r = op(&k, "user", p.clone()).await;
        assert_eq!(r["reason"], "bad_arg", "{p}: {r}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn export_subtree_is_in_verbs_and_docs() {
    assert!(VERBS.contains(&"export_subtree"));
    let k = setup().await;
    let d = op(&k, "kernel", json!({"type": "docs", "of": "user"})).await;
    assert!(d["verbs"].get("export_subtree").is_some(), "{d}");
}

// ---- OWNED FILES, and the same engine on the root (#268) ----

/// A disk kernel at a fresh workdir whose root is the real `kernel_state`
/// loader: root ─ user · a (a folder-bundle agent with an html view) {b {c}}.
async fn disk_setup(tag: &str, agent_py: &str) -> (Arc<Kernel>, PathBuf) {
    let work = out_dir(tag);
    std::fs::create_dir_all(&work).unwrap();
    let mut reg = BundleRegistry::new();
    reg.register(
        fantastic_core::HANDLER_MODULE,
        fantastic_core::KernelStateBundle::default(),
    );
    reg.register(HANDLER_MODULE, UserBundle::default());
    reg.register("py_ab12", Echo);
    reg.register("echo.tools", Echo);
    let k = Arc::clone(
        &bootstrap(reg, BootstrapOptions::one_shot(&work))
            .expect("boots")
            .kernel,
    );
    let root = k.root().unwrap().id.0.clone();
    for (id, parent, hm, extra) in [
        ("user", root.as_str(), HANDLER_MODULE, json!({})),
        (
            "a",
            "user",
            "py_ab12",
            json!({"view": {"kind": "html", "bytes": "<p>hi</p>", "calls": ["a"]}}),
        ),
        ("b", "a", "py_ab12", json!({})),
        ("c", "b", "echo.tools", json!({})),
    ] {
        let mut p =
            json!({"type": "create_agent", "parent_id": parent, "id": id, "handler_module": hm});
        p.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let r = op(&k, &root, p.clone()).await;
        assert!(r.get("error").is_none(), "{p}: {r}");
    }
    let b = work.join(".fantastic/bundles/py_ab12");
    std::fs::create_dir_all(&b).unwrap();
    std::fs::write(b.join("bundle.yaml"), "base: py_agent\nsentence: x\n").unwrap();
    std::fs::write(b.join("agent.py"), agent_py).unwrap();
    std::fs::write(b.join("panel.html"), "<p>panel</p>").unwrap();
    (k, work)
}

#[tokio::test(flavor = "multi_thread")]
async fn export_subtree_writes_view_and_bundle_files_for_a_folder_bundle_agent() {
    let (k, work) = disk_setup("owned", "print('hi')\n").await;
    let r = op(
        &k,
        "user",
        json!({"type": "export_subtree", "agent_id": "a", "path": "out_owned"}),
    )
    .await;
    assert_eq!(r["exported"], 3, "{r}");
    assert_eq!(
        r["files"],
        json!([
            "blueprint.yaml",
            "a/view.html",
            "bundles/py_ab12/agent.py",
            "bundles/py_ab12/bundle.yaml",
            "bundles/py_ab12/panel.html"
        ]),
        "{r}"
    );
    let d = work.join("out_owned");
    assert_eq!(
        std::fs::read_to_string(d.join("a/view.html")).unwrap(),
        "<p>hi</p>"
    );
    assert_eq!(
        std::fs::read_to_string(d.join("bundles/py_ab12/agent.py")).unwrap(),
        "print('hi')\n"
    );
    let text = std::fs::read_to_string(d.join("blueprint.yaml")).unwrap();
    assert!(text.contains("# code: a/view.html\n"), "{text}");
    assert!(
        text.contains("# code: bundles/py_ab12/panel.html\n"),
        "{text}"
    );
    let v: Value = serde_yaml::from_str(&text).unwrap();
    assert_eq!(v["agents"][0]["view"]["bytes_file"], "a/view.html");
    assert!(v["agents"][0]["view"].get("bytes").is_none(), "{text}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        for f in [
            "a/view.html",
            "bundles/py_ab12/agent.py",
            "bundles/py_ab12/panel.html",
        ] {
            assert_eq!(mode(&d.join(f)), 0o600, "{f}");
        }
        assert_eq!(mode(&d.join("bundles/py_ab12")), 0o700);
    }
    let _ = std::fs::remove_dir_all(&work);
}

#[tokio::test(flavor = "multi_thread")]
async fn export_subtree_refuses_with_secret_found_when_agent_py_holds_a_key() {
    let (k, work) = disk_setup("keyed", "KEY = 'sk-abcd1234efgh'\n").await;
    let r = op(
        &k,
        "user",
        json!({"type": "export_subtree", "agent_id": "a", "path": "out_keyed"}),
    )
    .await;
    assert_eq!(r["reason"], "secret_found", "{r}");
    assert!(
        r["error"]
            .as_str()
            .unwrap()
            .contains("bundles/py_ab12/agent.py"),
        "{r}"
    );
    assert!(!work.join("out_keyed").exists());
    let _ = std::fs::remove_dir_all(&work);
}

#[tokio::test(flavor = "multi_thread")]
async fn user_and_root_export_the_same_folder() {
    let (k, work) = disk_setup("same", "print('hi')\n").await;
    let root = k.root().unwrap().id.0.clone();
    let u = op(
        &k,
        "user",
        json!({"type": "export_subtree", "agent_id": "a", "path": "by_user"}),
    )
    .await;
    let r = op(
        &k,
        &root,
        json!({"type": "export_subtree", "agent_id": "a", "path": "by_root"}),
    )
    .await;
    assert_eq!(u["exported"], r["exported"], "{u} {r}");
    assert_eq!(u["files"], r["files"], "{u} {r}");
    assert_eq!(u["ports"], r["ports"], "{u} {r}");
    for f in u["files"].as_array().unwrap() {
        let f = f.as_str().unwrap();
        assert_eq!(
            std::fs::read(work.join("by_user").join(f)).unwrap(),
            std::fs::read(work.join("by_root").join(f)).unwrap(),
            "{f}"
        );
    }
    let _ = std::fs::remove_dir_all(&work);
}
