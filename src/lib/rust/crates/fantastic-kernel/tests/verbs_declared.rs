//! Verbs declared once (issue #642): the kernel checks every call against the
//! bundle's declaration and renders `reflect` and `docs` from it. The probe bundle
//! below is written the way every bundle is: structs, `verbs!`, `record!`, and a
//! `handle` that parses the already-checked payload.

use async_trait::async_trait;
use fantastic_kernel::{Agent, AgentId, Bundle, BundleRegistry, Item, Kernel, Reply};
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

#[async_trait]
impl Bundle for Probe {
    fn name(&self) -> &str {
        "probe"
    }
    fn verbs(&self) -> Option<&'static [fantastic_kernel::Verb]> {
        Some(probe_verbs::VERBS)
    }
    fn record(&self) -> Option<&'static fantastic_kernel::RecordDecl> {
        Some(&probe_record::RECORD)
    }
    async fn handle(
        &self,
        id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, Box<dyn std::error::Error + Send + Sync>> {
        if payload["type"] == "reflect" {
            return Ok(Some(json!({"id": id.0, "sentence": "A probe."})));
        }
        // no validation here: the kernel checked the call
        match probe_verbs::Call::parse(payload)? {
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

#[async_trait]
impl Bundle for Open {
    fn name(&self) -> &str {
        "open"
    }
    fn record(&self) -> Option<&'static fantastic_kernel::RecordDecl> {
        Some(&open_record::RECORD)
    }
    async fn handle(
        &self,
        _id: &AgentId,
        _payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Some(json!({"ok": true})))
    }
}

/// A bundle not yet moved to declarations: its calls are not checked.
struct Legacy;

#[async_trait]
impl Bundle for Legacy {
    fn name(&self) -> &str {
        "legacy"
    }
    async fn handle(
        &self,
        _id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Some(json!({"saw": payload})))
    }
}

fn mk() -> (TempDir, Arc<Kernel>) {
    let tmp = TempDir::new().unwrap();
    let mut kernel = Kernel::new();
    let mut reg = BundleRegistry::new();
    reg.register("probe.tools", Probe);
    reg.register("open.tools", Open);
    reg.register("legacy.tools", Legacy);
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
    // a number written as text is typed first (scalar.rs), an enum by its name
    let r = call(
        &k,
        "p",
        json!({"type": "greet", "name": "Ann", "times": "3", "style": "loud"}),
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
        r["error"], "unknown verb 'nope'; p answers: greet, tally, reflect, boot, shutdown",
        "{r}"
    );
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
async fn an_open_record_takes_any_field_and_an_undeclared_one_is_not_checked() {
    let (_t, k) = mk();
    for hm in ["open.tools", "legacy.tools"] {
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
async fn a_bundle_not_yet_moved_is_not_checked() {
    let (_t, k) = mk();
    call(
        &k,
        "kernel",
        json!({"type": "create_agent", "parent_id": "kernel_state", "id": "old",
               "handler_module": "legacy.tools"}),
    )
    .await;
    let r = call(&k, "old", json!({"type": "whatever", "any": 1})).await;
    assert_eq!(r["saw"]["any"], 1, "{r}");
}

#[tokio::test]
async fn boot_and_shutdown_are_answered_for_a_bundle_that_declares_neither() {
    let (_t, k) = with_probe().await;
    for verb in ["boot", "shutdown"] {
        let r = call(&k, "p", json!({"type": verb})).await;
        assert!(r.is_null(), "{verb}: {r}");
    }
    let r = call(&k, "p", json!({"type": "shutdown", "reason": "reloaded"})).await;
    assert!(r.is_null(), "{r}");
}

#[tokio::test]
async fn reflect_renders_the_verbs_from_the_declarations() {
    let (_t, k) = with_probe().await;
    let r = call(&k, "p", json!({"type": "reflect"})).await;
    let verbs = r["verbs"].as_object().expect("a verbs block");
    let names: Vec<&str> = verbs.keys().map(String::as_str).collect();
    assert_eq!(
        names,
        ["greet", "tally", "reflect", "boot", "shutdown"],
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
        r["hint"], "its verbs: greet, tally, reflect, boot, shutdown",
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
