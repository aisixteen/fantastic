//! Unit tests for [`crate::lifecycle`].

use super::*;
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};

struct CountingBundle {
    deletes: Arc<AtomicUsize>,
}
#[async_trait]
impl crate::Bundle for CountingBundle {
    fn name(&self) -> &str {
        "counting"
    }
    async fn handle(
        &self,
        _id: &AgentId,
        _payload: &Value,
        _k: &Arc<Kernel>,
    ) -> Result<crate::Reply, crate::bundle::BundleError> {
        Ok(None)
    }
    async fn on_delete(
        &self,
        _id: &AgentId,
        _k: &Arc<Kernel>,
    ) -> Result<(), crate::bundle::BundleError> {
        self.deletes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn mk_kernel(deletes: Arc<AtomicUsize>) -> Arc<Kernel> {
    mk_kernel_with_storage(deletes, crate::storage::StorageMode::InMemory)
}

fn mk_kernel_with_storage(
    deletes: Arc<AtomicUsize>,
    storage: crate::storage::StorageMode,
) -> Arc<Kernel> {
    let mut kernel = Kernel::with_storage(storage.clone());
    kernel
        .bundles
        .register("counting.tools", CountingBundle { deletes });
    // Disk mode persists THROUGH a provider — register the fake `file_bridge`
    // store rooted at the workdir so a test can wire one (see `wire_fake_store`).
    if let Some(wd) = storage.workdir() {
        crate::test_support::register_fake_store(&mut kernel.bundles, wd);
    }
    // The ROOT LOADER — its `boot` is what subscribes the record writer, which
    // is the only path a record reaches disk by now.
    crate::test_support::register_root_loader(&mut kernel.bundles);
    Arc::new(kernel)
}

#[tokio::test]
async fn mint_id_format_is_bundle_underscore_hex6() {
    let id = mint_id("file_bridge.tools");
    assert!(id.starts_with("file_bridge_"));
    // 6 hex chars after the final underscore (the bundle name itself
    // contains an underscore, so split off the trailing token).
    let suffix = &id["file_bridge_".len()..];
    assert_eq!(suffix.len(), 6);
    assert!(suffix.chars().all(|c| c.is_ascii_hexdigit()));
}

#[tokio::test]
async fn create_then_delete_unregisters_and_calls_hook() {
    let tmp = tempfile::TempDir::new().unwrap();
    let deletes = Arc::new(AtomicUsize::new(0));
    // Disk-backed kernel so persistence::persist actually writes per-agent
    // agent.yaml files (the assertions below rely on that).
    let kernel = mk_kernel_with_storage(
        Arc::clone(&deletes),
        crate::storage::StorageMode::Disk(tmp.path().to_path_buf()),
    );
    // Stand up a root pointing at the tempdir.
    let root = Agent::new(
        AgentId::from("kernel_state"),
        Some("kernel_state.tools".to_string()),
        None,
        Map::new(),
        tmp.path().to_path_buf(),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    // The root boots FIRST (registration order), then the provider is wired —
    // that is what lets the store persist itself through itself.
    crate::test_support::boot_root(&kernel).await;
    crate::test_support::wire_fake_store(&kernel, tmp.path()).await;

    // Create one child via the system verb.
    let v = kernel
        .send(vec![Item::new(AgentId::from("kernel_state"), json!({"type": "create_agent", "parent_id": "kernel", "handler_module": "counting.tools", "id": "kid_1"}))])
        .await.remove(0).result;
    assert_eq!(v["id"], "kid_1");
    assert!(kernel.agents.contains_key(&AgentId::from("kid_1")));
    // DEBOUNCED: the record reaches disk through the root loader's writer.
    crate::test_support::settle_records().await;
    assert!(tmp.path().join("agents/kid_1/agent.yaml").exists());

    // Delete it.
    let v = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "delete_agent", "id": "kid_1"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(v["deleted"], true);
    assert!(!kernel.agents.contains_key(&AgentId::from("kid_1")));
    assert_eq!(deletes.load(Ordering::SeqCst), 1);
    crate::test_support::settle_records().await;
    assert!(!tmp.path().join("agents/kid_1").exists());
}

#[tokio::test]
async fn delete_refuses_locked() {
    let tmp = tempfile::TempDir::new().unwrap();
    let kernel = mk_kernel(Arc::new(AtomicUsize::new(0)));
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        tmp.path().to_path_buf(),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let _ = kernel
        .send(vec![Item::new(AgentId::from("kernel_state"), json!({"type": "create_agent", "parent_id": "kernel", "handler_module": "counting.tools", "id": "locked_1", "delete_lock": true}))])
        .await.remove(0).result;
    let v = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "delete_agent", "id": "locked_1"}),
        )])
        .await
        .remove(0)
        .result;
    // #27: the machine field, not the ad-hoc flag it replaced.
    assert_eq!(v["reason"], "locked");
    assert!(kernel.agents.contains_key(&AgentId::from("locked_1")));
}

#[tokio::test]
async fn cascade_delete_fires_hooks_depth_first() {
    let tmp = tempfile::TempDir::new().unwrap();
    let deletes = Arc::new(AtomicUsize::new(0));
    let kernel = mk_kernel(Arc::clone(&deletes));
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        tmp.path().to_path_buf(),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let _ = kernel
        .send(vec![Item::new(AgentId::from("kernel_state"), json!({"type": "create_agent", "parent_id": "kernel", "handler_module": "counting.tools", "id": "p_1"}))])
        .await.remove(0).result;
    // PLACEMENT IS `parent_id`, not the target. The verb is kernel-owned, so
    // these go to the root; the id names the OWNER of the new agent. Addressing
    // `p_1` used to make it the parent implicitly — that is the ambiguity #23
    // removed.
    for kid in ["c_1", "c_2"] {
        let _ = kernel
            .send(vec![Item::new(
                AgentId::from("kernel"),
                json!({"type": "create_agent", "parent_id": "p_1",
                       "handler_module": "counting.tools", "id": kid}),
            )])
            .await
            .remove(0)
            .result;
    }
    // Delete parent — both children's hooks must fire too.
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "delete_agent", "id": "p_1"}),
        )])
        .await
        .remove(0)
        .result;
    // 3 hook fires (p_1 + c_1 + c_2).
    assert_eq!(deletes.load(Ordering::SeqCst), 3);
    assert!(!kernel.agents.contains_key(&AgentId::from("p_1")));
    assert!(!kernel.agents.contains_key(&AgentId::from("c_1")));
    assert!(!kernel.agents.contains_key(&AgentId::from("c_2")));
}

/// Kernel + registered root, the shape every lifecycle test needs.
fn mk_rooted(tmp: &tempfile::TempDir) -> Arc<Kernel> {
    let kernel = mk_kernel(Arc::new(AtomicUsize::new(0)));
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        tmp.path().to_path_buf(),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    kernel
}

// ─── #22-#25 — kernel verbs are ROOT-owned, placement is `parent_id` ───

#[tokio::test]
async fn a_kernel_verb_at_a_non_root_agent_says_where_to_send_it() {
    // The verb acts on the TREE, so the agent named is the OPERAND, not the
    // actor. Answered everywhere, "send create_agent to X" read three ways and
    // a model spent 16 messages unable to resolve which — finishing with nothing
    // created. Refused in the HANDLER, so gate-denial probes stay valid.
    let tmp = tempfile::TempDir::new().unwrap();
    let kernel = mk_rooted(&tmp);
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "create_agent", "parent_id": "kernel",
                   "handler_module": "counting.tools", "id": "victim"}),
        )])
        .await
        .remove(0)
        .result;

    let r = kernel
        .send(vec![Item::new(
            AgentId::from("victim"),
            json!({"type": "list_agents"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], "not_here");
    assert!(r["hint"].as_str().unwrap().contains("kernel"));
    // Python's prose + the machine field naming the misaddressed verb.
    assert!(r["hint"].as_str().unwrap().contains("`parent_id` / `id`"));
    assert_eq!(r["verb"], "list_agents");
}

#[tokio::test]
async fn create_agent_demands_a_parent_and_resolves_it_like_a_target() {
    let tmp = tempfile::TempDir::new().unwrap();
    let kernel = mk_rooted(&tmp);

    // Absent ⇒ missing_arg, and the refusal says what to name.
    let missing = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "create_agent", "handler_module": "counting.tools", "id": "x"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(missing["reason"], "missing_arg");
    assert!(missing["hint"]
        .as_str()
        .unwrap()
        .contains("OWN the new one"));

    // Unknown ⇒ unknown_agent, carrying the live id index.
    let unknown = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "create_agent", "parent_id": "nope",
                   "handler_module": "counting.tools", "id": "x"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(unknown["reason"], "unknown_agent");
    assert!(unknown["agents"].is_array());

    // Named ⇒ the new agent lands THERE, not under whoever was addressed.
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "create_agent", "parent_id": "kernel",
                   "handler_module": "counting.tools", "id": "mid"}),
        )])
        .await
        .remove(0)
        .result;
    let rec = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "create_agent", "parent_id": "mid",
                   "handler_module": "counting.tools", "id": "leaf"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(
        rec["parent_id"], "mid",
        "the field that expresses placement must actually place it"
    );
}

/// C57: `create_agent` branches on the boot OUTCOME STATUS. A handler that
/// RAISED settles `failed` and is the one case python prints, on stdout, with
/// the result's `str()`; a boot that merely answered `{error:…}` (`errored`) or
/// completed prints nothing. Rust used to log a tracing WARN for every refusal.
#[tokio::test]
async fn a_raised_child_boot_prints_python_line_and_an_errored_one_is_silent() {
    struct BootBundle;
    #[async_trait::async_trait]
    impl crate::Bundle for BootBundle {
        fn name(&self) -> &str {
            "booty.tools"
        }
        async fn handle(
            &self,
            id: &AgentId,
            payload: &Value,
            _k: &Arc<Kernel>,
        ) -> Result<crate::Reply, crate::bundle::BundleError> {
            if payload.get("type").and_then(Value::as_str) == Some("boot") {
                if id.0.starts_with("raiser") {
                    return Err("boot blew up".into());
                }
                return Ok(Some(json!({"error": "sealed leg"})));
            }
            Ok(None)
        }
    }

    let mut kernel = Kernel::new();
    kernel.bundles.register("booty.tools", BootBundle);
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        std::path::PathBuf::new(),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));

    // A RAISED boot: the agent exists, its boot broke, and the create still
    // answers the record (surfaced, not fatal).
    let rec = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"create_agent","parent_id":"kernel","handler_module":"booty.tools","id":"raiser_1"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(rec["id"], json!("raiser_1"));
    // An ERRORED boot is silent and equally non-fatal.
    let rec = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"create_agent","parent_id":"kernel","handler_module":"booty.tools","id":"sealed_1"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(rec["id"], json!("sealed_1"));
    // Both are live — a boot outcome never removes the agent.
    assert!(kernel.agents.contains_key(&AgentId::from("raiser_1")));
    assert!(kernel.agents.contains_key(&AgentId::from("sealed_1")));
}

// ─── C26/C27 — create/update/delete refusals, python's prose + order ───

#[tokio::test]
async fn duplicate_id_refusal_is_python_text() {
    let tmp = tempfile::TempDir::new().unwrap();
    let kernel = mk_rooted(&tmp);
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"create_agent","parent_id":"kernel","handler_module":"counting.tools","id":"dup"}),
        )])
        .await
        .remove(0)
        .result;
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"create_agent","parent_id":"kernel","handler_module":"counting.tools","id":"dup"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["error"], "agent 'dup' exists");
    assert_eq!(r["reason"], "exists");
    assert_eq!(r["hint"], "pick another id, or update the existing agent");
    assert_eq!(r["id"], "dup");
    // KEY ORDER: error, reason, hint, id (python `refusal(EXISTS, ..., hint=…, id=…)`).
    let keys: Vec<&String> = r.as_object().unwrap().keys().collect();
    assert_eq!(keys, vec!["error", "reason", "hint", "id"]);
}

#[tokio::test]
async fn missing_hint_orders_hint_before_arg() {
    let tmp = tempfile::TempDir::new().unwrap();
    let kernel = mk_rooted(&tmp);
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"create_agent","handler_module":"counting.tools","id":"x"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], "missing_arg");
    assert_eq!(
        r["hint"],
        "name the agent that will OWN the new one — your own id for a tool \
         you keep, or the root ('kernel_state') for a top-level agent"
    );
    // python `missing(verb, arg, hint=hint)` → `refusal(code, error, hint=hint,
    // arg=arg)`: `hint` is a named parameter of `refusal`, inserted BEFORE
    // `arg`, which rides `**extra`.
    let keys: Vec<&String> = r.as_object().unwrap().keys().collect();
    assert_eq!(keys, vec!["error", "reason", "hint", "arg"]);
}

#[tokio::test]
async fn empty_required_args_are_missing() {
    let tmp = tempfile::TempDir::new().unwrap();
    let kernel = mk_rooted(&tmp);
    let empty_hm = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"create_agent","handler_module":"","parent_id":"kernel","id":"x"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(empty_hm["reason"], "missing_arg");
    assert_eq!(empty_hm["arg"], "handler_module");
    let empty_pid = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"create_agent","handler_module":"counting.tools","parent_id":"","id":"x"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(empty_pid["reason"], "missing_arg");
    assert_eq!(empty_pid["arg"], "parent_id");
}

#[tokio::test]
async fn create_refusal_precedence_matches_python() {
    // handler_module -> parent_id -> resolve parent -> reserved keys (sorted)
    // -> grant shape -> validate_meta -> guard id -> exists. Each probe below
    // trips exactly ONE gate earlier than the next one down the list would.
    let tmp = tempfile::TempDir::new().unwrap();
    let kernel = mk_rooted(&tmp);

    // Both handler_module AND parent_id absent -> handler_module wins (checked first).
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"create_agent"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["arg"], "handler_module");

    // parent_id present but unresolvable AND a reserved key present -> parent
    // resolution wins (checked before the reserved-key sweep).
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"create_agent","handler_module":"counting.tools",
                   "parent_id":"ghost","root_path":"/evil"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], "unknown_agent");

    // A resolvable parent but TWO reserved keys present -> the alphabetically
    // FIRST of `sorted(RESERVED_RECORD_KEYS)` = ["ctx", "parent", "root_path"]
    // is named, never whichever the payload listed first.
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"create_agent","handler_module":"counting.tools",
                   "parent_id":"kernel","root_path":"/evil","parent":"x"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["arg"], "parent");

    // Reserved keys clear but the grant shape is bad -> invalid_meta before
    // `guard_agent_id`/`exists` are ever reached (id is a number, not a str).
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"create_agent","handler_module":"counting.tools",
                   "parent_id":"kernel","can_call":"*","id":123}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], "invalid_meta");
    // python NAMES THE TYPE THAT ARRIVED: `— got str. To open wide, …`.
    assert_eq!(
        r["error"],
        "can_call must be a grant MAP {\"<id|$token>\": \"*\" | [\"<verb>\", …]} \
         — got str. To open wide, grant per key (e.g. {\"$self\": \"*\", \"ai\": \"*\"}), \
         never a bare value"
    );

    // Grant shape fine, id invalid (a digit-only name) -> guard_agent_id fires
    // before the (irrelevant here) exists check.
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"create_agent","handler_module":"counting.tools",
                   "parent_id":"kernel","id":"123"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], "bad_arg");
}

/// `$subtree:` and heritable entries are judged at WRITE time — python
/// `_bad_grant_shape`, text for text.
#[tokio::test]
async fn subtree_and_heritable_shapes_are_refused_at_write_time() {
    let tmp = tempfile::TempDir::new().unwrap();
    let kernel = mk_rooted(&tmp);
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"create_agent","handler_module":"counting.tools",
                   "parent_id":"kernel","id":"hub"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(r.get("error").is_none(), "{r}");
    let heritable = json!({"verbs": ["list"], "for": "descendants"});
    let cases = [
        (
            json!({"can_call": {"$subtree:hub": ["list"]}}),
            "can_call['$subtree:hub']: `$subtree:` is callable_by-only — it names who \
             may call you, never whom you may call",
        ),
        (
            json!({"callable_by": {"$subtree:": ["list"]}}),
            "callable_by['$subtree:']: `$subtree:` names one LOCAL agent id \
             (`$subtree:<id>`, no `|`)",
        ),
        (
            json!({"callable_by": {"$subtree:g1|x": ["list"]}}),
            "callable_by['$subtree:g1|x']: `$subtree:` names one LOCAL agent id \
             (`$subtree:<id>`, no `|`)",
        ),
        (
            json!({"callable_by": {"hub": heritable}}),
            "callable_by['hub']: a heritable entry {\"verbs\": [...], \"for\": \
             \"descendants\"} is can_call-only",
        ),
        (
            json!({"can_call": {"$parent": heritable}}),
            "can_call['$parent']: a heritable entry names a literal target id, never a token",
        ),
        (
            json!({"can_call": {"served": {"verbs": ["list"], "for": "siblings"}}}),
            "can_call['served'] must be exactly {\"verbs\": [\"<verb>\", …], \
             \"for\": \"descendants\"}",
        ),
        (
            json!({"can_call": {"served": {"verbs": [], "for": "descendants"}}}),
            "can_call['served'] must be exactly {\"verbs\": [\"<verb>\", …], \
             \"for\": \"descendants\"}",
        ),
        (
            json!({"can_call": {"served": {"verbs": ["list"], "for": "descendants", "x": 1}}}),
            "can_call['served'] must be exactly {\"verbs\": [\"<verb>\", …], \
             \"for\": \"descendants\"}",
        ),
        (
            json!({"can_call": {"served": 7}}),
            "can_call['served'] must be \"*\", a list of verb strings, or (can_call) \
             {\"verbs\": [...], \"for\": \"descendants\"}",
        ),
    ];
    for (patch, want) in cases {
        let mut payload = json!({"type": "update_agent", "id": "hub"});
        for (k, v) in patch.as_object().unwrap() {
            payload[k] = v.clone();
        }
        let r = kernel
            .send(vec![Item::new(AgentId::from("kernel"), payload)])
            .await
            .remove(0)
            .result;
        assert_eq!(r["reason"], "invalid_meta", "{patch}: {r}");
        assert_eq!(r["error"], want, "{patch}");
    }
}

/// A heritable value is a MAP grant value: it is stored as written and the
/// record view shows it verbatim; `$subtree:<id>` in callable_by likewise.
#[tokio::test]
async fn a_heritable_grant_is_stored_and_reflected_verbatim() {
    let tmp = tempfile::TempDir::new().unwrap();
    let kernel = mk_rooted(&tmp);
    let can = json!({"$self": "*", "served": {"verbs": ["list", "allow"], "for": "descendants"}});
    let by = json!({"$subtree:hub": ["list"]});
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"create_agent","handler_module":"counting.tools",
                   "parent_id":"kernel","id":"hub","can_call": can, "callable_by": by}),
        )])
        .await
        .remove(0)
        .result;
    assert!(r.get("error").is_none(), "{r}");
    let hub = kernel
        .agents
        .get(&AgentId::from("hub"))
        .map(|e| Arc::clone(&e))
        .unwrap();
    let meta = hub.meta.read().unwrap().clone();
    // the baseline keys are filled in beside what was written
    assert_eq!(meta["can_call"]["served"], can["served"]);
    assert_eq!(meta["callable_by"]["$subtree:hub"], by["$subtree:hub"]);
    let listed = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "list_agents"}),
        )])
        .await
        .remove(0)
        .result;
    let rec = listed["agents"]
        .as_array()
        .and_then(|a| a.iter().find(|r| r["id"] == "hub"))
        .unwrap_or_else(|| panic!("hub not listed: {listed}"));
    assert_eq!(rec["can_call"], meta["can_call"], "{rec}");
    assert_eq!(rec["callable_by"], meta["callable_by"], "{rec}");
}

#[tokio::test]
async fn update_agent_checks_reserved_keys_before_resolving_id() {
    // python: `for k in ("handler_module", *sorted(RESERVED_RECORD_KEYS))` runs
    // BEFORE `self.ctx.agents.get(target_id)` — an unknown target with a
    // reserved key present still answers `bad_arg`, not `unknown_agent`.
    let tmp = tempfile::TempDir::new().unwrap();
    let kernel = mk_rooted(&tmp);
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"update_agent","id":"ghost","handler_module":"x.tools"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], "bad_arg");
    assert_eq!(r["arg"], "handler_module");

    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"update_agent","id":"ghost","root_path":"/evil","parent":"x"}),
        )])
        .await
        .remove(0)
        .result;
    // `handler_module` absent here -> sorted(RESERVED_RECORD_KEYS) =
    // ["ctx","parent","root_path"] -> `parent` is alphabetically first of the
    // two present.
    assert_eq!(r["arg"], "parent");

    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"update_agent","id":""}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], "missing_arg");
    assert_eq!(r["arg"], "id");
}

#[tokio::test]
async fn delete_agent_refuses_the_root_with_python_text() {
    let tmp = tempfile::TempDir::new().unwrap();
    let kernel = mk_rooted(&tmp);
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"delete_agent","id":"kernel_state"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(
        r["error"],
        "delete_agent: kernel_state is the ROOT — it owns persistence and \
         answers the kernel verbs; deleting it would leave no kernel"
    );
    assert_eq!(r["reason"], "bad_arg");
    assert_eq!(r["deleted"], false);
    let keys: Vec<&String> = r.as_object().unwrap().keys().collect();
    assert_eq!(keys, vec!["error", "reason", "arg", "id", "deleted"]);
}

#[tokio::test]
async fn delete_agent_empty_id_is_missing_arg() {
    let tmp = tempfile::TempDir::new().unwrap();
    let kernel = mk_rooted(&tmp);
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"delete_agent","id":""}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], "missing_arg");
    assert_eq!(r["arg"], "id");
}

#[tokio::test]
async fn delete_lock_refusal_is_python_text_and_key_order() {
    let tmp = tempfile::TempDir::new().unwrap();
    let kernel = mk_rooted(&tmp);
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"create_agent","parent_id":"kernel","handler_module":"counting.tools","id":"P","delete_lock":true}),
        )])
        .await
        .remove(0)
        .result;
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"delete_agent","id":"P"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(
        r["error"],
        "delete_agent: 'P' blocked by delete_lock on 'P'; clear it via \
         update_agent before deleting"
    );
    assert_eq!(r["reason"], "locked");
    assert_eq!(
        r["hint"],
        "clear the lock with update_agent, then delete again"
    );
    assert_eq!(r["blocked_by"], "P");
    assert_eq!(r["id"], "P");
    let keys: Vec<&String> = r.as_object().unwrap().keys().collect();
    assert_eq!(keys, vec!["error", "reason", "hint", "blocked_by", "id"]);
}
