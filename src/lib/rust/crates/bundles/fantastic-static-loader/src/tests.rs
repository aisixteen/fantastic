//! static_loader — declarative YAML roster hydration + the deny-all closure.
//! Mirror of the python `static_loader/tests/test_static_loader.py`.

use super::*;
use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::test_support::as_agent;
use fantastic_kernel::{Agent, AgentRecord, Item};
use serde_json::{json, Map};

/// A trivial roster agent — answers boot/reflect, nothing else.
struct MockBundle;

#[async_trait]
impl Bundle for MockBundle {
    fn name(&self) -> &str {
        "mock"
    }
    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        Ok(Some(match verb {
            "boot" | "shutdown" => Value::Null,
            "reflect" => json!({"id": agent_id.0, "sentence": "mock"}),
            _ => json!({"ok": true}),
        }))
    }
}

const MANIFEST: &str = r#"
agents:
  - id: memory
    handler_module: mock.tools
    mode: mem
  - id: worker
    handler_module: mock.tools
    can_call:
      memory: [set, read]
"#;

fn mk_kernel() -> Arc<Kernel> {
    let mut kernel = Kernel::new();
    kernel.bundles.register("mock.tools", MockBundle);
    kernel.bundles.register(HANDLER_MODULE, StaticLoaderBundle);
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        // the running dir is the cwd — manifests are written under it
        std::env::current_dir().unwrap().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    kernel
}

/// A manifest file inside a temp dir UNDER the running dir, and the guard that
/// deletes it.
///
/// Inside, because the manifest read obeys the RUNNING-DIR LAW: `fs` is the one
/// disk surface and a cold bring-up read is not exempt, so a manifest under
/// `/tmp` is refused. Python's tests get the same effect by `chdir`-ing into
/// tmp_path (safe there — xdist forks a process per worker); rust test threads
/// share one cwd, so the file comes to the cwd instead of the cwd going to it.
///
/// The TempDir is RETURNED, never `forget`-ten: a leaked fixture is a file that
/// reappears on every run and lands in `git status` forever.
fn write_manifest(text: &str) -> (tempfile::TempDir, String) {
    let dir = tempfile::Builder::new()
        .prefix(".sl_fixture_")
        .tempdir_in(std::env::current_dir().unwrap())
        .unwrap();
    let path = dir.path().join("fantastic.yaml");
    std::fs::write(&path, text).unwrap();
    let rel = path
        .strip_prefix(std::env::current_dir().unwrap())
        .unwrap_or(&path)
        .to_string_lossy()
        .to_string();
    (dir, rel)
}

/// Returns the TempDir guard alongside the kernel: the fixture is deleted when
/// the caller drops it, so a test run leaves the tree exactly as it found it.
async fn boot_with_manifest(text: &str) -> (Arc<Kernel>, tempfile::TempDir) {
    let kernel = mk_kernel();
    let (fixture, path) = write_manifest(text);
    // Operator send (top-level → ungated); create_agent auto-boots the loader,
    // which hydrates the roster.
    let rec = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({
                "type": "create_agent", "parent_id": "kernel",
                "handler_module": HANDLER_MODULE,
                "id": "static_loader",
                "manifest": path,
            }),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(rec["id"], json!("static_loader"), "loader create: {rec}");
    (kernel, fixture)
}

fn record(kernel: &Arc<Kernel>, id: &str) -> AgentRecord {
    kernel.agents.get(&AgentId::from(id)).unwrap().record()
}

#[tokio::test]
async fn roster_is_hydrated_as_children_of_the_loader() {
    let (kernel, _fixture) = boot_with_manifest(MANIFEST).await;
    assert!(kernel.agents.contains_key(&AgentId::from("memory")));
    assert!(kernel.agents.contains_key(&AgentId::from("worker")));
    for id in ["memory", "worker"] {
        assert_eq!(
            record(&kernel, id).parent_id.as_deref(),
            Some("static_loader")
        );
    }
}

#[tokio::test]
async fn declared_agents_carry_baseline_plus_explicit_grants() {
    let (kernel, _fixture) = boot_with_manifest(MANIFEST).await;
    let can = record(&kernel, "worker").meta["can_call"].clone();
    // the baseline is `$self` alone — every other edge is explicit
    assert_eq!(can["$self"], json!("*"));
    assert!(
        can.get("$parent").is_none() && can.get("$subtree").is_none(),
        "{can}"
    );
    assert_eq!(can["memory"], json!(["set", "read"]));
    // memory got the baseline with no explicit can_call in the manifest.
    assert_eq!(
        record(&kernel, "memory").meta["can_call"]["$self"],
        json!("*")
    );
}

#[tokio::test]
async fn roster_is_closed_a_child_cannot_create_more() {
    let (kernel, _fixture) = boot_with_manifest(MANIFEST).await;
    let r = as_agent(
        AgentId::from("worker"),
        kernel.send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "create_agent", "parent_id": "kernel", "handler_module": "mock.tools"}),
        )]),
    )
    .await
    .remove(0)
    .result;
    assert_eq!(r["reason"], json!("unauthorized"), "{r}");
}

#[tokio::test]
async fn boot_is_idempotent_by_id() {
    let (kernel, _fixture) = boot_with_manifest(MANIFEST).await;
    let before = kernel.agents.len();
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("static_loader"),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["booted"], json!(true), "{r}");
    let reapplied: Vec<String> = r["reapplied"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(reapplied.contains(&"memory".to_string()) && reapplied.contains(&"worker".to_string()));
    assert_eq!(r["created"], json!([]));
    assert_eq!(kernel.agents.len(), before);
}

/// Create a static_loader under the root with `manifest`; returns the create reply.
async fn create_loader(kernel: &Arc<Kernel>, id: &str, manifest: Value) -> Value {
    kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({
                "type": "create_agent", "parent_id": "kernel", "handler_module": HANDLER_MODULE,
                "id": id, "manifest": manifest,
            }),
        )])
        .await
        .remove(0)
        .result
}

async fn boot_loader(kernel: &Arc<Kernel>, id: &str) -> Value {
    kernel
        .send(vec![Item::new(AgentId::from(id), json!({"type": "boot"}))])
        .await
        .remove(0)
        .result
}

#[tokio::test]
async fn the_manifest_shape_is_judged_at_write_time() {
    // `validate_meta` is SHAPE-ONLY: a named manifest must be a non-empty path
    // string. Readability is boot's question (it reads through the workdir).
    let kernel = mk_kernel();
    for (i, bad) in [json!(""), json!(7), json!(["a.yaml"]), json!(null)]
        .into_iter()
        .enumerate()
    {
        let id = format!("sl_shape{i}");
        let rec = create_loader(&kernel, &id, bad.clone()).await;
        assert_eq!(rec["reason"], "invalid_meta", "{bad}: {rec}");
        assert_eq!(
            rec["error"], "static_loader: manifest must be a non-empty path string",
            "{bad}"
        );
        assert!(
            !kernel.agents.contains_key(&AgentId::from(id.as_str())),
            "a refused create must leave no agent behind"
        );
    }
}

#[tokio::test]
async fn an_unreadable_manifest_is_reported_at_boot() {
    // The write is shape-only, so the record lands; `boot` names the resolved
    // path in python's OSError text and boots nothing.
    let kernel = mk_kernel();
    let rec = create_loader(&kernel, "sl2", json!("nope.yaml")).await;
    assert!(rec.get("error").is_none(), "{rec}");
    let r = boot_loader(&kernel, "sl2").await;
    let abs = fantastic_bundle::realpath(&std::env::current_dir().unwrap())
        .unwrap()
        .join("nope.yaml");
    assert_eq!(
        r,
        json!({
            "error": format!(
                "static_loader: cannot read manifest 'nope.yaml': \
                 [Errno 2] No such file or directory: '{}'",
                abs.display()
            ),
            "booted": false,
        })
    );
}

#[tokio::test]
async fn a_roster_entry_without_an_id_is_refused_loud() {
    // #105 — the roster is keyed by id, so an id-less entry can never match the
    // agent the LAST boot minted: every reboot created ANOTHER one beside it and
    // the flush persisted them all, with no word to the operator.
    let (kernel, _fixture) = boot_with_manifest("agents:\n  - handler_module: mock.tools\n").await;
    let before = kernel.agents.len();
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("static_loader"),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["booted"], json!(false), "{r}");
    let err = r["error"].as_str().unwrap_or_default();
    assert!(err.contains("explicit id"), "{err}");
    assert!(err.contains("mock.tools"), "the entry is named: {err}");
    // NOTHING was created — not on this boot, and not by the create-time boot.
    assert_eq!(kernel.agents.len(), before);
    assert_eq!(before, 2, "just the root + the loader itself");
}

#[tokio::test]
async fn an_id_less_entry_is_refused_before_the_valid_ones_hydrate() {
    // The refusal is a REFUSAL, not a skip: boot stops at the bad entry.
    let (kernel, _fixture) = boot_with_manifest(
        "agents:\n  - handler_module: mock.tools\n  - id: memory\n    handler_module: mock.tools\n",
    )
    .await;
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("static_loader"),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["booted"], json!(false), "{r}");
    assert!(!kernel.agents.contains_key(&AgentId::from("memory")));
}

// ─── the py-parity sweep (plan A8) ───────────────────────────────────

/// A loader agent built directly, so a record this bundle's `validate_meta`
/// would refuse can still be reflected/booted (python has no such gate on the
/// in-memory record either).
fn loader_with_meta(meta: Map<String, Value>) -> Arc<Agent> {
    Agent::new(
        AgentId::from("sl"),
        Some(HANDLER_MODULE.to_string()),
        None,
        meta,
        std::path::PathBuf::new(),
        false,
    )
}

fn meta_of(pairs: &[(&str, Value)]) -> Map<String, Value> {
    let mut m = Map::new();
    for (k, v) in pairs {
        m.insert((*k).to_string(), v.clone());
    }
    m
}

#[tokio::test]
async fn an_unknown_verb_is_a_coded_refusal() {
    // `shutdown` is not a verb of this bundle either.
    let (kernel, _fixture) = boot_with_manifest(MANIFEST).await;
    for verb in ["frobnicate", "shutdown"] {
        let r = kernel
            .send(vec![Item::new(
                AgentId::from("static_loader"),
                json!({"type": verb}),
            )])
            .await
            .remove(0)
            .result;
        assert_eq!(
            r,
            json!({
                "error": format!("static_loader: unknown type '{verb}'"),
                "reason": "unhandled",
                "verb": verb,
            }),
            "{verb}"
        );
    }
}

#[tokio::test]
async fn a_record_without_a_manifest_boots_with_pythons_text() {
    // A loader may exist before its roster does; `boot` says so in
    // python's words rather than the record being unwritable.
    let kernel = mk_kernel();
    let rec = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({
                "type": "create_agent", "parent_id": "kernel",
                "handler_module": HANDLER_MODULE, "id": "sl4",
            }),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(rec["id"], json!("sl4"), "{rec}");
    let b = kernel
        .send(vec![Item::new(
            AgentId::from("sl4"),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(
        b,
        json!({
            "error": "static_loader: no 'manifest' path on the record",
            "booted": false,
        })
    );
}

#[test]
fn an_empty_manifest_string_is_no_manifest() {
    // Python tests `isinstance(path, str) and path`.
    let agent = loader_with_meta(meta_of(&[("manifest", json!(""))]));
    assert_eq!(
        load_manifest(&agent, &std::env::current_dir().unwrap()).unwrap_err(),
        "static_loader: no 'manifest' path on the record"
    );
}

#[tokio::test]
async fn a_manifest_outside_the_running_dir_is_refused_at_boot() {
    // The cold bring-up read is not exempt from the running-dir law, and
    // the tail is python's own `fs` ValueError.
    let kernel = mk_kernel();
    let rec = create_loader(&kernel, "sl3", json!("/etc/hosts")).await;
    assert!(rec.get("error").is_none(), "{rec}");
    assert_eq!(
        boot_loader(&kernel, "sl3").await["error"],
        "static_loader: cannot read manifest '/etc/hosts': fs: path '/etc/hosts' escapes root"
    );
}

#[test]
fn an_absolute_manifest_inside_cwd_is_refused() {
    // `fs.resolve` refuses ANY leading '/', re-rooting nothing.
    let (fixture, rel) = write_manifest(MANIFEST);
    let abs = std::env::current_dir().unwrap().join(&rel);
    let agent = loader_with_meta(meta_of(&[("manifest", json!(abs.display().to_string()))]));
    assert_eq!(
        load_manifest(&agent, &std::env::current_dir().unwrap()).unwrap_err(),
        format!(
            "static_loader: cannot read manifest '{}': fs: path '{}' escapes root",
            abs.display(),
            abs.display()
        )
    );
    drop(fixture);
}

#[test]
fn a_symlinked_manifest_escaping_cwd_is_refused() {
    // The clamp resolves symlinks, and the escape is reported
    // under the `cannot read manifest` prefix.
    let (fixture, rel) = write_manifest(MANIFEST);
    let link = std::path::Path::new(&rel).with_file_name("link.yaml");
    std::os::unix::fs::symlink("/etc/hosts", &link).unwrap();
    let rel_link = link.display().to_string();
    let agent = loader_with_meta(meta_of(&[("manifest", json!(rel_link.clone()))]));
    assert_eq!(
        load_manifest(&agent, &std::env::current_dir().unwrap()).unwrap_err(),
        format!(
            "static_loader: cannot read manifest '{rel_link}': fs: path '{rel_link}' escapes root"
        )
    );
    drop(fixture);
}

#[test]
fn a_falsy_yaml_document_boots_empty() {
    // `yaml.safe_load(text) or {}`.
    for doc in ["", "~", "0", "false", "[]", "{}"] {
        let (fixture, rel) = write_manifest(doc);
        let agent = loader_with_meta(meta_of(&[("manifest", json!(rel))]));
        assert_eq!(
            load_manifest(&agent, &std::env::current_dir().unwrap()).unwrap(),
            Vec::<Value>::new(),
            "{doc:?}"
        );
        drop(fixture);
    }
    let (fixture, rel) = write_manifest("[1]");
    let agent = loader_with_meta(meta_of(&[("manifest", json!(rel.clone()))]));
    assert_eq!(
        load_manifest(&agent, &std::env::current_dir().unwrap()).unwrap_err(),
        format!("static_loader: manifest '{rel}' must be a mapping with `agents:`")
    );
    drop(fixture);
}

#[tokio::test]
async fn handler_module_absent_or_falsy_is_refused_truthy_passes() {
    // Python tests `not spec.get("handler_module")`.
    for entry in [
        "  - id: a\n",
        "  - id: a\n    handler_module: ''\n",
        "  - id: a\n    handler_module: 0\n",
    ] {
        let (kernel, _fixture) = boot_with_manifest(&format!("agents:\n{entry}")).await;
        let r = kernel
            .send(vec![Item::new(
                AgentId::from("static_loader"),
                json!({"type": "boot"}),
            )])
            .await
            .remove(0)
            .result;
        assert_eq!(r["booted"], json!(false), "{entry}");
        let want_spec = if entry.contains("handler_module: ''") {
            "{'id': 'a', 'handler_module': ''}"
        } else if entry.contains("handler_module: 0") {
            "{'id': 'a', 'handler_module': 0}"
        } else {
            "{'id': 'a'}"
        };
        assert_eq!(
            r["error"],
            json!(format!(
                "static_loader: each roster entry needs a handler_module: {want_spec}"
            )),
            "{entry}"
        );
    }
    // A truthy non-string passes this check and is judged by `create_agent`.
    let (kernel, _fixture) =
        boot_with_manifest("agents:\n  - id: a\n    handler_module: 5\n").await;
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("static_loader"),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["booted"], json!(false), "{r}");
    assert!(
        r["error"]
            .as_str()
            .unwrap()
            .starts_with("static_loader: create failed for {'id': 'a', 'handler_module': 5}: "),
        "{r}"
    );
}

#[tokio::test]
async fn a_spec_type_key_overrides_the_create_verb() {
    // Python spreads the spec OVER the verb, so an entry naming `type`
    // decides what is sent. Here the manifest sends `reflect` to the kernel: no
    // agent is created, and the loader reports the reflect's own `id` as
    // "created" — python's behaviour, root id apart.
    let (kernel, _fixture) = boot_with_manifest(
        "agents:\n  - id: a\n    handler_module: mock.tools\n    type: reflect\n",
    )
    .await;
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("static_loader"),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["booted"], json!(true), "{r}");
    assert_eq!(r["created"], json!(["kernel_state"]), "{r}");
    assert!(!kernel.agents.contains_key(&AgentId::from("a")), "{r}");
}

#[test]
fn reflect_display_name_empty_falls_back_to_id() {
    // Python `agent.display_name or id`.
    let agent = loader_with_meta(meta_of(&[("display_name", json!(""))]));
    assert_eq!(reflect_reply(&agent)["display_name"], json!("sl"));
    let agent = loader_with_meta(meta_of(&[("display_name", json!("Roster"))]));
    assert_eq!(reflect_reply(&agent)["display_name"], json!("Roster"));
}

#[test]
fn reflect_manifest_is_the_raw_meta_value() {
    // Python reports `agent.record.get("manifest")` as written.
    let agent = loader_with_meta(meta_of(&[("manifest", json!(7))]));
    assert_eq!(reflect_reply(&agent)["manifest"], json!(7));
    let agent = loader_with_meta(Map::new());
    assert_eq!(reflect_reply(&agent)["manifest"], Value::Null);
}

#[test]
fn verb_docs_are_python_docstrings() {
    // The first line of each python docstring, in VERBS order.
    assert_eq!(
        StaticLoaderBundle.verb_docs(),
        json!({
            "reflect": "The loader's posture: what manifest it hydrates, and that it IS a loader",
            "boot": "Hydrate the declared roster — kernel bookkeeping at boot.",
        })
    );
}

#[test]
fn readme_is_python_verbatim() {
    // Byte-for-byte the python bundle's readme.
    let py = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(4)
        .expect("repo's src/lib above the crate")
        .join("python/bundled_agents/loader/static_loader/src/static_loader/readme.md");
    assert_eq!(README, std::fs::read_to_string(&py).expect("python readme"));
}

#[tokio::test]
async fn a_manifest_that_reassigns_a_handler_module_fails_loud() {
    // Python's text, with `{!r}` on all three names (no `Some(...)`).
    let (kernel, fixture) = boot_with_manifest(MANIFEST).await;
    drop(fixture);
    let (fixture, path) =
        write_manifest("agents:\n  - id: memory\n    handler_module: static_loader.tools\n");
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "update_agent", "id": "static_loader", "manifest": path}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["updated"], json!(true), "{r}");
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("static_loader"),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(
        r,
        json!({
            "error": "static_loader: 'memory' exists as 'mock.tools' but the manifest \
                      declares 'static_loader.tools' — a handler_module is fixed at create; \
                      delete the agent to re-declare it",
            "booted": false,
        })
    );
    drop(fixture);
}

#[tokio::test]
async fn a_refused_re_apply_names_the_agent_and_forwards_the_error() {
    // Only a TRUTHY error is a failure, and the tail is `str(error)`.
    // (The kernel's own refusal text is pinned by C19/C26/C27, not here.)
    let (kernel, fixture) = boot_with_manifest(MANIFEST).await;
    drop(fixture);
    let (_fixture, path) = write_manifest(
        "agents:\n  - id: memory\n    handler_module: mock.tools\n    handler_module_x: 1\n    id_2: 2\n    display_name: ''\n    callable_by: 5\n",
    );
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "update_agent", "id": "static_loader", "manifest": path}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["updated"], json!(true), "{r}");
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("static_loader"),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["booted"], json!(false), "{r}");
    assert!(
        r["error"]
            .as_str()
            .unwrap()
            .starts_with("static_loader: re-applying 'memory': "),
        "{r}"
    );
}

// The manifest reader is the kernel's one YAML reader (`yaml_io::load`,
// PyYAML's `CSafeLoader`); its python vectors live with it
// (`fantastic-kernel/src/yaml_io/read_tests.rs`).

/// A NON-SCALAR mapping key is PyYAML's `ConstructorError ... found unhashable
/// key`: the manifest is refused as invalid YAML, never read as a mapping with
/// a stringified key (the old yaml-rust2 reader accepted `? [a]\n: 1`).
#[test]
fn an_unhashable_mapping_key_is_invalid_yaml() {
    for doc in ["? [a]\n: 1", "? {a: 1}\n: 1\n", "[a]: 1\n"] {
        let (fixture, rel) = write_manifest(doc);
        let agent = loader_with_meta(meta_of(&[("manifest", json!(rel.clone()))]));
        let err = load_manifest(&agent, &std::env::current_dir().unwrap()).unwrap_err();
        assert!(
            err.starts_with(&format!(
                "static_loader: manifest '{rel}' is not valid YAML: "
            )) && err.contains("found unhashable key"),
            "{doc:?}: {err}"
        );
        drop(fixture);
    }
}

/// The unknown type, as TEXT — key order is part of
/// the wire (`preserve_order`) and `Value` equality ignores it.
#[tokio::test]
async fn unknown_type_wire_shape_is_byte_exact() {
    let (kernel, _fixture) = boot_with_manifest(MANIFEST).await;
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("static_loader"),
            json!({"type": "frob"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(
        serde_json::to_string(&r).unwrap(),
        r#"{"error":"static_loader: unknown type 'frob'","reason":"unhandled","verb":"frob"}"#
    );
    let r = StaticLoaderBundle
        .handle(&AgentId::from("static_loader"), &json!({}), &kernel)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_string(&r).unwrap(),
        r#"{"error":"static_loader: unknown type None","reason":"unhandled","verb":null}"#
    );
}

/// The roster pass is kernel bookkeeping (ungated), so only the kernel's own
/// boot (origin none) may start it. A roster agent whose grant the operator
/// revoked sent `boot` to its loader and got the grant back; a deleted roster
/// agent came back. The edge is NAMED on both ends, so the refusal is the
/// loader's own, whatever the gate makes of `boot`. Python
/// `test_an_agent_sent_boot_is_refused_and_re_runs_nothing`.
#[tokio::test]
async fn an_agent_sent_boot_is_refused_and_re_runs_nothing() {
    let kernel = mk_kernel();
    let (_fixture, path) = write_manifest(
        "agents:\n  - id: memory\n    handler_module: mock.tools\n  - id: worker\n    \
         handler_module: mock.tools\n    can_call:\n      memory: [set, read]\n      \
         static_loader: [boot]\n",
    );
    let rec = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({
                "type": "create_agent", "parent_id": "kernel",
                "handler_module": HANDLER_MODULE, "id": "static_loader",
                "manifest": path, "callable_by": {"worker": ["boot"]},
            }),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(rec["id"], json!("static_loader"), "{rec}");
    // The operator revokes worker's lateral edge and deletes `memory`.
    for p in [
        json!({"type": "update_agent", "id": "worker", "can_call": {
            "$self": "*", "$parent": "*", "$subtree": "*", "static_loader": ["boot"]}}),
        json!({"type": "delete_agent", "id": "memory"}),
    ] {
        let r = kernel
            .send(vec![Item::new(AgentId::from("kernel"), p)])
            .await
            .remove(0)
            .result;
        assert!(r.get("error").is_none(), "{r}");
    }
    let r = as_agent(
        AgentId::from("worker"),
        kernel.send(vec![Item::new(
            AgentId::from("static_loader"),
            json!({"type": "boot"}),
        )]),
    )
    .await
    .remove(0)
    .result;
    assert_eq!(
        r,
        json!({
            "error": "static_loader: an agent-sent boot is refused — hydrating the \
                      roster is the kernel's own bookkeeping, so only the kernel's own \
                      boot (cold boot, create_agent) runs it",
            "reason": "unauthorized",
            "hint": "change an agent's record with update_agent (its parent writes \
                     it); the operator re-boots the loader",
        })
    );
    assert!(!kernel.agents.contains_key(&AgentId::from("memory")));
    assert!(record(&kernel, "worker").meta["can_call"]
        .get("memory")
        .is_none());
    // The operator's boot is the kernel's own: it re-runs the manifest.
    assert_eq!(
        boot_loader(&kernel, "static_loader").await,
        json!({"booted": true, "created": ["memory"], "reapplied": ["worker"]})
    );
}
