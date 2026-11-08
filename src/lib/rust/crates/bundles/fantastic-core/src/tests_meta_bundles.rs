//! META BUNDLES — a `bundles/<name>/bundle.yaml` with a `base` and no program: the
//! yaml over a compiled bundle, in RAM. An instance's `handler_module` is the meta
//! bundle; it runs the base's code with the bundle's `record` under its own fields.
//! The twin of python `kernel_state/tests/test_meta_bundles.py`. The base here is the
//! file bridge: any registered bundle will do.

use super::*;

/// A kernel with the root loader and the file bridge, booted on `tmp`.
async fn kernel_with_bundles(tmp: &TempDir, bundles: &[(&str, &str)]) -> Arc<Kernel> {
    for (name, yaml) in bundles {
        write_bundle(tmp, name, yaml);
    }
    let kernel = with_file(tmp);
    crate::folders::load_bundles(&kernel, &tmp.path().join(".fantastic"));
    kernel
}

fn write_bundle(tmp: &TempDir, name: &str, yaml: &str) {
    let folder = tmp.path().join(".fantastic").join("bundles").join(name);
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("bundle.yaml"), yaml).unwrap();
}

const NOTES: &str = "base: file_bridge.tools\nsentence: A notes folder.\nrecord:\n  root: notes\n  allow: ['.']\n  ingress_rule: allow_all_ingress\n";

async fn create(kernel: &Arc<Kernel>, extra: serde_json::Value) -> serde_json::Value {
    let mut p = json!({"type": "create_agent", "parent_id": "kernel"});
    for (k, v) in extra.as_object().unwrap() {
        p[k] = v.clone();
    }
    root_reflect(kernel, p).await
}

fn meta(kernel: &Arc<Kernel>, id: &str) -> serde_json::Map<String, serde_json::Value> {
    kernel.agents.get(&AgentId::from(id)).unwrap().meta.read().unwrap().clone()
}

#[tokio::test]
async fn an_instance_of_a_meta_bundle_inherits_its_fields_and_runs_its_base() {
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("notes")).unwrap();
    std::fs::write(tmp.path().join("notes").join("a.md"), "hello\n").unwrap();
    let kernel = kernel_with_bundles(&tmp, &[("notes", NOTES)]).await;
    let r = create(&kernel, json!({"id": "n1", "handler_module": "notes"})).await;
    assert_eq!(r["id"], json!("n1"), "{r}");
    let m = meta(&kernel, "n1");
    assert_eq!(m["root"], json!("notes"));
    assert_eq!(m["allow"], json!(["."]));
    let rec = kernel.agents.get(&AgentId::from("n1")).unwrap().record();
    assert_eq!(rec.handler_module.as_deref(), Some("notes"), "the kind stays the meta bundle");
    // the base's code answers: the file bridge reads its root
    let read = kernel
        .send(vec![Item::new(AgentId::from("n1"), json!({"type": "read", "path": "a.md"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(read["lines"][0]["text"], json!("hello"), "{read}");
    // the catalog lists it with its base, and not its fields
    let rows = kernel.bundles.folder_rows();
    let row = rows.iter().find(|r| r["name"] == json!("notes")).unwrap();
    assert_eq!(row["base"], json!("file_bridge.tools"), "{row}");
    assert!(row.get("record").is_none(), "{row}");
}

#[tokio::test]
async fn a_field_the_create_names_overrides_the_inherited_one() {
    let tmp = TempDir::new().unwrap();
    let kernel = kernel_with_bundles(&tmp, &[("notes", NOTES)]).await;
    create(&kernel, json!({"id": "n2", "handler_module": "notes", "root": "elsewhere"})).await;
    let m = meta(&kernel, "n2");
    assert_eq!(m["root"], json!("elsewhere"), "the instance's own value wins");
    assert_eq!(m["allow"], json!(["."]), "the rest is inherited");
}

#[tokio::test]
async fn bundle_beside_the_base_is_the_same_create() {
    let tmp = TempDir::new().unwrap();
    let kernel = kernel_with_bundles(&tmp, &[("notes", NOTES)]).await;
    let r = create(&kernel, json!({"id": "n3", "handler_module": "file_bridge.tools", "bundle": "notes"})).await;
    assert_eq!(r["id"], json!("n3"), "{r}");
    let rec = kernel.agents.get(&AgentId::from("n3")).unwrap().record();
    assert_eq!(rec.handler_module.as_deref(), Some("notes"));
    assert!(!rec.meta.contains_key("bundle"), "the record carries the kind, not a `bundle` field");
    assert_eq!(rec.meta["root"], json!("notes"));
}

#[tokio::test]
async fn a_meta_bundle_extends_a_meta_bundle_the_nearest_field_wins() {
    let tmp = TempDir::new().unwrap();
    // `drafts` sorts before `notes`: the boot load takes it on a second pass
    let drafts = "base: notes\nsentence: Drafts.\nrecord:\n  root: drafts\n";
    let kernel = kernel_with_bundles(&tmp, &[("notes", NOTES), ("drafts", drafts)]).await;
    create(&kernel, json!({"id": "d1", "handler_module": "drafts"})).await;
    let m = meta(&kernel, "d1");
    assert_eq!(m["root"], json!("drafts"), "the nearer bundle wins");
    assert_eq!(m["ingress_rule"], json!("allow_all_ingress"), "the farther one fills the rest");
    assert_eq!(kernel.bundles.meta_chain("drafts"), ["drafts", "notes"]);
}

#[tokio::test]
async fn a_reload_rewrites_the_inherited_fields_and_keeps_an_instances_own() {
    let tmp = TempDir::new().unwrap();
    let kernel = kernel_with_bundles(&tmp, &[("notes", NOTES)]).await;
    create(&kernel, json!({"id": "i1", "handler_module": "notes"})).await;
    create(&kernel, json!({"id": "i2", "handler_module": "notes", "root": "mine"})).await;
    write_bundle(
        &tmp,
        "notes",
        "base: file_bridge.tools\nsentence: A notes folder.\nrecord:\n  root: notes2\n  allow: ['.']\n",
    );
    let r = root_reflect(&kernel, json!({"type": "reload_bundle", "name": "notes"})).await;
    assert_eq!(r["reloaded"], json!("notes"), "{r}");
    let (m1, m2) = (meta(&kernel, "i1"), meta(&kernel, "i2"));
    assert_eq!(m1["root"], json!("notes2"), "an inherited field takes the new value");
    assert!(!m1.contains_key("ingress_rule"), "a field the bundle dropped goes");
    assert_eq!(m2["root"], json!("mine"), "the instance's own field stays");
    assert_eq!(r["instances"], json!(["i1", "i2"]), "{r}");
}

#[tokio::test]
async fn a_meta_bundle_is_refused_naming_the_field() {
    let tmp = TempDir::new().unwrap();
    let kernel = kernel_with_bundles(
        &tmp,
        &[
            ("nobase", "base: no_such.tools\nsentence: s\n"),
            ("badrec", "base: file_bridge.tools\nsentence: s\nrecord: [1]\n"),
            ("loop_a", "base: loop_b\nsentence: s\n"),
            ("loop_b", "base: loop_a\nsentence: s\n"),
        ],
    )
    .await;
    let rows = kernel.bundles.folder_rows();
    let err = |n: &str| {
        rows.iter().find(|r| r["name"] == json!(n)).unwrap()["error"].as_str().unwrap_or("").to_string()
    };
    assert!(err("nobase").contains("base: 'no_such.tools' is no bundle"), "{}", err("nobase"));
    assert!(err("badrec").contains("record: must be a mapping"), "{}", err("badrec"));
    assert!(err("loop_a").contains("base:") && err("loop_b").contains("base:"), "{rows:?}");
    let r = create(&kernel, json!({"id": "x", "handler_module": "nobase"})).await;
    assert!(kernel.bundles.get("nobase").is_none(), "a refused meta bundle resolves to nothing: {r}");
}

#[tokio::test]
async fn a_record_loaded_from_disk_inherits_like_a_create() {
    let tmp = TempDir::new().unwrap();
    let kernel = kernel_with_bundles(&tmp, &[("notes", NOTES)]).await;
    let root = kernel.root().unwrap().record();
    let root_map = match serde_json::to_value(&root).unwrap() {
        serde_json::Value::Object(m) => m,
        _ => unreachable!(),
    };
    let child = json!({"id": "n9", "handler_module": "notes", "parent_id": root.id, "root": "mine"});
    kernel
        .load_records(vec![root_map, child.as_object().unwrap().clone()])
        .unwrap();
    let m = meta(&kernel, "n9");
    assert_eq!(m["root"], json!("mine"), "the record's own field wins");
    assert_eq!(m["allow"], json!(["."]), "the bundle's fills the rest");
}

#[tokio::test]
async fn a_bundle_on_another_base_keeps_that_handler_and_names_the_bundle() {
    let tmp = TempDir::new().unwrap();
    write_bundle(&tmp, "notes", NOTES);
    let mut reg = BundleRegistry::new();
    reg.register(HANDLER_MODULE, KernelStateBundle::default());
    reg.register("file_bridge.tools", fantastic_file::FileBundle::default());
    reg.register("files_two.tools", fantastic_file::FileBundle::default());
    let kernel = bootstrap(reg, BootstrapOptions::daemon(tmp.path())).expect("boot").kernel;
    crate::folders::load_bundles(&kernel, &tmp.path().join(".fantastic"));
    let r = create(&kernel, json!({"id": "o1", "handler_module": "files_two.tools", "bundle": "notes"})).await;
    assert_eq!(r["id"], json!("o1"), "{r}");
    let rec = kernel.agents.get(&AgentId::from("o1")).unwrap().record();
    assert_eq!(rec.handler_module.as_deref(), Some("files_two.tools"), "it runs the base it named");
    assert_eq!(rec.meta["bundle"], json!("notes"), "and names the bundle it inherits from");
    assert_eq!(rec.meta["root"], json!("notes"));
    write_bundle(&tmp, "notes", "base: file_bridge.tools\nsentence: s\nrecord:\n  root: notes3\n");
    let r = root_reflect(&kernel, json!({"type": "reload_bundle", "name": "notes"})).await;
    assert_eq!(r["instances"], json!(["o1"]), "{r}");
    assert_eq!(meta(&kernel, "o1")["root"], json!("notes3"), "a reload reaches it too");
}

// ---- EMBEDDED META BUNDLES: a product's own bundle.yaml texts, in RAM ----------------------

#[tokio::test]
async fn an_embedded_meta_bundle_is_inherited_with_nothing_written_to_a_folder() {
    let tmp = TempDir::new().unwrap();
    let kernel = kernel_with_bundles(&tmp, &[]).await;
    kernel.bundles.register_meta_yaml("notes", NOTES).unwrap();
    let r = create(&kernel, json!({"id": "e1", "handler_module": "notes"})).await;
    assert_eq!(r["id"], json!("e1"), "{r}");
    assert_eq!(meta(&kernel, "e1")["root"], json!("notes"));
    assert!(!tmp.path().join(".fantastic").join("bundles").join("notes").exists(), "no folder is written");
    let rows = kernel.bundles.folder_rows();
    let row = rows.iter().find(|r| r["name"] == json!("notes")).unwrap();
    assert_eq!(row["source"], json!("embedded"), "{row}");
    assert!(row.get("record").is_none(), "{row}");
}

#[tokio::test]
async fn a_folder_bundle_overrides_an_embedded_one_of_the_same_name() {
    let tmp = TempDir::new().unwrap();
    write_bundle(&tmp, "notes", "base: file_bridge.tools\nsentence: Mine.\nrecord:\n  root: mine\n");
    let kernel = with_file(&tmp);
    kernel.bundles.register_meta_yaml("notes", NOTES).unwrap();
    crate::folders::load_bundles(&kernel, &tmp.path().join(".fantastic"));
    create(&kernel, json!({"id": "e2", "handler_module": "notes"})).await;
    assert_eq!(meta(&kernel, "e2")["root"], json!("mine"), "the folder's wins");
    let rows = kernel.bundles.folder_rows();
    let row = rows.iter().find(|r| r["name"] == json!("notes")).unwrap();
    assert_eq!(row["source"], json!("folder"), "{row}");
}

#[tokio::test]
async fn a_refused_folder_leaves_the_embedded_one_live_and_says_why() {
    let tmp = TempDir::new().unwrap();
    write_bundle(&tmp, "notes", "base: file_bridge.tools\nsentence: Bad.\nrecord: [1]\n");
    let kernel = with_file(&tmp);
    kernel.bundles.register_meta_yaml("notes", NOTES).unwrap();
    crate::folders::load_bundles(&kernel, &tmp.path().join(".fantastic"));
    create(&kernel, json!({"id": "e3", "handler_module": "notes"})).await;
    assert_eq!(meta(&kernel, "e3")["root"], json!("notes"), "the embedded one stands");
    let rows = kernel.bundles.folder_rows();
    let row = rows.iter().find(|r| r["name"] == json!("notes")).unwrap();
    assert_eq!(row["source"], json!("embedded"), "{row}");
    assert!(row["folder_error"].as_str().unwrap_or("").contains("record"), "{row}");
}

#[tokio::test]
async fn an_embedded_meta_bundle_is_refused_naming_the_field() {
    let tmp = TempDir::new().unwrap();
    let kernel = kernel_with_bundles(&tmp, &[]).await;
    let b = &kernel.bundles;
    assert!(b.register_meta_yaml("x", "sentence: s\n").unwrap_err().contains("base: required"));
    assert!(b.register_meta_yaml("x", "base: no_such.tools\nsentence: s\n").unwrap_err().contains("is no bundle"));
    assert!(b.register_meta_yaml("x", "base: file_bridge.tools\n").unwrap_err().contains("sentence: required"));
    assert!(b.register_meta_yaml("x", "base: file_bridge.tools\nsentence: s\nrecord: [1]\n").unwrap_err().contains("record:"));
    b.register_meta_yaml("a", "base: file_bridge.tools\nsentence: s\n").unwrap();
    b.register_meta_yaml("b", "base: a\nsentence: s\n").unwrap();
    assert!(b.register_meta_yaml("a", "base: b\nsentence: s\n").unwrap_err().contains("leads back"));
}

#[tokio::test]
async fn an_embedded_meta_bundle_extends_an_embedded_one() {
    let tmp = TempDir::new().unwrap();
    let kernel = kernel_with_bundles(&tmp, &[]).await;
    kernel.bundles.register_meta_yaml("notes", NOTES).unwrap();
    kernel.bundles.register_meta_yaml("drafts", "base: notes\nsentence: Drafts.\nrecord:\n  root: drafts\n").unwrap();
    create(&kernel, json!({"id": "e4", "handler_module": "drafts"})).await;
    let m = meta(&kernel, "e4");
    assert_eq!(m["root"], json!("drafts"), "the nearer wins");
    assert_eq!(m["ingress_rule"], json!("allow_all_ingress"), "the farther fills the rest");
}
