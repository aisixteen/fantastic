//! The ONE validation engine (issue #642) states what declarations need: a map of
//! declared objects (`additionalProperties` as a schema), a `pattern` on text, and a
//! declared default the engine applies and `reflect` shows; and a running program
//! declares its verbs at runtime (`DynRegistry`), checked by the same engine.

use fantastic_kernel::verbs::{self, validate, Row};
use fantastic_kernel::{DynRegistry, DynVerb, Verb};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// One route of a web agent.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Route {
    /// The agent the route reaches.
    pub target: String,
    /// How many seconds to wait.
    #[serde(default = "sixty")]
    pub wait: u32,
}
fn sixty() -> u32 {
    60
}

/// Declare routes and a store reference.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Wire {
    /// Routes by name.
    pub routes: BTreeMap<String, Route>,
    /// A plain map of text.
    pub labels: Option<BTreeMap<String, String>>,
    /// A store reference: a lowercase word.
    #[schemars(regex(pattern = "^[a-z]+$"))]
    pub store: String,
    /// The host to bind.
    #[serde(default = "loopback")]
    pub host: String,
    /// An optional count, no default.
    pub limit: Option<u32>,
    /// A non-empty map of text.
    #[serde(default)]
    #[schemars(schema_with = "some_text")]
    pub fields: Option<BTreeMap<String, String>>,
}
fn some_text(_gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
    serde_json::from_value(json!({
        "type": ["object", "null"],
        "additionalProperties": {"type": "string"},
        "minProperties": 1,
    }))
    .expect("a map schema")
}
fn loopback() -> String {
    "127.0.0.1".to_string()
}

static WIRE: Verb = Verb::of::<Wire>("wire");

fn call(v: Value) -> Result<Value, Value> {
    validate("wire", WIRE.schema(), &v)
}

fn refusal(v: Value) -> Value {
    call(v).expect_err("refused")
}

#[test]
fn a_map_of_declared_objects_is_checked_entry_by_entry() {
    let ok = call(json!({"type":"wire","store":"abc",
        "routes":{"a":{"target":"x"},"b":{"target":"y","wait":5}}}))
    .unwrap();
    assert_eq!(ok["routes"]["b"]["wait"], 5);

    let r = refusal(json!({"type":"wire","store":"abc","routes":{"a":{"wait":1}}}));
    assert_eq!(r["reason"], "missing_arg");
    assert_eq!(r["arg"], "routes.a.target");

    let r = refusal(json!({"type":"wire","store":"abc","routes":{"a":{"target":"x","extra":1}}}));
    assert_eq!(r["reason"], "bad_arg");
    assert_eq!(r["arg"], "routes.a.extra");

    let r = refusal(json!({"type":"wire","store":"abc","routes":{"a":{"target":"x","wait":"3"}}}));
    assert_eq!(r["arg"], "routes.a.wait");
    assert!(r["error"].as_str().unwrap().contains("must be int"));

    // an entry that is not an object at all
    let r = refusal(json!({"type":"wire","store":"abc","routes":{"a":"x"}}));
    assert_eq!(r["arg"], "routes.a");
    assert!(r["error"].as_str().unwrap().contains("must be object"));
}

#[test]
fn a_map_of_scalars_checks_each_value_type() {
    let ok = call(json!({"type":"wire","store":"abc","routes":{},"labels":{"k":"v"}}));
    assert!(ok.is_ok());
    let r = refusal(json!({"type":"wire","store":"abc","routes":{},"labels":{"k":5}}));
    assert_eq!(r["arg"], "labels.k");
    assert!(r["error"].as_str().unwrap().contains("must be string"));
    // the map itself must be an object
    let r = refusal(json!({"type":"wire","store":"abc","routes":[]}));
    assert_eq!(r["arg"], "routes");
}

#[test]
fn a_pattern_on_text_is_enforced_and_shown() {
    assert!(call(json!({"type":"wire","store":"abc","routes":{}})).is_ok());
    for bad in ["ABC", "a1", "", "a b"] {
        let r = refusal(json!({"type":"wire","store":bad,"routes":{}}));
        assert_eq!(r["arg"], "store", "{bad:?}");
    }
    let r = refusal(json!({"type":"wire","store":"a1","routes":{}}));
    assert!(r["error"].as_str().unwrap().contains("matching /^[a-z]+$/"));
}

#[test]
fn a_declared_default_is_applied_by_the_engine_and_shown() {
    let ok = call(json!({"type":"wire","store":"abc",
        "routes":{"a":{"target":"x"}}}))
    .unwrap();
    assert_eq!(ok["host"], "127.0.0.1");
    // nested: the default of an entry of a map
    assert_eq!(ok["routes"]["a"]["wait"], 60);
    // a value the call gave is kept
    let ok = call(json!({"type":"wire","store":"abc","routes":{},"host":"0.0.0.0"})).unwrap();
    assert_eq!(ok["host"], "0.0.0.0");
    // no default declared: nothing is invented
    assert!(ok.get("limit").is_none());

    let row = Row { name: "wire", schema: WIRE.schema() };
    let line = verbs::line(&row);
    assert!(line.contains("host:string (default \"127.0.0.1\")"), "{line}");
    assert!(line.contains("routes:map[object] (req)"), "{line}");
    assert!(line.contains("store:string matching /^[a-z]+$/ (req)"), "{line}");
    let d = verbs::detail(&row);
    assert!(d.contains("labels:map[string]?"), "{d}");
}

#[test]
fn defaults_are_applied_only_to_a_call_that_passed() {
    let r = refusal(json!({"type":"wire","store":"ABC","routes":{}}));
    assert_eq!(r["reason"], "bad_arg");
}

const PROGRAM_VERBS: &[&[Verb]] = &[];

#[test]
fn a_running_program_declares_verbs_the_same_engine_checks() {
    let reg = DynRegistry::new();
    assert!(reg.of("a1").is_empty());
    reg.declare(
        "a1",
        &json!([
            {"name":"say","description":"Say a thing. More.",
             "inputSchema":{"type":"object","properties":{"text":{"type":"string"},
                "n":{"type":"integer","default":2}},"required":["text"]}},
            {"name":"stop"}
        ]),
        PROGRAM_VERBS,
    )
    .unwrap();
    let dynamic = reg.of("a1");
    assert_eq!(dynamic.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["say", "stop"]);
    // other agents have none
    assert!(reg.of("a2").is_empty());

    let say: &DynVerb = &dynamic[0];
    let ok = validate("say", &say.schema, &json!({"type":"say","text":"hi"})).unwrap();
    assert_eq!(ok, json!({"text":"hi","n":2}));
    let r = validate("say", &say.schema, &json!({"type":"say"})).unwrap_err();
    assert_eq!(r["reason"], "missing_arg");
    let r = validate("say", &say.schema, &json!({"type":"say","text":1})).unwrap_err();
    assert_eq!(r["reason"], "bad_arg");
    let r = validate("stop", &dynamic[1].schema, &json!({"type":"stop","x":1})).unwrap_err();
    assert_eq!(r["arg"], "x");

    // a later declaration REPLACES the list; forget drops it
    reg.declare("a1", &json!([{"name":"only"}]), PROGRAM_VERBS).unwrap();
    assert_eq!(reg.of("a1").len(), 1);
    reg.forget("a1");
    assert!(reg.of("a1").is_empty());
}

#[test]
fn a_program_cannot_declare_a_reserved_or_repeated_or_malformed_verb() {
    let reg = DynRegistry::new();
    let reserved: &[&[Verb]] = &[&[Verb::of::<Wire>("wire")]];
    for (decl, why) in [
        (json!([{"name":"wire"}]), "already a verb"),
        (json!([{"name":"x"},{"name":"x"}]), "already a verb"),
        (json!([{"description":"no name"}]), "no 'name'"),
        (json!([{"name":""}]), "no 'name'"),
        (json!([{"name":"x","inputSchema":5}]), "must be an object"),
        (json!({"name":"x"}), "expected a list"),
    ] {
        let r = reg.declare("a1", &decl, reserved).unwrap_err();
        assert!(r["error"].as_str().unwrap().contains(why), "{decl}: {r}");
    }
    // a refused declaration leaves the agent's list as it was
    assert!(reg.of("a1").is_empty());
}

#[test]
fn min_properties_refuses_a_map_with_fewer_entries() {
    let base = json!({"type":"wire","store":"abc","routes":{}});
    let mut with = |f: Value| {
        let mut v = base.clone();
        v["fields"] = f;
        call(v)
    };
    let r = with(json!({})).expect_err("an empty map is below minProperties");
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(r["arg"], "fields", "{r}");
    assert!(r["error"].as_str().unwrap().contains("with at least 1 entry"), "{r}");
    assert!(with(json!({"a": "b"})).is_ok());
}
