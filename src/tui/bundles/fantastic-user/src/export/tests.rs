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

fn export(recs: &[AgentRecord]) -> Exported {
    build(recs, "a", ROOT, None, &none).expect("exports")
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
    let ex = build(&recs, "a", ROOT, None, &read).unwrap();
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
    assert!(build(&recs, ROOT, ROOT, None, &none).is_err());
    assert!(build(&recs, "nope", ROOT, None, &none).is_err());
    let mut loose = recs.clone();
    loose.push(AgentRecord {
        id: "loose".into(),
        handler_module: None,
        meta: Map::new(),
        parent_id: None,
    });
    assert!(
        build(&loose, "loose", ROOT, None, &none).is_err(),
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
