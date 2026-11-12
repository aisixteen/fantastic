//! Verbs declared once (issue #642): each agent checks its own calls against its
//! bundle's declaration (the shared entry `Call::parse`) and replies with the
//! refusal; `reflect` and `docs` render the same declarations, builtin and dynamic.
//! The probe bundle below is written the way every bundle is: structs, `verbs!`,
//! `record!`, and a `handle` whose first act is the entry.

use async_trait::async_trait;
use fantastic_kernel::{Agent, AgentId, BundleRegistry, Handler, Item, Kernel, Reply};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::sync::Arc;
use tempfile::TempDir;

/// Say hello to someone, a number of times.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Greet {
    /// Who to greet.
    pub name: String,
    /// How many times; one by default.
    #[serde(default = "one")]
    pub times: u32,
    /// `loud` shouts.
    pub style: Option<Style>,
}

fn one() -> u32 {
    1
}

/// How a greeting is said.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Style {
    /// Plain.
    Calm,
    /// Shouted.
    Loud,
}

/// Count what was said. No arguments.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Tally {}

mod probe_verbs {
    use super::*;
    fantastic_kernel::verbs! {
        "greet" => Greet,
        "tally" => Tally,
    }
}

/// The record of a probe agent.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProbeRecord {
    /// The greeting's first word.
    pub word: Option<String>,
    /// How many greetings may be said in total.
    pub limit: Option<u32>,
}

mod probe_record {
    use super::*;
    fantastic_kernel::record!(ProbeRecord);
}

struct Probe;

/// How many calls reached a probe's handler, per kernel (the tests run side by side).
static HANDLED: std::sync::Mutex<Vec<(usize, usize)>> = std::sync::Mutex::new(Vec::new());

fn handled_by(kernel: &Arc<Kernel>) -> usize {
    let key = Arc::as_ptr(kernel) as usize;
    HANDLED
        .lock()
        .unwrap()
        .iter()
        .find(|(k, _)| *k == key)
        .map_or(0, |(_, n)| *n)
}

/// A bundle whose agents carry verbs built at runtime: the record's `tools` list
/// (`{name, description, schema}`), the way an MCP client builds one verb per tool.
struct Tools;

/// What the dynamic probe declares itself: one builtin verb.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Tools0 {}

mod tools_verbs {
    use super::*;
    fantastic_kernel::verbs! {
        dynamic;
        "tools" => Tools0,
    }
}

/// The record of a dynamic probe agent.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ToolsRecord {
    /// The tools this agent offers: `{name, description, schema}` each.
    pub tools: Option<Vec<Value>>,
}
mod tools_record {
    use super::*;
    fantastic_kernel::record!(ToolsRecord);
}

#[async_trait]
impl Handler for Tools {
    type Call = tools_verbs::Call;
    fn man(&self) -> &'static str {
        "A test stand-in."
    }
    fn name(&self) -> &str {
        "tools_probe"
    }
    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        tools_verbs::VERBS
    }
    fn dynamic_verbs(&self, agent: &Agent) -> Vec<fantastic_kernel::DynVerb> {
        let meta = agent.meta.read().unwrap();
        meta.get("tools")
            .and_then(Value::as_array)
            .map(|tools| {
                tools
                    .iter()
                    .map(|t| {
                        fantastic_kernel::DynVerb::new(
                            t["name"].as_str().unwrap_or(""),
                            t["description"].as_str().unwrap_or(""),
                            t["schema"].as_object().expect("a schema object"),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        &tools_record::RECORD
    }
    async fn handle(
        &self,
        id: &AgentId,
        call: Self::Call,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, Box<dyn std::error::Error + Send + Sync>> {
        match call {
            tools_verbs::Call::Reflect(_) => Ok(Some(json!({"id": id.0, "sentence": "Tools."}))),
            tools_verbs::Call::Tools0(_) => {
                let agent = kernel.agents.get(id).map(|e| Arc::clone(&e)).unwrap();
                Ok(Some(
                    json!({"tools": Handler::dynamic_verbs(self, &agent).len()}),
                ))
            }
            tools_verbs::Call::Dynamic(c) => Ok(Some(json!({"ran": c.name, "args": c.args}))),
        }
    }
}

#[async_trait]
impl Handler for Probe {
    type Call = probe_verbs::Call;
    fn man(&self) -> &'static str {
        "A test stand-in."
    }
    fn name(&self) -> &str {
        "probe"
    }
    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        probe_verbs::VERBS
    }
    fn dynamic_verbs(&self, agent: &Agent) -> Vec<fantastic_kernel::DynVerb> {
        probe_verbs::dynamic_verbs(agent)
    }
    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        &probe_record::RECORD
    }
    async fn handle(
        &self,
        id: &AgentId,
        call: Self::Call,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, Box<dyn std::error::Error + Send + Sync>> {
        {
            let key = Arc::as_ptr(kernel) as usize;
            let mut all = HANDLED.lock().unwrap();
            match all.iter_mut().find(|(k, _)| *k == key) {
                Some((_, n)) => *n += 1,
                None => all.push((key, 1)),
            }
        }
        match call {
            probe_verbs::Call::Reflect(_) => Ok(Some(json!({"id": id.0, "sentence": "A probe."}))),
            probe_verbs::Call::Greet(g) => Ok(Some(json!({
                "said": g.name, "times": g.times,
                "loud": matches!(g.style, Some(Style::Loud)),
            }))),
            probe_verbs::Call::Tally(_) => Ok(Some(json!({"count": 0}))),
        }
    }
}

/// Free-form record: any field is accepted.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct OpenRecord {}
mod open_record {
    use super::*;
    fantastic_kernel::record!(OpenRecord, open);
}

struct Open;

fantastic_kernel::stand_in_verbs!(open_verbs { AnythingV "anything" {  } });
#[async_trait]
impl Handler for Open {
    type Call = open_verbs::Call;
    fn man(&self) -> &'static str {
        "A test stand-in."
    }
    fn name(&self) -> &str {
        "open"
    }
    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        open_verbs::VERBS
    }
    fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
        open_verbs::dynamic_verbs(agent)
    }
    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        &open_record::RECORD
    }
    async fn handle(
        &self,
        _id: &AgentId,
        _call: Self::Call,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Some(json!({"ok": true})))
    }
}

fn mk() -> (TempDir, Arc<Kernel>) {
    let tmp = TempDir::new().unwrap();
    let mut kernel = Kernel::new();
    let mut reg = BundleRegistry::new();
    reg.register("probe.tools", Probe);
    reg.register("open.tools", Open);
    reg.register("tools_probe.tools", Tools);
    kernel.bundles = reg;
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(root);
    (tmp, kernel)
}

async fn call(kernel: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    kernel
        .send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
        .result
}

async fn with_probe() -> (TempDir, Arc<Kernel>) {
    let (t, k) = mk();
    let made = call(
        &k,
        "kernel",
        json!({"type": "create_agent", "parent_id": "kernel_state", "id": "p",
               "handler_module": "probe.tools", "word": "hello"}),
    )
    .await;
    assert_eq!(made["id"], "p", "{made}");
    (t, k)
}

#[tokio::test]
async fn a_call_that_matches_its_declaration_reaches_the_handler_typed() {
    let (_t, k) = with_probe().await;
    let r = call(&k, "p", json!({"type": "greet", "name": "Ann"})).await;
    assert_eq!(r, json!({"said": "Ann", "times": 1, "loud": false}));
    // an enum by its name
    let r = call(
        &k,
        "p",
        json!({"type": "greet", "name": "Ann", "times": 3, "style": "loud"}),
    )
    .await;
    assert_eq!(r, json!({"said": "Ann", "times": 3, "loud": true}));
}

#[tokio::test]
async fn an_unknown_verb_is_refused_with_the_list() {
    let (_t, k) = with_probe().await;
    let r = call(&k, "p", json!({"type": "nope"})).await;
    assert_eq!(r["reason"], "unhandled", "{r}");
    assert_eq!(r["verb"], "nope", "{r}");
    assert_eq!(
        r["error"], "unknown verb 'nope'; p answers: greet, tally, reflect",
        "{r}"
    );
}

/// The refusal of an unknown verb echoes the RAW `type` — python `verb=t`: `null` when
/// it is absent, the number for `5`, the empty string for `""`. `send` refuses those
/// payloads before any handler runs (`send: payload.type must be a non-empty string`),
/// so the kernel's own dispatch is driven directly, as for a bundle that forwards a
/// payload.
#[tokio::test]
async fn an_unknown_type_echoes_the_raw_value_as_verb() {
    let (_t, k) = with_probe().await;
    let agent = k
        .agents
        .get(&AgentId::from("p"))
        .map(|e| Arc::clone(&e))
        .unwrap();
    for (payload, shown, raw) in [
        (json!({}), "None", Value::Null),
        (json!({"type": null}), "None", Value::Null),
        (json!({"type": 5}), "5", json!(5)),
        (json!({"type": ""}), "''", json!("")),
    ] {
        let r = k.bookkeeping(&agent, payload.clone(), None).await.result;
        assert_eq!(
            r["error"],
            format!("unknown verb {shown}; p answers: greet, tally, reflect"),
            "{payload}: {r}"
        );
        assert_eq!(r["reason"], "unhandled", "{payload}: {r}");
        assert_eq!(r["verb"], raw, "{payload}: {r}");
    }
}

#[tokio::test]
async fn an_unknown_argument_is_refused_by_name_with_the_real_list() {
    let (_t, k) = with_probe().await;
    let r = call(
        &k,
        "p",
        json!({"type": "greet", "name": "Ann", "volume": 11}),
    )
    .await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(r["arg"], "volume", "{r}");
    assert_eq!(r["error"], "greet: unknown argument 'volume'", "{r}");
    assert_eq!(
        r["hint"],
        "greet takes — args: name:string (req), times:int (default 1), style:'calm'|'loud'?",
        "{r}"
    );
}

#[tokio::test]
async fn a_missing_argument_is_refused_by_name() {
    let (_t, k) = with_probe().await;
    let r = call(&k, "p", json!({"type": "greet"})).await;
    assert_eq!(r["reason"], "missing_arg", "{r}");
    assert_eq!(r["arg"], "name", "{r}");
    assert_eq!(r["error"], "greet: missing argument 'name'", "{r}");
    assert!(
        r["hint"].as_str().unwrap().contains("name: Who to greet."),
        "{r}"
    );
}

#[tokio::test]
async fn a_wrongly_typed_argument_is_refused_by_name() {
    let (_t, k) = with_probe().await;
    for (payload, arg, want) in [
        (
            json!({"type": "greet", "name": 7}),
            "name",
            "greet: argument 'name' must be string, got int",
        ),
        (
            json!({"type": "greet", "name": "Ann", "times": "many"}),
            "times",
            "greet: argument 'times' must be int, got str",
        ),
        // NO coercion: text that reads as a number is still text
        (
            json!({"type": "greet", "name": "Ann", "times": "3"}),
            "times",
            "greet: argument 'times' must be int, got str",
        ),
        (
            json!({"type": "greet", "name": "Ann", "times": true}),
            "times",
            "greet: argument 'times' must be int, got bool",
        ),
        (
            json!({"type": "greet", "name": "Ann", "times": 2.5}),
            "times",
            "greet: argument 'times' must be int, got float",
        ),
        (
            json!({"type": "greet", "name": "Ann", "style": "whisper"}),
            "style",
            "greet: argument 'style' must be 'calm'|'loud', got str",
        ),
    ] {
        let r = call(&k, "p", payload.clone()).await;
        assert_eq!(r["reason"], "bad_arg", "{payload}: {r}");
        assert_eq!(r["arg"], arg, "{payload}: {r}");
        assert_eq!(r["error"], want, "{payload}: {r}");
    }
}

#[tokio::test]
async fn a_verb_with_no_arguments_refuses_any() {
    let (_t, k) = with_probe().await;
    assert_eq!(
        call(&k, "p", json!({"type": "tally"})).await,
        json!({"count": 0})
    );
    let r = call(&k, "p", json!({"type": "tally", "since": "monday"})).await;
    assert_eq!(r["arg"], "since", "{r}");
    assert_eq!(r["hint"], "tally takes — No args", "{r}");
}

#[tokio::test]
async fn the_kernel_checks_its_own_verbs_the_same_way() {
    let (_t, k) = with_probe().await;
    // `of` ignored was the failure that started this: now refused by name
    let r = call(&k, "kernel", json!({"type": "reflect", "of": "brain"})).await;
    assert_eq!(r["arg"], "of", "{r}");
    assert_eq!(r["error"], "reflect: unknown argument 'of'", "{r}");
    let r = call(
        &k,
        "kernel",
        json!({"type": "update_agent", "id": "p",
        "edits": "x"}),
    )
    .await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(r["arg"], "edits", "{r}");
    let r = call(
        &k,
        "kernel",
        json!({"type": "delete_agent", "id": "p", "now": true}),
    )
    .await;
    assert_eq!(r["arg"], "now", "{r}");
    // a verb the kernel does not have, asked of the root, lists what it answers
    let r = call(&k, "kernel", json!({"type": "make_tea"})).await;
    assert_eq!(r["reason"], "unhandled", "{r}");
}

#[tokio::test]
async fn a_record_field_unknown_to_the_bundle_is_refused_at_create_and_update() {
    let (_t, k) = with_probe().await;
    let r = call(
        &k,
        "kernel",
        json!({"type": "create_agent", "parent_id": "kernel_state", "id": "q",
               "handler_module": "probe.tools", "instructions": "be brief"}),
    )
    .await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(r["arg"], "instructions", "{r}");
    assert_eq!(
        r["error"], "create_agent: probe has no record field 'instructions'",
        "{r}"
    );
    assert!(
        r["hint"]
            .as_str()
            .unwrap()
            .starts_with("a probe record takes — "),
        "{r}"
    );
    assert!(
        r["hint"]
            .as_str()
            .unwrap()
            .contains("word:string?, limit:int?"),
        "{r}"
    );
    assert!(k.agents.get(&AgentId::from("q")).is_none());
    let r = call(
        &k,
        "kernel",
        json!({"type": "update_agent", "id": "p", "edits": {"word": "hi"}}),
    )
    .await;
    assert_eq!(
        r["error"], "update_agent: probe has no record field 'edits'",
        "{r}"
    );
    assert_eq!(
        k.agents
            .get(&AgentId::from("p"))
            .unwrap()
            .meta
            .read()
            .unwrap()
            .get("edits"),
        None
    );
    // a wrong type for a known field is refused by name too
    let r = call(
        &k,
        "kernel",
        json!({"type": "update_agent", "id": "p", "limit": "many"}),
    )
    .await;
    assert_eq!(r["arg"], "limit", "{r}");
    // the fields every record has, and the bundle's own, are accepted
    let r = call(
        &k,
        "kernel",
        json!({"type": "update_agent", "id": "p", "limit": 4, "description": "d", "word": "yo"}),
    )
    .await;
    assert_eq!(r["updated"], true, "{r}");
}

#[tokio::test]
async fn an_open_record_takes_any_field() {
    let (_t, k) = mk();
    for hm in ["open.tools"] {
        let r = call(
            &k,
            "kernel",
            json!({"type": "create_agent", "parent_id": "kernel_state", "id": hm.replace('.', "_"),
                   "handler_module": hm, "anything": {"at": "all"}}),
        )
        .await;
        assert_eq!(r["anything"], json!({"at": "all"}), "{hm}: {r}");
    }
}

#[tokio::test]
async fn a_handler_that_is_not_installed_is_refused_at_create() {
    let (_t, k) = mk();
    let r = call(
        &k,
        "kernel",
        json!({"type": "create_agent", "parent_id": "kernel_state", "id": "ghost",
               "handler_module": "ghost.tools"}),
    )
    .await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(r["arg"], "handler_module", "{r}");
    assert_eq!(
        r["error"], "create_agent: handler_module 'ghost.tools' is not installed in this kernel",
        "{r}"
    );
    assert!(k.agents.get(&AgentId::from("ghost")).is_none());
}

#[tokio::test]
async fn boot_and_shutdown_are_not_answered_for_a_bundle_that_declares_neither() {
    let (_t, k) = with_probe().await;
    for verb in ["boot", "shutdown"] {
        let r = call(&k, "p", json!({"type": verb})).await;
        assert_eq!(r["reason"], "unhandled", "{verb}: {r}");
        assert_eq!(
            r["error"],
            format!("unknown verb '{verb}'; p answers: greet, tally, reflect"),
            "{r}"
        );
    }
}

async fn with_tools() -> (TempDir, Arc<Kernel>) {
    let (t, k) = mk();
    let made = call(
        &k,
        "kernel",
        json!({"type": "create_agent", "parent_id": "kernel_state", "id": "t",
               "handler_module": "tools_probe.tools",
               "tools": [
                   {"name": "search", "description": "Find pages. Ranked.",
                    "schema": {"type": "object",
                               "properties": {
                                   "q": {"type": "string", "description": "What to find."},
                                   "limit": {"type": "integer", "description": "How many."}},
                               "required": ["q"]}},
                   {"name": "ping", "description": "Say pong.", "schema": {"type": "object"}}]}),
    )
    .await;
    assert_eq!(made["id"], "t", "{made}");
    (t, k)
}

#[tokio::test]
async fn a_dynamic_verb_is_checked_by_the_same_engine() {
    let (_t, k) = with_tools().await;
    let ok = call(&k, "t", json!({"type": "search", "q": "rust", "limit": 3})).await;
    assert_eq!(
        ok,
        json!({"ran": "search", "args": {"q": "rust", "limit": 3}})
    );
    let r = call(&k, "t", json!({"type": "search"})).await;
    assert_eq!(r["reason"], "missing_arg", "{r}");
    assert_eq!(r["error"], "search: missing argument 'q'", "{r}");
    assert!(
        r["hint"].as_str().unwrap().contains("q: What to find."),
        "{r}"
    );
    // no coercion for a dynamic verb either
    let r = call(&k, "t", json!({"type": "search", "q": "x", "limit": "3"})).await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(
        r["error"], "search: argument 'limit' must be int, got str",
        "{r}"
    );
    let r = call(&k, "t", json!({"type": "ping"})).await;
    assert_eq!(r, json!({"ran": "ping", "args": {}}));
    // an unknown verb lists the builtin and the dynamic ones
    let r = call(&k, "t", json!({"type": "nope"})).await;
    assert_eq!(
        r["error"], "unknown verb 'nope'; t answers: tools, reflect, search, ping",
        "{r}"
    );
    // the builtin verb of a bundle with dynamic verbs
    assert_eq!(
        call(&k, "t", json!({"type": "tools"})).await,
        json!({"tools": 2})
    );
}

#[tokio::test]
async fn a_nested_object_is_checked_by_its_own_declared_fields() {
    let (_t, k) = mk();
    let made = call(
        &k,
        "kernel",
        json!({"type": "create_agent", "parent_id": "kernel_state", "id": "n",
               "handler_module": "tools_probe.tools",
               "tools": [{"name": "put", "description": "Store one.",
                          "schema": {"type": "object",
                                     "properties": {"item": {
                                         "type": "object",
                                         "required": ["id"],
                                         "properties": {"id": {"type": "string"},
                                                        "qty": {"type": "integer"}}}},
                                     "required": ["item"]}}]}),
    )
    .await;
    assert_eq!(made["id"], "n", "{made}");
    let ok = call(
        &k,
        "n",
        json!({"type": "put", "item": {"id": "a", "qty": 2, "x": 1}}),
    )
    .await;
    assert_eq!(ok["ran"], "put", "{ok}");
    let r = call(&k, "n", json!({"type": "put", "item": {"qty": 2}})).await;
    assert_eq!(r["reason"], "missing_arg", "{r}");
    assert_eq!(r["arg"], "item.id", "{r}");
    assert_eq!(r["error"], "put: missing argument 'item.id'", "{r}");
    let r = call(
        &k,
        "n",
        json!({"type": "put", "item": {"id": "a", "qty": "2"}}),
    )
    .await;
    assert_eq!(
        r["error"], "put: argument 'item.qty' must be int, got str",
        "{r}"
    );
    let r = call(&k, "n", json!({"type": "put", "item": "a"})).await;
    assert_eq!(
        r["error"], "put: argument 'item' must be object, got str",
        "{r}"
    );
}

#[tokio::test]
async fn reflect_and_docs_render_both_kinds_of_verb_from_the_agent() {
    let (_t, k) = with_tools().await;
    let r = call(&k, "t", json!({"type": "reflect"})).await;
    let names: Vec<&str> = r["verbs"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        names,
        ["tools", "reflect", "search", "ping"]
    );
    assert_eq!(
        r["verbs"]["search"],
        "args: q:string (req), limit:int? — Find pages"
    );
    assert_eq!(r["verbs"]["ping"], "No args — Say pong");
    let one = call(&k, "kernel", json!({"type": "docs", "of": "t"})).await;
    assert_eq!(one["verbs"]["search"], r["verbs"]["search"], "{one}");
    assert_eq!(one["verbs"]["ping"], "No args — Say pong", "{one}");
    let det = call(
        &k,
        "kernel",
        json!({"type": "docs", "of": "t", "tier": "detailed", "verb": "search"}),
    )
    .await;
    assert_eq!(
        det["verbs"]["search"],
        "Find pages. Ranked.\n\nargs:\n  q:string (req) — What to find.\n  limit:int? — How many.",
        "{det}"
    );
    // a bundle addressed by name has no agent, so no dynamic verbs
    let bundle = call(
        &k,
        "kernel",
        json!({"type": "docs", "of": "tools_probe.tools"}),
    )
    .await;
    assert!(bundle["verbs"].get("search").is_none(), "{bundle}");
    assert!(bundle["verbs"].get("tools").is_some(), "{bundle}");
}

#[tokio::test]
async fn reflect_renders_the_verbs_from_the_declarations() {
    let (_t, k) = with_probe().await;
    let r = call(&k, "p", json!({"type": "reflect"})).await;
    let verbs = r["verbs"].as_object().expect("a verbs block");
    let names: Vec<&str> = verbs.keys().map(String::as_str).collect();
    assert_eq!(
        names,
        ["greet", "tally", "reflect"],
        "{r}"
    );
    assert_eq!(
        verbs["greet"],
        "args: name:string (req), times:int (default 1), style:'calm'|'loud'? — Say hello to someone, a number of times"
    );
    assert_eq!(verbs["tally"], "No args — Count what was said");
}

#[tokio::test]
async fn docs_render_both_tiers_from_the_declarations() {
    let (_t, k) = with_probe().await;
    let one = call(&k, "kernel", json!({"type": "docs", "of": "p"})).await;
    assert_eq!(one["bundle"], "probe.tools", "{one}");
    assert_eq!(
        one["verbs"]["greet"],
        "args: name:string (req), times:int (default 1), style:'calm'|'loud'? — Say hello to someone, a number of times",
        "{one}"
    );
    assert!(one["verbs"].get("reflect").is_some(), "{one}");
    let det = call(
        &k,
        "kernel",
        json!({"type": "docs", "of": "probe.tools", "verb": "greet", "tier": "detailed"}),
    )
    .await;
    let text = det["verbs"]["greet"].as_str().expect("one verb");
    assert!(
        text.starts_with("Say hello to someone, a number of times."),
        "{text}"
    );
    assert!(text.contains("name:string (req) — Who to greet."), "{text}");
    assert!(
        text.contains("times:int (default 1) — How many times; one by default."),
        "{text}"
    );
    assert!(
        text.contains("style:'calm'|'loud'? — `loud` shouts."),
        "{text}"
    );
    assert_eq!(det["verbs"].as_object().unwrap().len(), 1, "{det}");
    let r = call(
        &k,
        "kernel",
        json!({"type": "docs", "of": "p", "verb": "nope"}),
    )
    .await;
    assert_eq!(r["error"], "docs: p has no verb 'nope'", "{r}");
    assert_eq!(
        r["hint"], "its verbs: greet, tally, reflect",
        "{r}"
    );
}

#[tokio::test]
async fn the_bundle_catalog_lists_the_declared_record() {
    let (_t, k) = mk();
    let r = call(
        &k,
        "kernel",
        json!({"type": "reflect", "tree": "none", "bundles": "all"}),
    )
    .await;
    let probe = r["bundles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["name"] == "probe")
        .expect("probe in the catalog");
    let fields = probe["schema"]["fields"].as_array().expect("fields");
    assert_eq!(fields[0]["name"], "word", "{probe}");
    assert_eq!(fields[0]["hint"], "The greeting's first word.", "{probe}");
    assert_eq!(fields[1]["name"], "limit", "{probe}");
}

// ─── the typed arguments of the kernel's own verbs: the declared type decides ───

#[tokio::test]
async fn a_page_size_and_cursor_are_the_declared_types() {
    let (_t, k) = with_probe().await;
    for verb in ["list_agents", "ps", "ledger"] {
        let r = call(&k, "kernel", json!({"type": verb, "limit": 0})).await;
        assert_eq!(r["reason"], "bad_arg", "{verb}: {r}");
        assert_eq!(r["arg"], "limit", "{verb}: {r}");
        assert_eq!(
            r["error"],
            format!("{verb}: argument 'limit' must be int >= 1, got int"),
            "{r}"
        );
        // a number is no cursor, text is no page size
        let r = call(&k, "kernel", json!({"type": verb, "after": 2})).await;
        assert_eq!(r["arg"], "after", "{verb}: {r}");
        let r = call(&k, "kernel", json!({"type": verb, "limit": "2"})).await;
        assert_eq!(r["arg"], "limit", "{verb}: {r}");
    }
    let page = call(&k, "kernel", json!({"type": "list_agents", "limit": 1})).await;
    assert_eq!(page["agents"].as_array().unwrap().len(), 1, "{page}");
    assert!(page["next"].is_string(), "{page}");
}

#[tokio::test]
async fn text_is_a_name_never_a_task_id() {
    let (_t, k) = with_probe().await;
    // an id is a number; the text of one names no execution, and says so honestly
    let r = call(&k, "kernel", json!({"type": "cancel", "task_id": "123456"})).await;
    assert_eq!(r, json!({"cancelled": false, "killed": []}), "{r}");
    let r = call(&k, "kernel", json!({"type": "cancel", "task_id": 123_456})).await;
    assert_eq!(r, json!({"cancelled": false, "killed": []}), "{r}");
    // neither a float nor a bool is either
    for bad in [json!(3.5), json!(true), json!([1])] {
        let r = call(
            &k,
            "kernel",
            json!({"type": "cancel", "task_id": bad.clone()}),
        )
        .await;
        assert_eq!(r["reason"], "bad_arg", "{bad}: {r}");
        assert_eq!(r["arg"], "task_id", "{bad}: {r}");
    }
    let r = call(&k, "kernel", json!({"type": "cancel"})).await;
    assert_eq!(r["reason"], "missing_arg", "{r}");
    // the wait of `await_cancel` is an integer id
    let r = call(
        &k,
        "kernel",
        json!({"type": "await_cancel", "wait_id": "5"}),
    )
    .await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(r["arg"], "wait_id", "{r}");
}

#[tokio::test]
async fn await_set_takes_its_declared_arguments() {
    let (_t, k) = with_probe().await;
    let r = call(
        &k,
        "kernel",
        json!({"type": "await_set", "ids": ["p"], "timeout_s": 0}),
    )
    .await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(r["arg"], "timeout_s", "{r}");
    let r = call(
        &k,
        "kernel",
        json!({"type": "await_set", "ids": ["p"], "timeout_s": "5"}),
    )
    .await;
    assert_eq!(r["arg"], "timeout_s", "{r}");
    let r = call(&k, "kernel", json!({"type": "await_set", "ids": "p"})).await;
    assert_eq!(r["arg"], "ids", "{r}");
    let r = call(
        &k,
        "kernel",
        json!({"type": "await_set", "ids": ["p"], "label": 4}),
    )
    .await;
    assert_eq!(r["arg"], "label", "{r}");
}

#[tokio::test]
async fn docs_of_a_bundle_carries_its_one_man_text_and_no_other_prose() {
    let (_t, k) = with_probe().await;
    for tier in ["oneliners", "detailed"] {
        let r = call(
            &k,
            "kernel",
            json!({"type": "docs", "of": "p", "tier": tier}),
        )
        .await;
        assert_eq!(r["man"], "A test stand-in.", "{tier}: {r}");
        assert_eq!(r["tier"], tier, "{r}");
    }
    // one verb's entry carries the verb alone
    let r = call(
        &k,
        "kernel",
        json!({"type": "docs", "of": "p", "verb": "tally"}),
    )
    .await;
    assert!(r.get("man").is_none(), "{r}");
    // the tier is the declared enum
    let r = call(
        &k,
        "kernel",
        json!({"type": "docs", "of": "p", "tier": "full"}),
    )
    .await;
    assert_eq!(r["arg"], "tier", "{r}");
}

/// The handler runs on a CHECKED call only: whatever the entry refuses never reaches it.
#[tokio::test]
async fn a_refused_call_never_reaches_the_handler() {
    let (_t, k) = with_probe().await;
    let ran = call(&k, "p", json!({"type": "tally"})).await;
    assert_eq!(ran, json!({"count": 0}), "{ran}");
    let handled = handled_by(&k);
    for bad in [
        json!({"type": "greet"}),
        json!({"type": "greet", "name": 5}),
        json!({"type": "greet", "name": "a", "extra": 1}),
        json!({"type": "nope"}),
    ] {
        let r = call(&k, "p", bad.clone()).await;
        assert!(r["error"].is_string(), "{bad}: {r}");
        assert!(r["reason"].is_string(), "{bad}: {r}");
    }
    assert_eq!(handled_by(&k), handled, "a refused call ran the handler");
    // a checked one does
    let ok = call(&k, "p", json!({"type": "greet", "name": "Ann"})).await;
    assert_eq!(ok["said"], "Ann", "{ok}");
    assert_eq!(handled_by(&k), handled + 1);
}

#[tokio::test]
async fn a_reflect_flag_is_the_declared_type() {
    let (_t, k) = with_probe().await;
    for (arg, bad) in [
        ("detail", json!("everything")),
        ("tree", json!(false)),
        ("bundles", json!("some")),
        ("limit", json!(0)),
        ("after", json!(3)),
        ("show_subtrees", json!("p")),
    ] {
        let r = call(&k, "kernel", json!({"type": "reflect", arg: bad.clone()})).await;
        assert_eq!(r["reason"], "bad_arg", "{arg}={bad}: {r}");
        assert_eq!(r["arg"], arg, "{arg}={bad}: {r}");
    }
}
