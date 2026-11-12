//! The pure plan: hand-written blueprints, no kernel.

use super::*;

const ROOT: &str = "kernel_root";

/// The agents that exist: memory (a store), exec, two_ex (an exec), and `taken`.
fn existing(id: &str, taken: &[&str]) -> Option<String> {
    match id {
        "memory" | "memory2" => Some("sqlite_store.tools".into()),
        "exec" => Some("exec_bridge.tools".into()),
        "home" => Some("user.tools".into()),
        ROOT => Some(String::new()),
        x if taken.contains(&x) => Some("echo.tools".into()),
        _ => None,
    }
}

fn known(hm: &str) -> bool {
    matches!(
        hm,
        "echo.tools" | "sqlite_store.tools" | "exec_bridge.tools"
    )
}

fn run(
    yaml: &str,
    files: &[(&str, &str)],
    bind: Value,
    allow_code: bool,
    taken: &[&str],
) -> Result<Plan, ImportError> {
    let files: BTreeMap<String, String> = files
        .iter()
        .map(|(a, b)| ((*a).to_string(), (*b).to_string()))
        .collect();
    let bind = bind.as_object().cloned().unwrap_or_default();
    let agent = |id: &str| existing(id, taken);
    let w = World {
        parent_id: "home",
        root_id: ROOT,
        bind: &bind,
        allow_code,
        agent: &agent,
        known_bundle: &known,
    };
    plan(yaml, &files, &w)
}

fn ok(yaml: &str, bind: Value) -> Plan {
    run(yaml, &[], bind, false, &[]).expect("plans")
}

fn agent<'a>(p: &'a Plan, id: &str) -> &'a Planned {
    p.agents.iter().find(|a| a.new_id == id).expect(id)
}

const THREE: &str = "
agents:
  - id: a
    handler_module: echo.tools
    can_call: {b: '*', $self: '*'}
    agents:
      - id: b
        handler_module: echo.tools
        callable_by: {a: '*', $parent: '*'}
        can_call: {'*': '*'}
        agents:
          - id: c
            handler_module: echo.tools
            notes: plain
";

#[test]
fn plan_gives_every_agent_a_fresh_suffixed_id() {
    let p = ok(THREE, json!({}));
    let ids: Vec<(&str, &str)> = p
        .ids
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    assert_eq!(ids, [("a", "a_1"), ("b", "b_1"), ("c", "c_1")]);
    let order: Vec<(&str, &str)> = p
        .agents
        .iter()
        .map(|a| (a.new_id.as_str(), a.parent.as_str()))
        .collect();
    assert_eq!(order, [("a_1", "home"), ("b_1", "a_1"), ("c_1", "b_1")]);
    assert_eq!(agent(&p, "c_1").meta["notes"], "plain");
    assert!(p.edges.is_empty());
}

#[test]
fn plan_picks_next_free_suffix_when_taken() {
    let p = run(THREE, &[], json!({}), false, &["a_1"]).unwrap();
    assert_eq!(
        p.ids.values().cloned().collect::<Vec<_>>(),
        ["a_2", "b_2", "c_2"]
    );
    // one taken anywhere moves them all
    let p = run(THREE, &[], json!({}), false, &["c_1", "b_2"]).unwrap();
    assert_eq!(
        p.ids.values().cloned().collect::<Vec<_>>(),
        ["a_3", "b_3", "c_3"]
    );
}

#[test]
fn plan_rewrites_inside_grants_on_both_ends() {
    let p = ok(THREE, json!({}));
    assert_eq!(
        Value::Object(
            agent(&p, "a_1").meta["can_call"]
                .as_object()
                .unwrap()
                .clone()
        ),
        json!({"b_1": "*", "$self": "*"})
    );
    assert_eq!(
        agent(&p, "b_1").meta["callable_by"],
        json!({"a_1": "*", "$parent": "*"})
    );
    assert_eq!(agent(&p, "b_1").meta["can_call"], json!({"*": "*"}));
}

#[test]
fn plan_resolves_root_port() {
    let y = "
agents:
  - id: a
    handler_module: echo.tools
    can_call: {'@root': ['reflect']}
";
    let p = ok(y, json!({}));
    assert_eq!(
        agent(&p, "a_1").meta["can_call"],
        json!({ROOT: ["reflect"]})
    );
    // the root grants back, and the preview shows it
    assert_eq!(
        p.edges,
        [Edge {
            agent: ROOT.into(),
            field: "callable_by",
            key: "a_1".into(),
            grant: json!(["reflect"]),
        }]
    );
    assert_eq!(p.reaches[0]["ports"]["@root"], ROOT);
}

const PORTED: &str = "
agents:
  - id: a
    handler_module: echo.tools
    can_call: {'@memory': '*'}
    callable_by: {'@exec': ['emit']}
    memory_store_id: '@memory'
    credentials: {store: '@memory', ref: r1}
ports:
  '@memory': {kind: memory, why: x, used_by: [a]}
  '@exec': {kind: exec, why: y, used_by: [a]}
";

#[test]
fn plan_binds_ports_into_grants_and_refs() {
    let p = ok(PORTED, json!({"@memory": "memory2", "@exec": "exec"}));
    let a = agent(&p, "a_1");
    assert_eq!(a.meta["can_call"], json!({"memory2": "*"}));
    assert_eq!(a.meta["callable_by"], json!({"exec": ["emit"]}));
    assert_eq!(a.meta["memory_store_id"], "memory2");
    assert_eq!(
        a.meta["credentials"],
        json!({"store": "memory2", "ref": "r1"})
    );
    assert_eq!(
        p.edges,
        [
            Edge {
                agent: "memory2".into(),
                field: "callable_by",
                key: "a_1".into(),
                grant: json!("*"),
            },
            Edge {
                agent: "exec".into(),
                field: "can_call",
                key: "a_1".into(),
                grant: json!(["emit"]),
            },
        ]
    );
    assert_eq!(p.ports["@memory"], "memory2");
    assert_eq!(p.reaches[0]["can_call"], json!(["memory2"]));
    assert_eq!(p.reaches[0]["callable_by"], json!(["exec"]));
}

#[test]
fn plan_refuses_unbound_ports_naming_all() {
    let e = run(PORTED, &[], json!({}), false, &[]).unwrap_err();
    assert_eq!(e.reason, "unbound_ports");
    for want in [
        "@memory (memory)",
        "@exec (exec)",
        "bind {",
        "\"<agent id>\"",
    ] {
        assert!(e.message.contains(want), "{want}: {}", e.message);
    }
    // one bound: the other is still named
    let e = run(PORTED, &[], json!({"@memory": "memory"}), false, &[]).unwrap_err();
    assert!(
        e.message.contains("@exec") && !e.message.contains("@memory ("),
        "{}",
        e.message
    );
}

#[test]
fn plan_refuses_bind_of_wrong_kind() {
    let e = run(
        PORTED,
        &[],
        json!({"@memory": "exec", "@exec": "exec"}),
        false,
        &[],
    )
    .unwrap_err();
    assert_eq!(e.reason, "bad_binding");
    for want in ["@memory", "memory agent", "exec (exec_bridge.tools)"] {
        assert!(e.message.contains(want), "{want}: {}", e.message);
    }
    // an agent that does not exist
    let e = run(
        PORTED,
        &[],
        json!({"@memory": "nope", "@exec": "exec"}),
        false,
        &[],
    )
    .unwrap_err();
    assert_eq!(e.reason, "bad_binding", "{}", e.message);
}

#[test]
fn plan_refuses_bind_of_missing_agent_naming_the_port() {
    let e = run(
        PORTED,
        &[],
        json!({"@memory": "nope", "@exec": "exec"}),
        false,
        &[],
    )
    .unwrap_err();
    assert_eq!(e.reason, "bad_binding");
    for want in ["@memory", "nope", "no agent"] {
        assert!(e.message.contains(want), "{want}: {}", e.message);
    }
}

#[test]
fn plan_refuses_bind_key_that_is_not_a_port() {
    let e = run(
        PORTED,
        &[],
        json!({"@memory": "memory", "@exec": "exec", "@model": "memory"}),
        false,
        &[],
    )
    .unwrap_err();
    assert_eq!(e.reason, "bad_binding");
    assert!(e.message.contains("@model"), "{}", e.message);
}

#[test]
fn plan_refuses_path_port_for_now() {
    let y = "
agents:
  - id: a
    handler_module: echo.tools
    cwd: '@path:cwd'
ports:
  '@path:cwd': {kind: path, why: x, used_by: [a]}
";
    let e = run(y, &[], json!({"@path:cwd": "exec"}), false, &[]).unwrap_err();
    assert_eq!(e.reason, "unbound_ports");
    assert!(
        e.message.contains("not bindable yet") && e.message.contains("@path:cwd"),
        "{}",
        e.message
    );
}

#[test]
fn plan_refuses_a_key_value_naming_agent_and_key_not_value() {
    let planted = "sk-planted0123456789";
    let cases = [
        (format!("agents:\n  - id: a\n    handler_module: echo.tools\n    api_key: {planted}\n"), vec![], "a api_key"),
        (format!("agents:\n  - id: a\n    handler_module: echo.tools\n    llm: {{opts: {{token: {planted}}}}}\n"), vec![], "a token"),
        ("agents:\n  - id: a\n    handler_module: echo.tools\n    rows: [{text: hi}]\n".to_string(), vec![], "a rows"),
        (
            "agents:\n  - id: a\n    handler_module: echo.tools\n    system_prompt_file: a/directive.md\n".to_string(),
            vec![("a/directive.md", "use sk-planted0123456789 please")],
            "a/directive.md",
        ),
    ];
    for (y, files, want) in cases {
        let e = run(&y, &files, json!({}), false, &[]).unwrap_err();
        assert_eq!(e.reason, "secret_found", "{y}");
        let (id, key) = want.split_once(' ').unwrap_or((want, ""));
        assert!(
            e.message.contains(id) && e.message.contains(key),
            "{want}: {}",
            e.message
        );
        assert!(!e.message.contains(planted), "{}", e.message);
        assert!(e.message.contains("export again"), "{}", e.message);
    }
    // a reference to a key is not a key
    let y = "agents:\n  - id: a\n    handler_module: echo.tools\n    credentials: {ref: r1}\n";
    assert!(run(y, &[], json!({}), false, &[]).is_ok());
}

#[test]
fn plan_refuses_absolute_path() {
    let y = "agents:\n  - id: a\n    handler_module: echo.tools\n    cwd: /private/x\n";
    let e = run(y, &[], json!({}), false, &[]).unwrap_err();
    assert_eq!(e.reason, "secret_found");
    assert!(
        e.message.contains("agent a field cwd: an absolute path"),
        "{}",
        e.message
    );
    assert!(!e.message.contains("/private"), "{}", e.message);
}

#[test]
fn plan_refuses_code_without_allow_code_and_accepts_with_it() {
    let y = "
agents:
  - id: a
    handler_module: echo.tools
    system_prompt_file: a/directive.md
    agents:
      - id: s
        handler_module: py_ab12.tools
";
    let files = [("a/directive.md", "be kind"), ("s/bundle.yaml", "name: s")];
    let e = run(y, &files, json!({}), false, &[]).unwrap_err();
    assert_eq!(e.reason, "code_not_allowed");
    for want in [
        "agent s",
        "py_ab12.tools",
        "file s/bundle.yaml",
        "allow_code: true",
    ] {
        assert!(e.message.contains(want), "{want}: {}", e.message);
    }
    assert!(!e.message.contains("directive.md"), "{}", e.message);
    let p = run(y, &files, json!({}), true, &[]).unwrap();
    assert_eq!(p.code.len(), 2, "{:?}", p.code);
    // a directive alone is not code
    let y = "agents:\n  - id: a\n    handler_module: echo.tools\n";
    assert!(run(y, &[("a/directive.md", "hi")], json!({}), false, &[])
        .unwrap()
        .code
        .is_empty());
}

#[test]
fn plan_refuses_parent_id_in_entry() {
    let y = "agents:\n  - id: a\n    handler_module: echo.tools\n    parent_id: home\n";
    let e = run(y, &[], json!({}), false, &[]).unwrap_err();
    assert_eq!(e.reason, "bad_arg");
    assert!(e.message.contains("parent_id"), "{}", e.message);
}

#[test]
fn plan_refuses_two_top_agents_or_missing_agents_key() {
    for y in [
        "agents:\n  - {id: a, handler_module: echo.tools}\n  - {id: b, handler_module: echo.tools}\n",
        "ports: {}\n",
        "agents: []\n",
        "not: [yaml",
    ] {
        let e = run(y, &[], json!({}), false, &[]).unwrap_err();
        assert_eq!(e.reason, "bad_arg", "{y}: {}", e.message);
    }
}

#[test]
fn plan_refuses_undeclared_port_and_unknown_outside_id() {
    let y = "agents:\n  - id: a\n    handler_module: echo.tools\n    can_call: {'@model': '*'}\n";
    let e = run(y, &[], json!({}), false, &[]).unwrap_err();
    assert!(
        e.reason == "bad_arg" && e.message.contains("@model"),
        "{}",
        e.message
    );
    // a literal outside id is neither the blueprint's nor a port
    let y = "agents:\n  - id: a\n    handler_module: echo.tools\n    can_call: {memory: '*'}\n";
    let e = run(y, &[], json!({}), false, &[]).unwrap_err();
    assert!(
        e.reason == "bad_arg" && e.message.contains("memory"),
        "{}",
        e.message
    );
}
