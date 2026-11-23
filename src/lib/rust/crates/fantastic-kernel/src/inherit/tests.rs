//! Unit tests for [`crate::inherit`].

use super::*;
use crate::agent::AgentId;
use serde_json::json;

/// A compiled base with no verbs of its own.
struct Plain;

#[async_trait::async_trait]
impl crate::Handler for Plain {
    type Call = crate::test_support::NoCall;
    fn man(&self) -> &'static str {
        "A test stand-in."
    }
    fn name(&self) -> &str {
        "plain"
    }
    fn verbs(&self) -> &'static [crate::Verb] {
        crate::test_support::no_verbs()
    }
    fn dynamic_verbs(&self, agent: &crate::Agent) -> Vec<crate::DynVerb> {
        crate::test_support::no_dynamic(agent)
    }
    fn record(&self) -> &'static crate::RecordDecl {
        crate::fixture_record!()
    }
    async fn handle(
        &self,
        _id: &AgentId,
        _call: Self::Call,
        _k: &Arc<Kernel>,
    ) -> Result<crate::Reply, crate::bundle::BundleError> {
        Ok(None)
    }
}

fn chatty(model: &str, verbs: &str) -> String {
    format!(
        "base: plain.tools\nsentence: A chat stand-in.\nrecord:\n  model: {model}\n  can:\n    memory:\n      call: [{verbs}]\n"
    )
}

/// A reload that changes the bundle's `can` default AND another default: the
/// instance takes both, the `can` through `edit_can`.
#[test]
fn a_reload_changing_can_and_another_default_keeps_both() {
    let mut kernel = Kernel::new();
    kernel.bundles.register("plain.tools", Plain);
    kernel
        .bundles
        .register_meta_yaml("chatty", &chatty("m1", "read"))
        .expect("meta bundle");
    let kernel = Arc::new(kernel);
    let mut meta = Map::new();
    apply(&kernel.bundles, "chatty", &mut meta);
    assert_eq!(meta["can"], json!({"memory": {"call": ["read"]}}));
    kernel.register(Agent::new(
        AgentId::from("c1"),
        Some("chatty".to_string()),
        None,
        meta,
        std::path::PathBuf::new(),
        true,
    ));
    let updated = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let u = std::sync::Arc::clone(&updated);
    let _tap = kernel.add_state_subscriber(std::sync::Arc::new(move |ev, _body| {
        if ev["kind"] == "updated" {
            u.lock().unwrap().push(ev["changed"].clone());
        }
    }));
    let before = inheriting(&kernel, "chatty");
    kernel
        .bundles
        .register_meta_yaml("chatty", &chatty("m2", "read, append"))
        .expect("meta bundle reloaded");
    assert_eq!(reapply(&kernel, &before), vec!["c1".to_string()]);
    let m = kernel.resolve_target("c1").expect("c1").record().meta;
    assert_eq!(m["can"], json!({"memory": {"call": ["read", "append"]}}));
    assert_eq!(m["model"], "m2");
    let changed = updated.lock().unwrap().clone();
    assert!(changed.contains(&json!(["can"])), "{changed:?}");
    assert!(changed.contains(&json!(["model"])), "{changed:?}");
}

/// A LOADED record takes the bundle's defaults but its `can` (rule 13, Q10): a record
/// with no lines stays with none at the next load.
#[test]
fn a_loaded_record_takes_every_default_but_can() {
    let mut kernel = Kernel::new();
    kernel.bundles.register("plain.tools", Plain);
    kernel
        .bundles
        .register_meta_yaml("chatty", &chatty("m1", "read"))
        .expect("meta bundle");
    let mut meta = Map::new();
    apply_loaded(&kernel.bundles, "chatty", &mut meta);
    assert_eq!(meta["model"], json!("m1"));
    assert!(meta.get("can").is_none(), "{meta:?}");
}
