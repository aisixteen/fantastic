//! THE MODEL TYPE OF A PASS (#766): which adapter formats a model's calls, chosen by
//! substring of the model id from a bundled, ordered rule list, and the model id itself,
//! from the first source that names one.

use std::future::Future;
use std::sync::{Arc, OnceLock};

use fantastic_kernel::{AgentId, Item, Kernel};
use serde_json::{json, Map, Value};

use crate::state::BackendState;

/// The bundled rule list: an ordered array of `{"contains": "<substring>", "type":
/// "<adapter type>"}`. It changes only with a build.
pub(crate) const RULES_JSON: &str = include_str!("res/model_types.json");

/// One rule: an id that contains `contains` (lower case) is of type `kind`.
#[derive(Debug)]
pub(crate) struct Rule {
    /// The substring, lower case.
    pub(crate) contains: String,
    /// The adapter type, a registry name.
    pub(crate) kind: &'static str,
}

/// A loaded rule list, in file order, with the bytes it was read from (the cache epoch
/// hashes them).
#[derive(Debug)]
pub(crate) struct Rules {
    /// The rules, first match wins.
    pub(crate) rules: Vec<Rule>,
    /// The file's bytes as loaded.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) source: String,
}

/// Read a rule list. Refused, naming the rule: not an array of objects; a rule with a key
/// other than `contains` and `type`, or without either; a `contains` that is empty or not
/// lower case (it could never match a lower-cased id); a `type` the registry does not hold.
pub(crate) fn load(text: &str) -> Result<Rules, String> {
    let parsed: Value =
        serde_json::from_str(text).map_err(|e| format!("model types: not JSON: {e}"))?;
    let items = parsed
        .as_array()
        .ok_or_else(|| "model types: not an array of rules".to_string())?;
    let mut rules = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let obj = item
            .as_object()
            .ok_or_else(|| format!("model types: rule {i} is not an object"))?;
        if let Some(k) = obj.keys().find(|k| *k != "contains" && *k != "type") {
            return Err(format!("model types: rule {i} has an unknown key `{k}`"));
        }
        let contains = obj
            .get("contains")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("model types: rule {i} has no string `contains`"))?;
        if contains.is_empty() || contains != contains.to_lowercase() {
            return Err(format!(
                "model types: rule {i}: `contains` {contains:?} must be a non-empty lower-case substring"
            ));
        }
        let kind = obj
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("model types: rule {i} has no string `type`"))?;
        let entry = super::entry(kind)
            .ok_or_else(|| format!("model types: rule {i} names an unknown type `{kind}`"))?;
        rules.push(Rule {
            contains: contains.to_string(),
            kind: entry.name,
        });
    }
    Ok(Rules {
        rules,
        source: text.to_string(),
    })
}

/// The bundled rule list, parsed once.
pub(crate) fn shipped() -> &'static Rules {
    static RULES: OnceLock<Rules> = OnceLock::new();
    RULES.get_or_init(|| load(RULES_JSON).expect("the bundled model_types.json loads"))
}

/// The type of `model_id`: the type of the first rule whose `contains` is a substring of
/// the lower-cased id; no match, [`super::DEFAULT`].
pub(crate) fn type_of(rules: &Rules, model_id: &str) -> &'static str {
    let id = model_id.to_lowercase();
    rules
        .rules
        .iter()
        .find(|r| id.contains(r.contains.as_str()))
        .map_or(super::DEFAULT, |r| r.kind)
}

// ── the model id of a pass ───────────────────────────────────────────────────

/// The record's `(provider_id, model)` pair the model id was last resolved for, and the
/// provider's `models` default for it: `None` until answered, then the answer
/// (`Some(None)`: answered, no default). An ask that got no answer is not kept, so the
/// next pass asks again. Kept in [`BackendState::model_key`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ModelKey {
    /// `(provider_id, model)` as the record has them, empty when absent.
    pub(crate) pair: (String, String),
    /// The `models` answer's `default`, once answered for this pair.
    pub(crate) models_default: Option<Option<String>>,
}

/// Keep the model a provider named (a pass's reply, a `capabilities` answer) as
/// [`BackendState::model_seen`]. An empty name is none.
pub(crate) fn note_seen(state: &BackendState, model: &str) {
    if !model.is_empty() {
        *state.model_seen.lock().expect("model_seen poisoned") = Some(model.to_string());
    }
}

/// The first non-empty source, in the order of 1.3: the record's `model`, the model the
/// provider last named, the provider's `models` default, the newest assistant row's
/// `attributes.model`.
pub(crate) fn first_set(sources: [Option<&str>; 4]) -> Option<&str> {
    sources.into_iter().flatten().find(|s| !s.is_empty())
}

/// The model id of the chat's next pass ([`model_of_with`] over the kernel): the provider
/// named by the record's `provider_id` is asked `models`, and the newest stored assistant
/// row is read, each only when nothing earlier names a model.
pub(crate) async fn model_of(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    meta: &Map<String, Value>,
    state: &BackendState,
) -> Option<String> {
    model_of_with(
        meta,
        state,
        |provider| ask_models(kernel, provider),
        || newest_row_model(self_id, kernel),
    )
    .await
}

/// The model id of the chat's next pass, first that is set: the record's `model`; the
/// model the provider last named (`model_seen`); the provider's `models` default, asked
/// through `ask` once per `(provider_id, model)` pair, and only when the record names a
/// provider (`ask` gives `None` when no answer came: not delivered, not completed, or a
/// transport refusal; that is not kept, and the next pass asks again; an answer, a
/// refusal or one with no default included, is kept for the pair); the newest assistant row's model, read through `newest`. `None`: no source
/// names one, and the pass is of the [`super::DEFAULT`] type.
///
/// When the record's pair differs from the one last resolved, what the provider named
/// before was for another provider or model: `model_seen` is cleared.
pub(crate) async fn model_of_with<A, AF, N, NF>(
    meta: &Map<String, Value>,
    state: &BackendState,
    ask: A,
    newest: N,
) -> Option<String>
where
    A: FnOnce(String) -> AF,
    AF: Future<Output = Option<Option<String>>>,
    N: FnOnce() -> NF,
    NF: Future<Output = Option<String>>,
{
    let field = |k: &str| {
        meta.get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let record = field("model");
    let provider = field(crate::remote_provider::PROVIDER_FIELD);
    let pair = (
        provider.clone().unwrap_or_default(),
        record.clone().unwrap_or_default(),
    );
    {
        let mut key = state.model_key.lock().expect("model_key poisoned");
        if key.as_ref().map(|k| &k.pair) != Some(&pair) {
            if key.is_some() {
                *state.model_seen.lock().expect("model_seen poisoned") = None;
            }
            *key = Some(ModelKey {
                pair,
                models_default: None,
            });
        }
    }
    let seen = state
        .model_seen
        .lock()
        .expect("model_seen poisoned")
        .clone();
    if let Some(m) = first_set([record.as_deref(), seen.as_deref(), None, None]) {
        return Some(m.to_string());
    }
    let asked = state
        .model_key
        .lock()
        .expect("model_key poisoned")
        .as_ref()
        .and_then(|k| k.models_default.clone());
    let default = match asked {
        Some(d) => d,
        None => {
            match provider {
                Some(p) => match ask(p).await {
                    Some(d) => {
                        if let Some(k) =
                            state.model_key.lock().expect("model_key poisoned").as_mut()
                        {
                            k.models_default = Some(d.clone());
                        }
                        d
                    }
                    // no answer (the provider is not up yet, the call failed): kept as
                    // nothing, so a later pass asks again
                    None => None,
                },
                None => None,
            }
        }
    };
    if let Some(m) = first_set([None, None, default.as_deref(), None]) {
        return Some(m.to_string());
    }
    let row = newest().await;
    first_set([None, None, None, row.as_deref()]).map(str::to_string)
}

/// The `default` of a `models` answer: none when the answer is a refusal (it has an
/// `error`) or names no default.
pub(crate) fn default_in(answer: &Value) -> Option<String> {
    if answer.get("error").is_some() {
        return None;
    }
    answer
        .get("default")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Ask the provider agent `provider` for its `models`, as the chat: `None` when no answer
/// came (not delivered, not completed, or a `transport_error` refusal); else the answer's
/// `default`, none when it refuses another way or names none.
async fn ask_models(kernel: &Arc<Kernel>, provider: String) -> Option<Option<String>> {
    let out = kernel
        .send(vec![Item::new(
            AgentId::from(provider.as_str()),
            json!({"type": "models"}),
        )])
        .await
        .into_iter()
        .next()?;
    if out.status != fantastic_kernel::Status::Completed
        || out.result.get("reason").and_then(Value::as_str)
            == Some(fantastic_kernel::refusal::TRANSPORT_ERROR)
    {
        return None;
    }
    Some(default_in(&out.result))
}

/// The `attributes.model` of the newest assistant row the chat holds.
async fn newest_row_model(self_id: &AgentId, kernel: &Arc<Kernel>) -> Option<String> {
    crate::memory::load_history(self_id, kernel, false)
        .await
        .ok()?
        .iter()
        .rev()
        .find(|r| r.get("role").and_then(Value::as_str) == Some("assistant"))?
        .get("attributes")?
        .get("model")?
        .as_str()
        .map(str::to_string)
}
