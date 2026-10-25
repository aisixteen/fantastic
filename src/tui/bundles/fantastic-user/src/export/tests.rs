//! The pure exporter: records in, blueprint out.

use super::*;
use serde_json::json;

const ROOT: &str = "kernel_state";

fn rec(id: &str, parent: Option<&str>, hm: &str, meta: Value) -> AgentRecord {
    AgentRecord {
        id: id.into(),
        handler_module: Some(hm.into()),
        meta: meta.as_object().cloned().unwrap_or_default(),
        parent_id: parent.map(str::to_string),
    }
}

/// root ─ user {memory (sqlite_store), mem2 (ram_store), exec, files, model
/// (a backend), mcp_x (unknown)} · team {a {b {c}}}
fn tree(a_meta: Value, b_meta: Value, c_meta: Value) -> Vec<AgentRecord> {
    vec![
        AgentRecord {
            id: ROOT.into(),
            handler_module: Some("kernel_state.tools".into()),
            meta: Map::new(),
            parent_id: None,
        },
        rec("user", Some(ROOT), "user.tools", json!({})),
        rec("memory", Some("user"), "sqlite_store.tools", json!({})),
        rec("mem2", Some("user"), "ram_store.tools", json!({})),
        rec("exec", Some("user"), "exec_bridge.tools", json!({})),
        rec("files", Some("user"), "file_bridge.tools", json!({})),
        rec(
            "credentials",
            Some("user"),
            "credential_store.tools",
            json!({}),
        ),
        rec("model", Some("user"), "anthropic_backend.tools", json!({})),
        rec("mcp_x", Some("user"), "mcp_client.tools", json!({})),
        rec("a", Some("user"), "ai_chat.tools", a_meta),
        rec("b", Some("a"), "ai_chat.tools", b_meta),
        rec("c", Some("b"), "noop.tools", c_meta),
    ]
}

fn none(_: &str, _: &str) -> Option<String> {
    None
}

fn no_bundle(_: &str) -> Vec<BundleFile> {
    Vec::new()
}

fn export(recs: &[AgentRecord]) -> Exported {
    build(recs, "a", ROOT, None, &none, &no_bundle).expect("exports")
}

fn doc(ex: &Exported) -> Value {
    serde_yaml::from_str(&ex.yaml).expect("the output is yaml")
}

/// The entry for `id`, anywhere in the nesting.
fn find<'a>(v: &'a Value, id: &str) -> Option<&'a Value> {
    v.get("agents")?.as_array()?.iter().find_map(|e| {
        if e["id"] == json!(id) {
            Some(e)
        } else {
            find(e, id)
        }
    })
}

/// seed.rs-style flattening: `(id, parent, record minus agents)`.
fn flatten(list: &Value, parent: Option<&str>, out: &mut Vec<(String, Option<String>, Value)>) {
    for e in list.as_array().into_iter().flatten() {
        let id = e["id"].as_str().unwrap().to_string();
        let mut r = e.clone();
        r.as_object_mut().unwrap().remove("agents");
        out.push((id.clone(), parent.map(str::to_string), r));
        if let Some(k) = e.get("agents") {
            flatten(k, Some(&id), out);
        }
    }
}

#[test]
fn three_level_subtree_nests_without_parent_id() {
    let ex = export(&tree(json!({}), json!({}), json!({})));
    assert_eq!(ex.agents, 3);
    let d = doc(&ex);
    let a = &d["agents"][0];
    assert_eq!(a["id"], "a");
    assert_eq!(a["agents"][0]["id"], "b");
    assert_eq!(a["agents"][0]["agents"][0]["id"], "c");
    assert_eq!(
        d["agents"].as_array().unwrap().len(),
        1,
        "only the subtree: {}",
        ex.yaml
    );
    assert!(!ex.yaml.contains("parent_id"), "{}", ex.yaml);
    assert!(find(&d, "user").is_none() && find(&d, ROOT).is_none());
}

#[test]
fn round_trip_reparse_gives_same_records_and_grants() {
    let recs = tree(
        json!({"display_name": "A", "can_call": {"b": "*", "$self": "*"}, "callable_by": {"b": ["send"]}}),
        json!({"model": "m1", "can_call": {"a": ["send"], "c": "*"}, "callable_by": {"a": "*"}}),
        json!({"n": 3, "callable_by": {"b": "*"}}),
    );
    let ex = export(&recs);
    let mut flat = Vec::new();
    flatten(&doc(&ex)["agents"], None, &mut flat);
    assert_eq!(flat.len(), 3);
    for (id, parent, r) in flat {
        let orig = recs.iter().find(|x| x.id == id).unwrap();
        let want_parent = if id == "a" {
            None
        } else {
            orig.parent_id.clone()
        };
        assert_eq!(parent, want_parent, "{id}");
        assert_eq!(r["handler_module"], json!(orig.handler_module), "{id}");
        let mut want = Value::Object(orig.meta.clone());
        want["id"] = json!(id);
        want["handler_module"] = json!(orig.handler_module);
        assert_eq!(r, want, "{id}: every inside record and grant comes back");
    }
}

#[test]
fn inside_grant_stays_an_id() {
    let ex = export(&tree(
        json!({"can_call": {"b": "*"}}),
        json!({"callable_by": {"a": "*"}}),
        json!({}),
    ));
    let d = doc(&ex);
    assert_eq!(find(&d, "a").unwrap()["can_call"]["b"], "*");
    assert_eq!(find(&d, "b").unwrap()["callable_by"]["a"], "*");
    assert!(ex.ports.is_empty(), "{:?}", ex.ports);
}

#[test]
fn outside_grant_becomes_a_port() {
    let ex = export(&tree(
        json!({"can_call": {"memory": "*"}, "memory_store_id": "memory"}),
        json!({}),
        json!({}),
    ));
    let d = doc(&ex);
    let a = find(&d, "a").unwrap();
    assert_eq!(a["can_call"]["@memory"], "*", "{}", ex.yaml);
    assert!(a["can_call"].get("memory").is_none());
    assert_eq!(a["memory_store_id"], "@memory");
    assert_eq!(d["ports"]["@memory"]["kind"], "memory");
    assert_eq!(d["ports"]["@memory"]["used_by"], json!(["a"]));
    assert!(d["ports"]["@memory"]["why"]
        .as_str()
        .unwrap()
        .contains("reaches memory"));
}

#[test]
fn outside_backend_becomes_model_port() {
    let ex = export(&tree(
        json!({"backend_id": "model", "can_call": {"model": "*"}}),
        json!({}),
        json!({}),
    ));
    let d = doc(&ex);
    assert_eq!(find(&d, "a").unwrap()["backend_id"], "@model");
    assert_eq!(d["ports"]["@model"]["kind"], "model");
}

#[test]
fn outside_exec_becomes_exec_port() {
    let ex = export(&tree(
        json!({}),
        json!({"can_call": {"exec": ["run"], "files": "*"}}),
        json!({}),
    ));
    let d = doc(&ex);
    let b = find(&d, "b").unwrap();
    assert_eq!(b["can_call"]["@exec"], json!(["run"]));
    assert_eq!(b["can_call"]["@files"], "*");
    assert_eq!(d["ports"]["@exec"]["kind"], "exec");
    assert_eq!(d["ports"]["@files"]["kind"], "files");
}

#[test]
fn outside_unknown_becomes_connector_port() {
    let ex = export(&tree(
        json!({"can_call": {"mcp_x": "*", "ghost": "*"}}),
        json!({}),
        json!({}),
    ));
    let d = doc(&ex);
    let a = find(&d, "a").unwrap();
    assert_eq!(a["can_call"]["@connector:mcp_x"], "*");
    assert_eq!(
        a["can_call"]["@connector:ghost"], "*",
        "an id nowhere in the tree"
    );
    assert_eq!(d["ports"]["@connector:mcp_x"]["kind"], "connector");
}

#[test]
fn two_outside_stores_get_distinct_ports() {
    // mem2 is referenced first, but `memory` > `mem2` by id: mem2 is @memory
    let ex = export(&tree(
        json!({"memory_store_id": "mem2"}),
        json!({"memory_store_id": "memory"}),
        json!({}),
    ));
    let d = doc(&ex);
    assert_eq!(find(&d, "a").unwrap()["memory_store_id"], "@memory");
    assert_eq!(find(&d, "b").unwrap()["memory_store_id"], "@memory_2");
    let again = export(&tree(
        json!({"memory_store_id": "mem2"}),
        json!({"memory_store_id": "memory"}),
        json!({}),
    ));
    assert_eq!(again.yaml, ex.yaml, "deterministic");
    let names: Vec<&str> = ex.ports.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, vec!["@memory", "@memory_2"]);
    assert_eq!(ex.ports[0].target, "mem2");
}

#[test]
fn root_grant_is_written_as_at_root() {
    let ex = export(&tree(
        json!({"can_call": {ROOT: "*"}, "callable_by": {"$parent": "*"}}),
        json!({}),
        json!({}),
    ));
    let d = doc(&ex);
    let a = find(&d, "a").unwrap();
    assert_eq!(a["can_call"]["@root"], "*");
    assert_eq!(a["callable_by"]["$parent"], "*", "baseline grants stay");
    assert!(!ex.yaml.contains(ROOT), "{}", ex.yaml);
    assert!(ex.ports.is_empty());
}

#[test]
fn runtime_fields_are_dropped() {
    let ex = export(&tree(
        json!({"guid": "g-123", "pid": 7, "version": 2, "created_at": 1, "updated_at": 2, "ts": 3,
               "seen_ts": 4, "last_turn": "x", "last_error": "e", "boot_error": "e", "rows": [1],
               "history": ["hello there"], "cache": {}, "usage": {"in": 1}, "keep": "yes"}),
        json!({}),
        json!({}),
    ));
    let a = find(&doc(&ex), "a").unwrap().clone();
    let mut keys: Vec<&String> = a.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(
        keys,
        vec!["agents", "handler_module", "id", "keep"],
        "{}",
        ex.yaml
    );
    assert!(!ex.yaml.contains("hello there") && !ex.yaml.contains("g-123"));
}

#[test]
fn credentials_keep_the_ref_but_never_a_key() {
    let ex = export(&tree(
        json!({"credentials": {"store": "credentials", "ref": "anthropic", "api_key": "sk-secret"},
               "api_key": "sk-secret", "nested": {"token": "sk-secret"}}),
        json!({"credentials": {"store": "c", "ref": "r2"}}),
        json!({}),
    ));
    let d = doc(&ex);
    let a = find(&d, "a").unwrap();
    assert_eq!(a["credentials"]["ref"], "anthropic");
    assert_eq!(a["credentials"]["store"], "@credentials");
    assert_eq!(d["ports"]["@credentials"]["kind"], "credentials");
    assert_eq!(
        find(&d, "b").unwrap()["credentials"]["store"],
        "c",
        "an inside store stays an id"
    );
    assert!(!ex.yaml.contains("sk-secret"), "{}", ex.yaml);
    assert!(!ex.yaml.contains("api_key") && !ex.yaml.contains("token"));
}

#[test]
fn absolute_paths_become_relative_or_port() {
    let recs = tree(
        json!({"root": "/home/u/proj/.fantastic/agents/user/agents/a/data", "cwd": "/etc", "allow": ["/etc", "/opt"]}),
        json!({"path": "C:\\Users\\u\\x.db"}),
        json!({}),
    );
    let ex = build(
        &recs,
        "a",
        ROOT,
        Some("/home/u/proj/.fantastic/agents/user/agents/a"),
        &none,
        &no_bundle,
    )
    .unwrap();
    let d = doc(&ex);
    let a = find(&d, "a").unwrap();
    assert_eq!(a["root"], "data");
    assert_eq!(a["cwd"], "@path:cwd");
    assert_eq!(a["allow"], json!(["@path:allow", "@path:allow_2"]));
    assert_eq!(find(&d, "b").unwrap()["path"], "@path:path");
    assert_eq!(d["ports"]["@path:cwd"]["kind"], "path");
    for p in ["/home/u", "/etc", "/opt", "C:\\"] {
        assert!(!ex.yaml.contains(p), "{p} leaked: {}", ex.yaml);
    }
}

#[test]
fn directive_file_is_exported_beside_the_blueprint() {
    let recs = tree(
        json!({"system_prompt_file": "prompt.md"}),
        json!({"system_prompt_file": "missing.md"}),
        json!({}),
    );
    let read =
        |id: &str, f: &str| (id == "a" && f == "prompt.md").then(|| "You are A.".to_string());
    let ex = build(&recs, "a", ROOT, None, &read, &no_bundle).unwrap();
    assert_eq!(
        ex.files,
        vec![("a/directive.md".to_string(), "You are A.".to_string())]
    );
    let d = doc(&ex);
    assert_eq!(
        find(&d, "a").unwrap()["system_prompt_file"],
        "a/directive.md"
    );
    assert_eq!(find(&d, "b").unwrap()["system_prompt_file"], "missing.md");
    assert_eq!(ex.notes.len(), 1, "{:?}", ex.notes);
    assert!(ex.yaml.contains("# NOTE: agent b"), "{}", ex.yaml);
}

#[test]
fn exporting_the_root_or_unknown_id_is_refused() {
    let recs = tree(json!({}), json!({}), json!({}));
    assert!(build(&recs, ROOT, ROOT, None, &none, &no_bundle).is_err());
    assert!(build(&recs, "nope", ROOT, None, &none, &no_bundle).is_err());
    let mut loose = recs.clone();
    loose.push(AgentRecord {
        id: "loose".into(),
        handler_module: None,
        meta: Map::new(),
        parent_id: None,
    });
    assert!(
        build(&loose, "loose", ROOT, None, &none, &no_bundle).is_err(),
        "a record in no tree"
    );
}

#[test]
fn the_scan_finds_keys_rows_and_absolute_paths() {
    let d = std::env::temp_dir().join(format!("fu-scan-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("a")).unwrap();
    std::fs::write(
        d.join(BLUEPRINT_FILE),
        "agents:\n- id: a\n  root: /etc\n  rows: [x]\n  api_key: k\n",
    )
    .unwrap();
    std::fs::write(d.join("a/directive.md"), "use sk-abcdef1234 now").unwrap();
    let hits = scan_secrets(&d);
    assert_eq!(hits.len(), 4, "{hits:?}");
    std::fs::write(d.join(BLUEPRINT_FILE), "agents:\n- id: a\n  root: data\n").unwrap();
    std::fs::write(d.join("a/directive.md"), "ask-me about task-lists").unwrap();
    assert!(scan_secrets(&d).is_empty());
    let _ = std::fs::remove_dir_all(&d);
}

// ---- OWNED FILES BEYOND THE DIRECTIVE (#268) ----

fn folder_tree(view: Value) -> Vec<AgentRecord> {
    let mut recs = tree(json!({}), json!({}), json!({}));
    for r in &mut recs {
        if r.id == "a" || r.id == "b" {
            r.handler_module = Some("py_ab12".into());
        }
        if r.id == "a" {
            r.meta.insert("view".into(), view.clone());
        }
    }
    recs
}

fn py_files(hm: &str) -> Vec<BundleFile> {
    if hm != "py_ab12" {
        return Vec::new();
    }
    vec![
        ("agent.py".into(), Ok("print('hi')\n".into())),
        (
            "bundle.yaml".into(),
            Ok("base: py_agent\nsentence: x\n".into()),
        ),
        ("panel.html".into(), Ok("<p>panel</p>".into())),
    ]
}

fn export_with(recs: &[AgentRecord], rb: &dyn Fn(&str) -> Vec<BundleFile>) -> Exported {
    build(recs, "a", ROOT, None, &none, rb).expect("exports")
}

#[test]
fn view_bytes_body_is_exported_beside_the_blueprint() {
    let recs = folder_tree(json!({"kind": "html", "bytes": "<p>hi</p>", "calls": ["a"]}));
    let ex = export_with(&recs, &no_bundle);
    assert!(
        ex.files
            .contains(&("a/view.html".to_string(), "<p>hi</p>".to_string())),
        "{:?}",
        ex.files
    );
    let d = doc(&ex);
    let v = &find(&d, "a").unwrap()["view"];
    assert_eq!(v["bytes_file"], "a/view.html");
    assert!(v.get("bytes").is_none(), "no inline body: {}", ex.yaml);
    assert_eq!(v["calls"], json!(["a"]));
    assert_eq!(v["kind"], "html");
}

#[test]
fn folder_bundle_files_are_exported_once_per_bundle_and_marked_as_code() {
    let recs = folder_tree(json!({}));
    let ex = export_with(&recs, &py_files);
    let paths: Vec<&str> = ex.files.iter().map(|(p, _)| p.as_str()).collect();
    for f in ["agent.py", "bundle.yaml", "panel.html"] {
        let want = format!("bundles/py_ab12/{f}");
        assert_eq!(paths.iter().filter(|p| **p == want).count(), 1, "{paths:?}");
        assert!(ex.code.contains(&want), "{:?}", ex.code);
        assert!(
            ex.yaml.contains(&format!("# code: {want}\n")),
            "{}",
            ex.yaml
        );
    }
    assert_eq!(ex.code.len(), 3, "{:?}", ex.code);
    let at = |p: &str| paths.iter().position(|x| *x == p).unwrap();
    assert!(at("bundles/py_ab12/agent.py") < at("bundles/py_ab12/bundle.yaml"));
}

#[test]
fn view_file_is_code_and_listed_in_code() {
    let recs = folder_tree(json!({"kind": "html", "bytes": "<b>x</b>"}));
    let ex = export_with(&recs, &no_bundle);
    assert_eq!(ex.code, vec!["a/view.html".to_string()]);
    assert!(ex.yaml.contains("# code: a/view.html\n"), "{}", ex.yaml);
}

#[test]
fn unreadable_or_oversized_bundle_files_are_noted_not_written() {
    let recs = folder_tree(json!({}));
    let big = "x".repeat(MAX_FILE + 1);
    let rb = move |hm: &str| -> Vec<BundleFile> {
        if hm != "py_ab12" {
            return Vec::new();
        }
        vec![
            ("agent.py".into(), Ok("ok".into())),
            ("link.py".into(), Err("a symlink".into())),
            ("blob.bin".into(), Err("not UTF-8 text".into())),
            ("big.txt".into(), Ok(big.clone())),
            (".env".into(), Ok("K=1".into())),
        ]
    };
    let ex = export_with(&recs, &rb);
    let paths: Vec<&str> = ex.files.iter().map(|(p, _)| p.as_str()).collect();
    assert!(paths.contains(&"bundles/py_ab12/agent.py"), "{paths:?}");
    for bad in ["link.py", "blob.bin", "big.txt", ".env"] {
        assert!(!paths.iter().any(|p| p.ends_with(bad)), "{bad}: {paths:?}");
        assert!(
            ex.notes.iter().any(|n| n.contains(bad)),
            "{bad}: {:?}",
            ex.notes
        );
        assert!(
            ex.yaml
                .contains(&format!("# NOTE: bundle py_ab12: file {bad}")),
            "{}",
            ex.yaml
        );
    }
}

#[test]
fn owned_files_leave_the_blueprint_free_of_secrets_and_absolute_paths() {
    let recs = folder_tree(json!({"kind": "html", "bytes": "<p>hi</p>"}));
    let ex = export_with(&recs, &py_files);
    let d = std::env::temp_dir().join(format!("fu-owned-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join(BLUEPRINT_FILE), &ex.yaml).unwrap();
    for (rel, text) in &ex.files {
        let p = d.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
    assert!(scan_secrets(&d).is_empty(), "{:?}", scan_secrets(&d));
    assert!(
        !ex.yaml.contains("<p>hi</p>"),
        "no inline body: {}",
        ex.yaml
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn an_sk_token_inside_an_exported_code_file_is_found_by_the_scan() {
    let recs = folder_tree(json!({}));
    let rb = |hm: &str| -> Vec<BundleFile> {
        if hm != "py_ab12" {
            return Vec::new();
        }
        vec![("agent.py".into(), Ok("KEY = 'sk-abcd1234efgh'\n".into()))]
    };
    let ex = export_with(&recs, &rb);
    let d = std::env::temp_dir().join(format!("fu-sk-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("bundles/py_ab12")).unwrap();
    std::fs::write(d.join(BLUEPRINT_FILE), &ex.yaml).unwrap();
    for (rel, text) in &ex.files {
        std::fs::write(d.join(rel), text).unwrap();
    }
    let hits = scan_secrets(&d);
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert!(hits[0].contains("bundles/py_ab12/agent.py"), "{hits:?}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn no_owned_files_when_the_record_has_none() {
    let ex = export(&tree(json!({}), json!({}), json!({})));
    assert!(ex.files.is_empty() && ex.code.is_empty(), "{:?}", ex.files);
    assert!(!ex.yaml.contains("# code:"), "{}", ex.yaml);
}
