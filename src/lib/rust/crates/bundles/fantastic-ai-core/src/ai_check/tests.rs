//! The AI check: an AI record, a plain record, an unknown id.

use super::*;
use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::Agent;
use serde_json::{json, Map, Value};
use std::sync::Arc;

struct Fake {
    name: &'static str,
    schema: Value,
}

#[async_trait]
impl Bundle for Fake {
    fn name(&self) -> &str {
        self.name
    }
    fn record_schema(&self) -> Option<Value> {
        Some(self.schema.clone())
    }
    async fn handle(
        &self,
        _id: &AgentId,
        _payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        Ok(None)
    }
}

fn kernel_with(records: &[(&str, &str, Value)]) -> Kernel {
    let mut kernel = Kernel::new();
    kernel.bundles.register(
        "fake_ai.tools",
        Fake {
            name: "fake_ai.tools",
            schema: json!({"fields": [{"name": "memory_store_id"}]}),
        },
    );
    kernel.bundles.register(
        "fake_plain.tools",
        Fake {
            name: "fake_plain.tools",
            schema: json!({"fields": [{"name": "model"}]}),
        },
    );
    for (id, handler, meta) in records {
        let meta: Map<String, Value> = meta.as_object().cloned().unwrap();
        kernel.register(Agent::new(
            AgentId::from(*id),
            Some((*handler).to_string()),
            None,
            meta,
            std::env::temp_dir(),
            true,
        ));
    }
    kernel
}

#[test]
fn an_ai_record_is_an_ai_and_a_plain_or_unknown_one_is_not() {
    let k = kernel_with(&[
        ("ai_1", "fake_ai.tools", json!({"memory_store_id": "m"})),
        ("no_store", "fake_ai.tools", json!({})),
        (
            "plain_1",
            "fake_plain.tools",
            json!({"memory_store_id": "m"}),
        ),
    ]);
    assert!(is_ai(&k, "ai_1"));
    assert!(!is_ai(&k, "no_store"), "an AI handler without a store");
    assert!(!is_ai(&k, "plain_1"), "a store on a non-AI handler");
    assert!(!is_ai(&k, "nobody"));
}

#[test]
fn a_handler_is_an_ai_handler_when_its_schema_names_a_store() {
    let k = kernel_with(&[]);
    assert!(is_ai_handler(&k, "fake_ai.tools"));
    assert!(!is_ai_handler(&k, "fake_plain.tools"));
    assert!(!is_ai_handler(&k, "no_such.tools"));
}
