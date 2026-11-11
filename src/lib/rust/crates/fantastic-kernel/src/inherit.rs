//! INHERITANCE: the one place a bundle's `record` reaches its instances. Every source
//! is the same here — a compiled bundle gives no fields, a folder bundle its `record`,
//! a meta bundle its own and each one's on the way to its base
//! ([`crate::bundle::BundleRegistry::defaults`]). The fields go UNDER an instance's
//! own: at `create_agent`, when a record is loaded, and on `reload_bundle`. The loader
//! only reads files; it knows nothing of this.
//!
//! A record's KIND is its `handler_module`, or its `bundle` when it runs a meta bundle
//! on another base than the bundle's own (an AI role on whichever backend its spawner
//! uses): `handler_module: vllm_backend.tools, bundle: worker`.

use std::sync::Arc;

use serde_json::{Map, Value};

use crate::agent::Agent;
use crate::bundle::BundleRegistry;
use crate::kernel::Kernel;

/// The bundle a record inherits from: its `bundle`, else its `handler_module`.
#[must_use]
pub fn kind_of(handler_module: &str, meta: &Map<String, Value>) -> String {
    meta.get("bundle")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(handler_module)
        .to_string()
}

/// Lay the fields of the record's bundle ([`kind_of`]) under `meta`'s own.
pub fn apply(bundles: &BundleRegistry, handler_module: &str, meta: &mut Map<String, Value>) {
    for (k, v) in bundles.defaults(&kind_of(handler_module, meta)) {
        meta.entry(k).or_insert(v);
    }
}

/// What a `create_agent` makes of its arguments once the bundle's fields are under
/// them: the handler module the agent runs (`None`: none was named) and its record's
/// fields. `bundle: X` (beside the base's handler_module, or alone) is the same
/// create as `handler_module: X`.
#[must_use]
pub fn create_args(
    bundles: &BundleRegistry,
    args: crate::declared::kernel::CreateAgent,
) -> (Option<String>, Map<String, Value>) {
    let crate::declared::kernel::CreateAgent {
        handler_module,
        bundle,
        fields,
        ..
    } = args;
    let mut meta = fields;
    let mut hm = handler_module.filter(|h| !h.is_empty());
    if let Some(b) = bundle.filter(|b| !b.is_empty()) {
        // the bundle's own base (or none named): the kind IS the bundle. Another base:
        // it keeps its handler and names the bundle it inherits from.
        let h = hm.as_deref().unwrap_or("");
        if h.is_empty() || h == b || h == bundles.base_of(&b) {
            hm = Some(b);
        } else {
            meta.insert("bundle".into(), Value::String(b));
        }
    }
    if let Some(h) = &hm {
        apply(bundles, h, &mut meta);
    }
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
            let kind = kind_of(&hm, &e.value().meta.read().expect("meta"));
            let from = kind == name || kernel.bundles.meta_chain(&kind).iter().any(|m| m == name);
            from.then(|| (Arc::clone(e.value()), kernel.bundles.defaults(&kind)))
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
        let new = kernel.bundles.defaults(&kind_of(&hm, &mine));
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
