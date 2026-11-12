//! INHERITANCE: the one place a bundle's `record` reaches its instances. Every source
//! is the same here — a compiled bundle gives no fields, a folder bundle its `record`,
//! a meta bundle its own and each one's on the way to its base
//! ([`crate::bundle::BundleRegistry::defaults`]). The fields go UNDER an instance's
//! own: at `create_agent`, when a record is loaded, and on `reload_bundle`. The loader
//! only reads files; it knows nothing of this.
//!
//! A record's KIND is its `handler_module`: the one field.

use std::sync::Arc;

use serde_json::{Map, Value};

use crate::agent::Agent;
use crate::bundle::BundleRegistry;
use crate::kernel::Kernel;

/// Lay the fields of the record's bundle (its `handler_module`) under `meta`'s own.
pub fn apply(bundles: &BundleRegistry, handler_module: &str, meta: &mut Map<String, Value>) {
    for (k, v) in bundles.defaults(handler_module) {
        meta.entry(k).or_insert(v);
    }
}

/// What a `create_agent` makes of its arguments once the bundle's fields are under
/// them: the handler module the agent runs and its record's fields.
#[must_use]
pub fn create_args(
    bundles: &BundleRegistry,
    args: crate::declared::kernel::CreateAgent,
) -> (String, Map<String, Value>) {
    let crate::declared::kernel::CreateAgent {
        handler_module,
        fields,
        ..
    } = args;
    let mut meta = fields;
    let hm = handler_module;
    apply(bundles, &hm, &mut meta);
    (hm, meta)
}

/// The instances that inherit from bundle `name` (its own and those of every bundle
/// that extends it), each with the fields it inherits NOW: taken before a reload, so
/// [`reapply`] can tell an inherited field from one the instance set itself.
#[must_use]
pub fn inheriting(kernel: &Kernel, name: &str) -> Vec<(Arc<Agent>, Map<String, Value>)> {
    kernel
        .agents
        .iter()
        .filter_map(|e| {
            let hm = e.value().handler_module.clone()?;
            let from = hm == name || kernel.bundles.meta_chain(&hm).iter().any(|m| m == name);
            from.then(|| (Arc::clone(e.value()), kernel.bundles.defaults(&hm)))
        })
        .collect()
}

/// After a reload: each instance's inherited fields take the new values (a field the
/// bundle dropped goes); a field the instance changed is its own and stays. Returns the
/// ids it changed, sorted.
pub fn reapply(kernel: &Kernel, before: &[(Arc<Agent>, Map<String, Value>)]) -> Vec<String> {
    let mut ids = Vec::new();
    for (a, old) in before {
        let hm = a.handler_module.clone().unwrap_or_default();
        let mine = a.meta.read().expect("meta").clone();
        let new = kernel.bundles.defaults(&hm);
        let mut patch = Map::new();
        let mut gone = Vec::new();
        for (k, v) in old {
            if mine.get(k) == Some(v) {
                match new.get(k) {
                    Some(n) if n != v => {
                        patch.insert(k.clone(), n.clone());
                    }
                    Some(_) => {}
                    None => gone.push(k.clone()),
                }
            }
        }
        for (k, v) in &new {
            if !old.contains_key(k) && !mine.contains_key(k) {
                patch.insert(k.clone(), v.clone());
            }
        }
        if patch.is_empty() && gone.is_empty() {
            continue;
        }
        {
            let mut m = a.meta.write().expect("meta");
            for k in &gone {
                m.remove(k);
            }
        }
        kernel.update(&a.id, patch);
        ids.push(a.id.0.clone());
    }
    ids.sort();
    ids
}
