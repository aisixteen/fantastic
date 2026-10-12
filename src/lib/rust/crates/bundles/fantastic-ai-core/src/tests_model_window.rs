//! THE BACKEND'S OWN WINDOW (`BackendConfig::model_window`): what
//! `context_window` resolves to when the record names neither `context_window`
//! nor `num_ctx`, and the schema's `context_window` default. A server backend
//! passes `DEFAULT_CONTEXT_WINDOW`; a backend that can read its model's real
//! window (foundation_models_backend) passes that. Swift `AIBackendConfig.modelWindow`.

use crate::constants::DEFAULT_CONTEXT_WINDOW;
use crate::context::{budget, inline_cap, resolve_context_window};
use crate::provider::{Provider, ProviderError, ProviderStream};
use async_trait::async_trait;
use serde_json::{json, Map, Value};

fn meta(v: Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap()
}

#[test]
fn an_unnamed_window_is_the_backends_own() {
    let none = Map::new();
    assert_eq!(resolve_context_window(&none, 4096), 4096);
    assert_eq!(
        resolve_context_window(&none, DEFAULT_CONTEXT_WINDOW),
        131_072
    );
    // the budget and the inline cap follow it
    assert_eq!(budget(&none, 4096), 4096 - 1024);
    assert_eq!(inline_cap(&none, 4096), (4096 - 1024) / 8);
    // the record still wins, by the same static precedence
    assert_eq!(
        resolve_context_window(&meta(json!({"context_window": 3000})), 4096),
        3000
    );
    assert_eq!(
        resolve_context_window(&meta(json!({"num_ctx": 2000})), 4096),
        2000
    );
}

#[test]
fn the_schema_defaults_context_window_to_the_backends_window() {
    let at = |s: &Value| {
        s["fields"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == json!("context_window"))
            .unwrap()["default"]
            .clone()
    };
    assert_eq!(
        at(&crate::schema::schema_with_window(vec![], 4096)),
        json!(4096)
    );
    assert_eq!(
        at(&crate::schema::schema(vec![])),
        json!(DEFAULT_CONTEXT_WINDOW)
    );
    // otherwise the same fields, byte for byte
    let a = crate::schema::schema(vec![]).to_string();
    let b = crate::schema::schema_with_window(vec![], DEFAULT_CONTEXT_WINDOW).to_string();
    assert_eq!(a, b);
}

struct Quiet;

#[async_trait]
impl Provider for Quiet {
    async fn chat(
        &self,
        _messages: &[Value],
        _max_tokens: Option<i64>,
    ) -> Result<ProviderStream, ProviderError> {
        Err(ProviderError::raised("unused"))
    }

    fn model(&self) -> String {
        String::new()
    }
}

#[test]
fn a_provider_is_ready_unless_it_says_otherwise() {
    assert!(Quiet.ready().is_ok());
}
