//! Pure unit tests for the grant model's lines (`parse_can`) and the solver
//! (Cedar, per principal, with the ancestor cap in the gate). Without a line an
//! agent reaches only its own tree; a line opens what it names, on the target's
//! tree.

use super::*;
use serde_json::{json, Map, Value};

#[test]
fn absent_can_denies_everything_outside_the_own_tree() {
    let k = v9();
    // t1_tasks holds no line: nothing outside its own tree, whatever the verb
    for verb in ["send", "ping", "*"] {
        assert!(!ok(&k, "t1_tasks", &call(verb), "t1_exec"), "{verb}");
        assert!(!ok(&k, "t1_tasks", &call(verb), "team_1"), "{verb}");
    }
    assert!(ok(&k, "t1_tasks", &call("send"), "t1_tasks"));
}

#[test]
fn empty_can_denies() {
    let k = v9();
    set_can(&k, "t1_tasks", json!({}));
    assert!(!ok(&k, "t1_tasks", &call("send"), "t1_exec"));
    assert!(!ok(&k, "t1_tasks", &upd("description"), "t1_exec"));
}

#[test]
fn parent_key_is_call_level_only() {
    let k = v9();
    set_can(&k, "t1_tasks", json!({"$parent": {"call": ["*"]}}));
    assert!(ok(&k, "t1_tasks", &call("anything"), "team_1"));
    assert!(!ok(&k, "t1_tasks", &upd("description"), "team_1"));
    assert!(!ok(&k, "t1_tasks", &Act::Delete, "team_1"));
    assert!(!ok(&k, "t1_tasks", &call("send"), "canvas_root"));
    // a `$parent` line above call level is outside the legal shape
    for level in ["update", "create"] {
        let v = if level == "update" {
            json!({"$parent": {"update": ["x"]}})
        } else {
            json!({"$parent": {"create": {"who": ["*"], "what": "*"}}})
        };
        assert!(parse_can(&v).is_err(), "{level}");
    }
}

#[test]
fn call_star_gives_no_update_create_or_delete() {
    let k = v9();
    set_can(&k, "t1_tasks", json!({"kernel_state": {"call": ["*"]}}));
    for verb in ["create_agent", "update_agent", "delete_agent", "kernel_stop", "watch"] {
        assert!(ok(&k, "t1_tasks", &call(verb), "kernel_state"), "{verb}");
    }
    // `call *` is calling only: no level above it, anywhere on the root's tree
    for target in ["kernel_state", "user", "t1_exec"] {
        assert!(!ok(&k, "t1_tasks", &upd("description"), target), "{target}");
        assert!(!ok(&k, "t1_tasks", &crt("worker", "x_1"), target), "{target}");
        assert!(!ok(&k, "t1_tasks", &Act::Delete, target), "{target}");
    }
}

#[test]
fn per_verb_list_grants_exactly_those() {
    let k = v9();
    set_can(&k, "t1_tasks", json!({"t1_exec": {"call": ["read", "send"]}}));
    assert!(ok(&k, "t1_tasks", &call("read"), "t1_exec"));
    assert!(ok(&k, "t1_tasks", &call("send"), "t1_exec"));
    assert!(!ok(&k, "t1_tasks", &call("write"), "t1_exec"));
    assert!(!ok(&k, "t1_tasks", &call("read"), "t1_lead"));
}

#[test]
fn removed_tokens_match_nothing() {
    // `$self`, `$subtree` and every other `$` key but `$parent` are refused at the
    // write: no line can carry them
    for key in ["$loader", "$children", "$subtree:worker", "$self", "$subtree"] {
        let v = json!({ key: {"call": ["*"]} });
        assert!(parse_can(&v).is_err(), "{key}");
    }
}

#[test]
fn can_targets_are_exactly_the_legal_set() {
    for ok_key in ["$parent", "ai"] {
        let v = json!({ ok_key: {"call": ["ping"]} });
        assert!(parse_can(&v).is_ok(), "{ok_key}");
    }
    for bad in [
        "",
        "*",
        "$self",
        "$subtree",
        "$loader",
        "$subtree:x",
        "$anything",
        "|ai",
        "g1|",
        "a|b|c",
        "g1|ai",
        "g1|*",
    ] {
        let v = json!({ bad: {"call": ["ping"]} });
        assert!(parse_can(&v).is_err(), "{bad}");
    }
}

// ---- `can` lines (grant model v9, #754) ----

fn refused(can: Value) -> String {
    parse_can(&can).expect_err(&format!("refused: {can}"))
}

#[test]
fn a_can_line_outside_the_legal_shape_is_refused() {
    for bad in [
        // not a map of targets
        json!([]),
        json!("*"),
        json!(null),
        // a target that is `*`, empty, or a `$` token other than `$parent`
        json!({"*": {"call": ["ping"]}}),
        json!({"": {"call": ["ping"]}}),
        json!({"$self": {"call": ["ping"]}}),
        json!({"$subtree": {"call": ["ping"]}}),
        json!({"$anything": {"call": ["ping"]}}),
        // a target that does not match the id pattern
        json!({"a/b": {"call": ["ping"]}}),
        json!({".hidden": {"call": ["ping"]}}),
        json!({"123": {"call": ["ping"]}}),
        json!({"a..b": {"call": ["ping"]}}),
        // `$parent` at update or create level
        json!({"$parent": {"update": ["x"]}}),
        json!({"$parent": {"create": {"who": ["*"], "what": "*"}}}),
        // peer targets are parked
        json!({"g1|ai": {"call": ["ping"]}}),
        json!({"g1|*": {"call": ["ping"]}}),
        // a level key other than call/update/create
        json!({"ai": {"delete": ["*"]}}),
        json!({"ai": {"callable_by": ["*"]}}),
        // the entry is not a map
        json!({"ai": "*"}),
        json!({"ai": ["ping"]}),
        // a slot that is not a list of strings
        json!({"ai": {"call": "*"}}),
        json!({"ai": {"call": [1]}}),
        json!({"ai": {"update": {"x": 1}}}),
        json!({"ai": {"create": {"who": "*", "what": "*"}}}),
        json!({"ai": {"create": {"who": [1], "what": "*"}}}),
        json!({"ai": {"create": {"who": ["*"], "what": ["chat"]}}}),
        json!({"ai": {"create": {"who": ["*"], "what": "chat"}}}),
        json!({"ai": {"create": {"who": ["*"], "what": {"chat": "*"}}}}),
        json!({"ai": {"create": "*"}}),
        // create needs who and what, and nothing else
        json!({"ai": {"create": {"who": ["*"]}}}),
        json!({"ai": {"create": {"what": "*"}}}),
        json!({"ai": {"create": {"who": ["*"], "what": "*", "how": 1}}}),
        // a `who` with a `*` that is not trailing, or more than one `*`
        json!({"ai": {"create": {"who": ["*gbr"], "what": "*"}}}),
        json!({"ai": {"create": {"who": ["g*br"], "what": "*"}}}),
        json!({"ai": {"create": {"who": ["gbr**"], "what": "*"}}}),
        json!({"ai": {"create": {"who": ["**"], "what": "*"}}}),
        // a `who` whose stem does not match the id pattern
        json!({"ai": {"create": {"who": ["a/b*"], "what": "*"}}}),
        json!({"ai": {"create": {"who": ["a\\b"], "what": "*"}}}),
        json!({"ai": {"create": {"who": [".x*"], "what": "*"}}}),
        json!({"ai": {"create": {"who": [""], "what": "*"}}}),
        // a verb, field or bundle name holding `\` or a control character
        json!({"ai": {"call": ["pi\\ng"]}}),
        json!({"ai": {"call": ["pi\u{1}ng"]}}),
        json!({"ai": {"update": ["fi\nld"]}}),
        json!({"ai": {"create": {"who": ["*"], "what": {"ch\\at": {}}}}}),
        json!({"ai": {"create": {"who": ["*"], "what": {"chat": {"mo\u{1}del": "*"}}}}}),
    ] {
        let why = refused(bad.clone());
        assert!(!why.is_empty(), "{bad}");
    }
    // the peer refusal names the parking, not the id rule
    assert!(refused(json!({"g1|ai": {"call": ["ping"]}})).contains("peer"));
    assert!(refused(json!({"g1|*": {"call": ["ping"]}})).contains("peer"));
}

#[test]
fn a_target_is_never_a_wildcard() {
    for star in ["*", "g1|*"] {
        assert!(
            parse_can(&json!({star: {"call": ["*"]}})).is_err(),
            "{star}"
        );
    }
    // `*` belongs in slots: verbs, fields, ids, bundles
    assert!(parse_can(&json!({"ai": {"call": ["*"]}})).is_ok());
}

#[test]
fn one_level_per_target() {
    for two in [
        json!({"ai": {"call": ["ping"], "update": ["model"]}}),
        json!({"ai": {"call": ["ping"], "create": {"who": ["*"], "what": "*"}}}),
        json!({"ai": {}}),
    ] {
        assert!(refused(two.clone()).contains("exactly one"), "{two}");
    }
}

#[test]
fn legal_can_lines_parse() {
    let lines = parse_can(&json!({
        "$parent": {"call": ["send"]},
        "ai": {"call": ["ping", "*"]},
        "files": {"update": ["root", "*"]},
        "garage": {"create": {"who": ["*"], "what": "*"}},
        "lead": {"create": {"who": ["gbr_*", "w1"], "what": {
            "*": {"*": "*"},
            "chat": {"model": "haiku", "max_tokens": 260000, "*": "*"},
            "file_bridge": {"root": "*"}
        }}},
        "ws": {"create": {"who": ["*"], "what": {"chat": {}}}}
    }))
    .expect("legal");
    assert_eq!(lines.len(), 6);
    assert_eq!(
        lines[0],
        Line {
            target: Target::Parent,
            level: Level::Call(vec!["send".into()])
        }
    );
    assert_eq!(
        lines[1],
        Line {
            target: Target::Id("ai".into()),
            level: Level::Call(vec!["ping".into(), "*".into()])
        }
    );
    assert_eq!(
        lines[2],
        Line {
            target: Target::Id("files".into()),
            level: Level::Update(vec!["root".into(), "*".into()])
        }
    );
    assert_eq!(
        lines[3],
        Line {
            target: Target::Id("garage".into()),
            level: Level::Create {
                who: vec!["*".into()],
                what: What::All
            }
        }
    );
    let Level::Create { who, what } = &lines[4].level else {
        panic!("create: {:?}", lines[4]);
    };
    assert_eq!(who, &vec!["gbr_*".to_string(), "w1".to_string()]);
    let What::Bundles(b) = what else {
        panic!("bundles: {what:?}");
    };
    // a `*` bundle key and a `*` field key are kept as given
    assert_eq!(b["*"]["*"], json!("*"));
    // a forced value is kept as given
    assert_eq!(b["chat"]["model"], json!("haiku"));
    assert_eq!(b["chat"]["max_tokens"], json!(260000));
    assert_eq!(b["chat"]["*"], json!("*"));
    assert_eq!(b["file_bridge"]["root"], json!("*"));
    assert_eq!(
        lines[5].level,
        Level::Create {
            who: vec!["*".into()],
            what: What::Bundles([("chat".to_string(), Map::new())].into_iter().collect())
        }
    );
    // an empty `can` holds no line
    assert_eq!(parse_can(&json!({})).expect("empty"), vec![]);
}

// ---------------------------------------------------------------- the solver index (U4)

use super::solver::{generic, line_policies, lines_of, Act, Slot};
use crate::agent::{Agent, AgentId};
use crate::kernel::Kernel;
use std::collections::HashMap;

/// A verb no line names: allowed only where every verb is.
const PROBE: &str = "\u{1}probe";

fn v9() -> Kernel {
    let k = Kernel::new();
    crate::test_support::v9_tree(&k);
    k
}

fn aid(id: &str) -> AgentId {
    AgentId::from(id)
}

fn call(v: &str) -> Act {
    Act::Call(v.into())
}

fn upd(f: &str) -> Act {
    Act::Update(f.into())
}

fn crt(bundle: &str, new_id: &str) -> Act {
    Act::Create {
        bundle: bundle.into(),
        new_id: new_id.into(),
    }
}

/// `principal`'s own set (no cap) allows `act` on `resource`.
fn ok(k: &Kernel, principal: &str, act: &Act, resource: &str) -> bool {
    k.cedar(&aid(principal), act, &aid(resource))
}

/// Replace `id`'s whole `can`.
fn set_can(k: &Kernel, id: &str, can: Value) {
    k.edit_can(&aid(id), None, |c, _| {
        *c = can.as_object().cloned().unwrap_or_default();
        Ok(true)
    })
    .expect("agent exists");
}

#[test]
fn position_reaches_own_tree_at_call_level_only() {
    let k = v9();
    // team_1 holds no line in its own tree: position alone
    assert!(ok(&k, "team_1", &call("anything"), "team_1"));
    assert!(ok(&k, "team_1", &call("anything"), "t1_lead"));
    assert!(ok(&k, "team_1", &call(PROBE), "t1_w1"));
    assert!(!ok(&k, "team_1", &upd("description"), "t1_w1"));
    assert!(!ok(&k, "team_1", &upd("can"), "t1_lead"));
    assert!(!ok(&k, "team_1", &Act::Delete, "t1_w1"));
    assert!(!ok(&k, "team_1", &crt("worker", "t1_w9"), "t1_lead"));
    assert!(!ok(&k, "team_1", &crt("worker", "t1_w9"), "team_1"));
}

#[test]
fn nothing_outside_own_tree_without_a_line_parent_included() {
    let k = v9();
    // a sibling
    assert!(!ok(&k, "t1_w1", &call("send"), "t1_w2"));
    // the parent, with no `$parent` line
    assert!(!ok(&k, "t1_lead", &call("send"), "team_1"));
    assert!(!ok(&k, "t1_tasks", &call("send"), "team_1"));
    // the root
    assert!(!ok(&k, "t1_tasks", &call("send"), "kernel_state"));
    // agents no line names
    assert!(!ok(&k, "chat_a", &call("get"), "credentials"));
    assert!(!ok(&k, "chat_b", &call("complete"), "prov_1"));
    assert!(!ok(&k, "t1_w1", &call("issues"), "github"));
}

#[test]
fn a_line_on_a_target_reaches_its_whole_tree() {
    let k = v9();
    // canvas_ws: files {call: [read, list, write]}, and static_files is under files
    assert!(ok(&k, "canvas_ws", &call("write"), "files"));
    assert!(ok(&k, "canvas_ws", &call("write"), "static_files"));
    assert!(ok(&k, "canvas_ws", &call("read"), "static_files"));
    assert!(!ok(&k, "canvas_ws", &call("delete"), "static_files"));
    assert!(!ok(&k, "canvas_ws", &call("read"), "exec"));
    assert!(!ok(&k, "canvas_ws", &upd("root"), "static_files"));
    // a line on the whole user space reaches agents at any depth in it
    set_can(&k, "t1_tasks", json!({"canvas_root": {"call": ["ping"]}}));
    for deep in ["canvas_root", "chat_a", "canvas_garage", "prov_1", "t1_w2"] {
        assert!(ok(&k, "t1_tasks", &call("ping"), deep), "{deep}");
        assert!(!ok(&k, "t1_tasks", &call("send"), deep), "{deep}");
    }
    assert!(!ok(&k, "t1_tasks", &call("ping"), "user"));
}

#[test]
fn update_includes_call_on_every_verb_and_create_includes_update_below_the_target() {
    let k = v9();
    set_can(
        &k,
        "t1_tasks",
        json!({"github": {"update": ["description"]}}),
    );
    assert!(ok(&k, "t1_tasks", &upd("description"), "github"));
    assert!(!ok(&k, "t1_tasks", &upd("model"), "github"));
    assert!(ok(&k, "t1_tasks", &call("issues"), "github"));
    assert!(ok(&k, "t1_tasks", &call(PROBE), "github"));
    assert!(!ok(&k, "t1_tasks", &Act::Delete, "github"));
    assert!(!ok(&k, "t1_tasks", &crt("chat", "cell_1"), "github"));
    // canvas_ws: canvas_root {create: {who: ["*"], what: "*"}}
    assert!(ok(&k, "canvas_ws", &upd("model"), "chat_a"));
    assert!(ok(&k, "canvas_ws", &call(PROBE), "chat_a"));
    assert!(ok(&k, "canvas_ws", &Act::Delete, "chat_a"));
    assert!(ok(&k, "canvas_ws", &upd("endpoint"), "prov_1"));
    assert!(ok(&k, "canvas_ws", &call("complete"), "prov_1"));
}

#[test]
fn parent_key_names_only_the_parent_at_call_level() {
    let k = v9();
    // t1_w1: $parent {call: [send]}; t1_lead is its parent
    assert!(ok(&k, "t1_w1", &call("send"), "t1_lead"));
    assert!(!ok(&k, "t1_w1", &call("other"), "t1_lead"));
    assert!(!ok(&k, "t1_w1", &call("send"), "team_1"));
    assert!(!ok(&k, "t1_w1", &call("send"), "t1_w2"));
    assert!(!ok(&k, "t1_w1", &upd("description"), "t1_lead"));
    assert!(!ok(&k, "t1_w1", &crt("worker", "t1_w9"), "t1_lead"));
    assert!(!ok(&k, "t1_w1", &Act::Delete, "t1_lead"));
    assert!(!ok(&k, "t1_w1", &call("send"), "canvas_root"));
}

#[test]
fn who_prefix_matches_one_trailing_star() {
    let k = v9();
    // t1_lead: t1_lead {create: {who: ["t1_w*"], what: {worker: {}}}}
    assert!(ok(&k, "t1_lead", &crt("worker", "t1_w3"), "t1_lead"));
    assert!(ok(&k, "t1_lead", &crt("worker", "t1_w"), "t1_lead"));
    assert!(ok(&k, "t1_lead", &crt("worker", "t1_w3_x"), "t1_lead"));
    assert!(!ok(&k, "t1_lead", &crt("worker", "x_w3"), "t1_lead"));
    assert!(!ok(&k, "t1_lead", &crt("worker", "t1_x3"), "t1_lead"));
    assert!(!ok(&k, "t1_lead", &crt("exec_bridge", "t1_w4"), "t1_lead"));
    assert!(!ok(&k, "t1_lead", &crt("worker", "t1_w3"), "team_1"));
    // a literal id matches itself only
    set_can(
        &k,
        "t1_lead",
        json!({"t1_lead": {"create": {"who": ["t1_w1"], "what": {"worker": {}}}}}),
    );
    assert!(ok(&k, "t1_lead", &Act::Delete, "t1_w1"));
    assert!(!ok(&k, "t1_lead", &Act::Delete, "t1_w2"));
    assert!(!ok(&k, "t1_lead", &crt("worker", "t1_w10"), "t1_lead"));
}

#[test]
fn nothing_changes_or_deletes_its_own_record_or_an_ancestors() {
    let k = v9();
    // a, b
    assert!(!ok(&k, "chat_a", &upd("can"), "chat_a"));
    assert!(!ok(&k, "chat_a", &upd("can"), "canvas_root"));
    // a', b': lines that would give it
    set_can(
        &k,
        "chat_a",
        json!({"chat_a": {"update": ["can"]}, "canvas_root": {"update": ["can"]}}),
    );
    assert!(!ok(&k, "chat_a", &upd("can"), "chat_a"));
    assert!(!ok(&k, "chat_a", &Act::Delete, "chat_a"));
    assert!(!ok(&k, "chat_a", &upd("can"), "canvas_root"));
    assert!(!ok(&k, "chat_a", &Act::Delete, "canvas_root"));
    set_can(
        &k,
        "chat_a",
        json!({"canvas_root": {"create": {"who": ["*"], "what": "*"}}}),
    );
    assert!(!ok(&k, "chat_a", &upd("can"), "chat_a"));
    assert!(!ok(&k, "chat_a", &Act::Delete, "chat_a"));
    assert!(!ok(&k, "chat_a", &upd("can"), "canvas_root"));
    assert!(!ok(&k, "chat_a", &Act::Delete, "canvas_root"));
    assert!(!ok(&k, "chat_a", &upd("can"), "user"));
    // the same line still works on a sibling
    assert!(ok(&k, "chat_a", &upd("can"), "chat_b"));
    assert!(ok(&k, "chat_a", &Act::Delete, "chat_b"));
}

#[test]
fn a_create_line_gives_nothing_on_its_own_target() {
    let k = v9();
    // canvas_ws: canvas_root {create: {who: ["*"], what: "*"}}
    for verb in ["send", "create", "reflect", PROBE] {
        assert!(!ok(&k, "canvas_ws", &call(verb), "canvas_root"), "{verb:?}");
    }
    for field in ["can", "description", "model"] {
        assert!(!ok(&k, "canvas_ws", &upd(field), "canvas_root"), "{field}");
    }
    assert!(!ok(&k, "canvas_ws", &Act::Delete, "canvas_root"));
    assert!(!k.cedar_all(&aid("canvas_ws"), Slot::Verb, &aid("canvas_root"), false));
    // below it, the line reaches
    assert!(ok(&k, "canvas_ws", &call("send"), "chat_a"));
}

#[test]
fn a_create_line_creates_anywhere_in_its_targets_tree() {
    let k = v9();
    assert!(ok(&k, "canvas_ws", &crt("chat", "cell_1"), "canvas_root"));
    assert!(ok(&k, "canvas_ws", &crt("chat", "cell_1"), "canvas_garage"));
    assert!(ok(
        &k,
        "canvas_ws",
        &crt("exec_bridge", "exec_2"),
        "canvas_garage"
    ));
    assert!(ok(&k, "canvas_ws", &crt("chat", "cell_1"), "team_1"));
    assert!(!ok(&k, "canvas_ws", &crt("chat", "cell_1"), "user"));
    assert!(!ok(&k, "canvas_ws", &crt("chat", "cell_1"), "files"));
    // canvas_root's own create line reaches into the garage too, for its bundles only
    assert!(ok(
        &k,
        "canvas_root",
        &crt("chat", "cell_2"),
        "canvas_garage"
    ));
    assert!(!ok(
        &k,
        "canvas_root",
        &crt("exec_bridge", "exec_2"),
        "canvas_garage"
    ));
}

#[test]
fn a_forced_field_is_not_updatable_through_the_create_line() {
    let k = v9();
    // canvas_garage: anthropic_backend {can: {...} (forced), endpoint: "*", model: "*"}
    assert!(ok(&k, "canvas_garage", &upd("endpoint"), "prov_1"));
    assert!(ok(&k, "canvas_garage", &upd("model"), "prov_1"));
    assert!(!ok(&k, "canvas_garage", &upd("can"), "prov_1"));
    assert!(!ok(&k, "canvas_garage", &upd("description"), "prov_1"));
    // exec_bridge {allowed: "*"}
    assert!(ok(&k, "canvas_garage", &upd("allowed"), "exec_1"));
    assert!(!ok(&k, "canvas_garage", &upd("root"), "exec_1"));
    // a forced value on a plain field
    set_can(
        &k,
        "canvas_garage",
        json!({"canvas_garage": {"create": {"who": ["*"], "what": {
            "anthropic_backend": {"model": "m1", "endpoint": "*"}
        }}}}),
    );
    assert!(!ok(&k, "canvas_garage", &upd("model"), "prov_1"));
    assert!(ok(&k, "canvas_garage", &upd("endpoint"), "prov_1"));
}

#[test]
fn a_create_line_gives_no_later_can_update_unless_what_frees_can() {
    let k = v9();
    // canvas_root: what {chat: {model: "*"}, canvas: {}}
    assert!(ok(&k, "canvas_root", &upd("model"), "chat_a"));
    assert!(!ok(&k, "canvas_root", &upd("can"), "chat_a"));
    assert!(!ok(&k, "canvas_root", &upd("can"), "canvas_garage"));
    assert!(!ok(&k, "canvas_garage", &upd("can"), "exec_1"));
    // what: "*" frees every field, `can` included
    assert!(ok(&k, "canvas_ws", &upd("can"), "chat_a"));
    // a what that frees `can`
    set_can(
        &k,
        "team_1",
        json!({"t1_lead": {"create": {"who": ["*"], "what": {"worker": {"can": "*"}}}}}),
    );
    assert!(ok(&k, "team_1", &upd("can"), "t1_w1"));
    assert!(!ok(&k, "team_1", &upd("model"), "t1_w1"));
}

#[test]
fn a_star_what_never_covers_a_bare_agent() {
    let k = v9();
    let bare = Agent::new(
        aid("bare_1"),
        None,
        Some(aid("canvas_root")),
        Map::new(),
        std::path::PathBuf::new(),
        true,
    );
    k.register(bare);
    let refused = |k: &Kernel, who: &str| {
        assert!(!ok(k, who, &crt("", "bare_2"), "canvas_root"), "{who}");
        assert!(!ok(k, who, &call("send"), "bare_1"), "{who}");
        assert!(!ok(k, who, &upd("description"), "bare_1"), "{who}");
        assert!(!ok(k, who, &upd("can"), "bare_1"), "{who}");
        assert!(!ok(k, who, &Act::Delete, "bare_1"), "{who}");
        // the same line covers an agent of a bundle
        assert!(ok(k, who, &Act::Delete, "chat_b"), "{who}");
        assert!(ok(k, who, &upd("description"), "chat_b"), "{who}");
    };
    // what: "*"
    refused(&k, "canvas_ws");
    // a "*" bundle key
    set_can(
        &k,
        "t1_tasks",
        json!({"canvas_root": {"create": {"who": ["*"], "what": {"*": {"*": "*"}}}}}),
    );
    refused(&k, "t1_tasks");
    // position is not a create line: the parent still calls it
    assert!(ok(&k, "canvas_root", &call("send"), "bare_1"));
}

#[test]
fn structural_fields_are_forbidden_to_every_agent() {
    let k = v9();
    set_can(&k, "t1_tasks", json!({"github": {"update": ["*"]}}));
    for field in ["id", "parent", "handler_module"] {
        assert!(!ok(&k, "canvas_ws", &upd(field), "chat_a"), "{field}");
        assert!(!ok(&k, "t1_tasks", &upd(field), "github"), "{field}");
        assert!(!ok(&k, "canvas_root", &upd(field), "chat_a"), "{field}");
        assert!(!ok(&k, "team_1", &upd(field), "t1_w1"), "{field}");
    }
    assert!(ok(&k, "canvas_ws", &upd("model"), "chat_a"));
    assert!(ok(&k, "t1_tasks", &upd("description"), "github"));
}

#[test]
fn star_detection_by_partial_eval_matches_a_full_verb_probe() {
    let k = v9();
    set_can(
        &k,
        "t1_tasks",
        json!({"github": {"update": ["description"]}}),
    );
    let cases = [
        ("chat_a", "memory", false),
        ("chat_a", "credentials", false),
        ("canvas_ws", "chat_a", true),
        ("canvas_ws", "canvas_root", false),
        ("team_1", "t1_w1", true),
        ("t1_w1", "t1_lead", false),
        ("web", "static_files", false),
        ("t1_tasks", "github", true),
        ("t1_tasks", "missing_id", false),
    ];
    for (p, r, star) in cases {
        let all = k.cedar_all(&aid(p), Slot::Verb, &aid(r), false);
        assert_eq!(all, ok(&k, p, &call(PROBE), r), "{p} -> {r}");
        assert_eq!(all, star, "{p} -> {r}");
    }
}

#[test]
fn the_written_set_holds_no_position() {
    let k = v9();
    let (team, w1) = (aid("team_1"), aid("t1_w1"));
    assert!(k.cedar(&team, &call("send"), &w1));
    assert!(!k.cedar_written(&team, &call("send"), &w1));
    assert!(!k.cedar_written(&team, &call("send"), &team));
    assert!(k.cedar_all(&team, Slot::Verb, &w1, false));
    assert!(!k.cedar_all(&team, Slot::Verb, &w1, true));
    // written lines are held
    assert!(k.cedar_written(&team, &call("issues"), &aid("github")));
    assert!(k.cedar_written(&aid("chat_a"), &call("send"), &aid("canvas_root")));
    assert!(k.cedar_written(
        &aid("canvas_ws"),
        &crt("chat", "cell_1"),
        &aid("canvas_root")
    ));
    // a principal with no lines holds nothing written
    assert!(!k.cedar_written(&aid("t1_tasks"), &call("send"), &aid("t1_tasks")));
    // the forbids stay
    set_can(&k, "chat_a", json!({"chat_a": {"update": ["can"]}}));
    assert!(!k.cedar_written(&aid("chat_a"), &upd("can"), &aid("chat_a")));
}

#[test]
fn a_can_change_rebuilds_one_principals_set_and_decides_the_same_as_a_global_set() {
    let k = v9();
    let ids = k.agent_ids();
    for p in &ids {
        k.cedar(p, &Act::Delete, p);
    }
    let before: HashMap<AgentId, Value> = k
        .authz
        .read()
        .expect("index")
        .per
        .iter()
        .map(|(id, (v, _))| (id.clone(), v.clone()))
        .collect();
    assert_eq!(before.len(), ids.len());
    let (chat_a, github) = (aid("chat_a"), aid("github"));
    assert!(!k.cedar(&chat_a, &call("issues"), &github));
    let new_can = json!({
        "$parent": {"call": ["send"]},
        "github": {"call": ["issues"]},
    });
    set_can(&k, "chat_a", new_can.clone());
    assert!(k.cedar(&chat_a, &call("issues"), &github));
    assert!(!k.cedar(&chat_a, &call("append"), &aid("memory")));
    {
        let ix = k.authz.read().expect("index");
        assert_eq!(ix.per.len(), before.len());
        for (id, v) in &before {
            let built = &ix.per[id].0;
            if *id == chat_a {
                assert_eq!(built, &new_can);
            } else {
                assert_eq!(built, v, "{id:?} was rebuilt");
            }
        }
    }
    // one global set of every principal's lines decides the same
    let mut text = generic();
    for id in &ids {
        let can = k
            .agents
            .get(id)
            .expect("agent")
            .meta
            .read()
            .expect("meta")
            .get("can")
            .cloned();
        text += &line_policies(id.as_str(), &lines_of(&can.unwrap_or(Value::Null)));
    }
    let global: cedar_policy::PolicySet = text.parse().expect("global set parses");
    let acts = [
        call("send"),
        call("issues"),
        call("get"),
        call(PROBE),
        upd("can"),
        upd("model"),
        upd("id"),
        crt("chat", "cell_9"),
        crt("worker", "t1_w9"),
        crt("", "bare_9"),
        Act::Delete,
    ];
    for p in &ids {
        for r in &ids {
            for act in &acts {
                let local = k.cedar(p, act, r);
                let whole = k.authz.read().expect("index").decide(&global, p, act, r);
                assert_eq!(local, whole, "{p:?} {act:?} {r:?}");
            }
        }
    }
}

// ---------------------------------------------------------------- the ancestor cap and held (U5)

use crate::send::gate::Held;

/// `caller` may do `act` on `target`, the ancestor cap included.
fn allowed(k: &Kernel, caller: &str, act: &Act, target: &str) -> bool {
    let t = k
        .agents
        .get(&aid(target))
        .map(|e| std::sync::Arc::clone(&e));
    let t = t.expect("target registered");
    k.allowed(&aid(caller), act, &t)
}

fn held(k: &Kernel, caller: &str, target: &str) -> Held {
    k.held(&aid(caller), &aid(target))
}

#[test]
fn cap_skips_an_ancestor_whose_tree_holds_the_target() {
    let k = v9();
    // t1_lead creates under itself: team_1 holds no create line, but its tree
    // holds the target, so it is skipped
    assert!(!ok(&k, "team_1", &crt("worker", "t1_w3"), "t1_lead"));
    assert!(allowed(&k, "t1_lead", &crt("worker", "t1_w3"), "t1_lead"));
    // canvas_ws creates under canvas_root: web caps it (its tree does not hold
    // canvas_root, its line does), user is skipped (its tree holds canvas_root)
    assert!(!ok(&k, "user", &crt("chat", "cell_1"), "canvas_root"));
    assert!(allowed(
        &k,
        "canvas_ws",
        &crt("chat", "cell_1"),
        "canvas_root"
    ));
    // canvas_garage updates prov_1: every ancestor's tree holds it, the root too
    assert!(!ok(&k, "canvas_root", &upd("endpoint"), "prov_1"));
    assert!(allowed(&k, "canvas_garage", &upd("endpoint"), "prov_1"));
    // the root holds no line: its tree holds every agent, so it caps nothing
    assert!(!ok(&k, "kernel_state", &upd("description"), "exec"));
    set_can(&k, "github", json!({"exec": {"update": ["description"]}}));
    assert!(allowed(&k, "github", &upd("description"), "exec"));
}

#[test]
fn cap_applies_for_every_ancestor() {
    let k = v9();
    // f: t1_w1 holds no line on github; t1_lead does, and team_1 and
    // canvas_root cap it with the same line
    assert!(!allowed(&k, "t1_w1", &call("issues"), "github"));
    assert!(allowed(&k, "t1_lead", &call("issues"), "github"));
    assert!(!allowed(&k, "t1_lead", &call("delete_repo"), "github"));
    assert!(k.may_call(&aid("t1_lead"), "issues", &aid("github")));
    assert!(!k.may_call(&aid("t1_w1"), "issues", &aid("github")));
    // j: canvas_root loses its github line; Cedar alone still allows t1_lead
    // (its line is unchanged), the cap refuses it
    set_can(
        &k,
        "canvas_root",
        json!({"memory": {"call": ["append", "read"]}}),
    );
    assert!(ok(&k, "t1_lead", &call("issues"), "github"));
    assert!(!allowed(&k, "t1_lead", &call("issues"), "github"));
    assert!(!k.may_call(&aid("t1_lead"), "issues", &aid("github")));
    // the nearer ancestor caps the same way
    set_can(&k, "canvas_root", json!({"github": {"call": ["issues"]}}));
    assert!(allowed(&k, "t1_lead", &call("issues"), "github"));
    set_can(&k, "team_1", json!({}));
    assert!(!allowed(&k, "t1_lead", &call("issues"), "github"));
    // h': web loses its canvas_root line; canvas_ws's own set still allows
    set_can(
        &k,
        "web",
        json!({"files": {"call": ["read", "list", "write"]}}),
    );
    assert!(ok(&k, "canvas_ws", &crt("chat", "cell_1"), "canvas_root"));
    assert!(!allowed(
        &k,
        "canvas_ws",
        &crt("chat", "cell_1"),
        "canvas_root"
    ));
    assert!(!allowed(&k, "canvas_ws", &Act::Delete, "chat_a"));
    // the written form: the caller's lines only, the ancestors still cap
    let k = v9();
    let w1 = k
        .agents
        .get(&aid("t1_w1"))
        .map(|e| std::sync::Arc::clone(&e));
    let w1 = w1.expect("t1_w1");
    let github = k
        .agents
        .get(&aid("github"))
        .map(|e| std::sync::Arc::clone(&e));
    let github = github.expect("github");
    assert!(k.allowed(&aid("team_1"), &call("send"), &w1));
    assert!(!k.allowed_written(&aid("team_1"), &call("send"), &w1));
    assert!(k.allowed_written(&aid("t1_lead"), &call("issues"), &github));
    set_can(&k, "team_1", json!({}));
    assert!(!k.allowed_written(&aid("t1_lead"), &call("issues"), &github));
}

#[test]
fn reflect_candidates_from_own_and_ancestor_lines_equal_the_whole_model() {
    let k = v9();
    set_can(
        &k,
        "t1_tasks",
        json!({"github": {"update": ["description"]}}),
    );
    set_can(&k, "github", json!({"exec": {"update": ["description"]}}));
    // levels the tree gives
    assert_eq!(held(&k, "team_1", "t1_w1"), Held::Own);
    assert_eq!(held(&k, "team_1", "team_1"), Held::Own);
    assert_eq!(held(&k, "canvas_ws", "canvas_root"), Held::Create);
    assert_eq!(held(&k, "canvas_ws", "chat_a"), Held::Create);
    assert_eq!(held(&k, "t1_lead", "t1_w1"), Held::Own);
    assert_eq!(held(&k, "github", "exec"), Held::Update);
    // t1_tasks holds update on github, team_1 caps it to call
    assert_eq!(held(&k, "t1_tasks", "github"), Held::Call);
    assert_eq!(held(&k, "chat_a", "prov_1"), Held::Call);
    assert_eq!(held(&k, "t1_w1", "t1_lead"), Held::Call);
    assert_eq!(held(&k, "chat_a", "credentials"), Held::Nothing);
    assert_eq!(held(&k, "t1_w1", "github"), Held::Nothing);
    assert_eq!(held(&k, "t1_w1", "t1_w2"), Held::Nothing);
    // the candidates of the caller's and its ancestors' lines find what the
    // literals of every line in the model find
    let ids = k.agent_ids();
    let whole = k.candidates_of(&ids);
    for p in &ids {
        for r in &ids {
            assert_eq!(k.held(p, r), k.held_from(p, r, &whole), "{p:?} on {r:?}");
        }
    }
}

#[test]
fn held_on_a_missing_or_remote_id_does_not_panic() {
    let k = v9();
    for caller in ["chat_a", "canvas_ws", "kernel_state", "missing_caller"] {
        assert_eq!(held(&k, caller, "missing_id"), Held::Nothing, "{caller}");
        assert_eq!(held(&k, caller, "g1|x"), Held::Nothing, "{caller}");
    }
    assert_eq!(held(&k, "missing_caller", "chat_a"), Held::Nothing);
    assert!(!k.may_call(&aid("t1_lead"), "issues", &aid("missing_id")));
    assert!(!k.may_call(&aid("missing_caller"), "issues", &aid("github")));
    // a remote target is refused for every agent caller
    let remote = Agent::remote(
        aid("g1|x"),
        crate::peers::RemoteRef {
            guid: "g1".to_string(),
            agent_id: "x".to_string(),
            routes: Vec::new(),
        },
    );
    for caller in ["canvas_ws", "kernel_state", "t1_lead"] {
        assert!(!k.allowed(&aid(caller), &call("send"), &remote), "{caller}");
        assert!(
            !k.allowed_written(&aid(caller), &call("send"), &remote),
            "{caller}"
        );
    }
}
