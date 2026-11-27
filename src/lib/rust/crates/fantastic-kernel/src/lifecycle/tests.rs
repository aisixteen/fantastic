//! Unit tests for [`crate::lifecycle`].

use super::*;
use crate::outcome::Item;
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};

struct CountingBundle {
    deletes: Arc<AtomicUsize>,
}
#[async_trait]
impl crate::Handler for CountingBundle {
    type Call = crate::test_support::NoCall;
    fn man(&self) -> &'static str {
        "A test stand-in."
    }
    fn name(&self) -> &str {
        "counting"
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
    if let Some(wd) = storage.work_dir() {
        crate::test_support::register_fake_store(&mut kernel.bundles, wd);
    }
    // The ROOT LOADER — its `boot` is what subscribes the record writer, which
    // is the only path a record reaches disk by now.
    crate::test_support::register_root_loader(&mut kernel.bundles);
    Arc::new(kernel)
}

#[test]
fn mint_id_takes_the_second_last_dotted_segment() {
    // python `handler_module.split(".")[-2] if "." in handler_module`.
    for (hm, bundle) in [
        ("a.b.tools", "b"),
        ("x.y", "x"),
        ("pkg.ts", "pkg"),
        ("web.tools", "web"),
        ("plain", "plain"),
        (".tools", ""),
    ] {
        let id = mint_id(hm);
        let (head, tail) = id.rsplit_once('_').expect("bundle_hex6");
        assert_eq!(head, bundle, "{hm} -> {id}");
        assert_eq!(tail.len(), 6);
    }
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
        crate::storage::StorageMode::Disk {
            state_dir: tmp.path().to_path_buf().join(".fantastic"),
            work_dir: tmp.path().to_path_buf(),
        },
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
    let v = crate::test_support::send_settled(&kernel, vec![Item::new(AgentId::from("kernel_state"), json!({"type": "create_agent", "parent_id": "kernel", "handler_module": "counting.tools", "id": "kid_1"}))])
        .await.remove(0).result;
    assert_eq!(v["id"], "kid_1");
    assert!(kernel.agents.contains_key(&AgentId::from("kid_1")));
    assert!(kernel.exists("kid_1"));
    assert!(!kernel.exists("no_such_agent"));
    // The record reaches disk through the root loader's writer; the flush
    // barrier waits for it.
    kernel.flush_records().await;
    assert!(tmp.path().join("agents/kid_1/agent.yaml").exists());

    // Delete it.
    let v = crate::test_support::send_settled(&kernel, vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "delete_agent", "id": "kid_1"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(v["deleted"], true);
    assert!(!kernel.agents.contains_key(&AgentId::from("kid_1")));
    assert!(!kernel.exists("kid_1"));
    assert_eq!(deletes.load(Ordering::SeqCst), 1);
    kernel.flush_records().await;
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
    let _ = crate::test_support::send_settled(&kernel, vec![Item::new(AgentId::from("kernel_state"), json!({"type": "create_agent", "parent_id": "kernel", "handler_module": "counting.tools", "id": "locked_1", "delete_lock": true}))])
        .await.remove(0).result;
    let v = crate::test_support::send_settled(&kernel, vec![Item::new(
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
    let _ = crate::test_support::send_settled(&kernel, vec![Item::new(AgentId::from("kernel_state"), json!({"type": "create_agent", "parent_id": "kernel", "handler_module": "counting.tools", "id": "p_1"}))])
        .await.remove(0).result;
    // PLACEMENT IS `parent_id`, not the target. The verb is kernel-owned, so
    // these go to the root; the id names the OWNER of the new agent — never the
    // addressed agent implicitly.
    for kid in ["c_1", "c_2"] {
        let _ = crate::test_support::send_settled(&kernel, vec![Item::new(
                AgentId::from("kernel"),
                json!({"type": "create_agent", "parent_id": "p_1",
                       "handler_module": "counting.tools", "id": kid}),
            )])
            .await
            .remove(0)
            .result;
    }
    // Delete parent — both children's hooks must fire too.
    let _ = crate::test_support::send_settled(&kernel, vec![Item::new(
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
    let _ = crate::test_support::send_settled(&kernel, vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "create_agent", "parent_id": "kernel",
                   "handler_module": "counting.tools", "id": "victim"}),
        )])
        .await
        .remove(0)
        .result;

    let r = crate::test_support::send_settled(&kernel, vec![Item::new(
            AgentId::from("victim"),
            json!({"type": "ps"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], "not_here");
    assert!(r["hint"].as_str().unwrap().contains("kernel"));
    // Python's prose + the machine field naming the misaddressed verb.
    assert!(r["hint"].as_str().unwrap().contains("`parent_id` / `id`"));
    assert_eq!(r["verb"], "ps");
}

#[tokio::test]
async fn create_agent_demands_a_parent_and_resolves_it_like_a_target() {
    let tmp = tempfile::TempDir::new().unwrap();
    let kernel = mk_rooted(&tmp);

    // Absent ⇒ missing_arg, and the refusal says what to name.
    let missing = crate::test_support::send_settled(&kernel, vec![Item::new(
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

    // Unknown ⇒ unknown_agent, carrying no id index.
    let unknown = crate::test_support::send_settled(&kernel, vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "create_agent", "parent_id": "nope",
                   "handler_module": "counting.tools", "id": "x"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(unknown["reason"], "unknown_agent");
    assert!(unknown.get("agents").is_none(), "{unknown}");

    // Named ⇒ the new agent lands THERE, not under whoever was addressed.
    let _ = crate::test_support::send_settled(&kernel, vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "create_agent", "parent_id": "kernel",
                   "handler_module": "counting.tools", "id": "mid"}),
        )])
        .await
        .remove(0)
        .result;
    let rec = crate::test_support::send_settled(&kernel, vec![Item::new(
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

/// The `agent_created` event lands on the PARENT's inbox at every depth — put
/// there as kernel bookkeeping, not emitted as the root (where the gate would
/// judge root→parent and the root's baseline grant reaches depth 1 only).
#[tokio::test]
async fn agent_created_reaches_a_parent_at_any_depth() {
    let tmp = tempfile::TempDir::new().unwrap();
    let kernel = mk_rooted(&tmp);
    let create = |parent: &'static str, id: &'static str| {
        let kernel = Arc::clone(&kernel);
        async move {
            crate::test_support::send_settled(&kernel, vec![Item::new(
                    AgentId::from("kernel"),
                    json!({"type": "create_agent", "parent_id": parent,
                           "handler_module": "counting.tools", "id": id}),
                )])
                .await
                .remove(0)
                .result
        }
    };
    assert_eq!(create("kernel", "kv4_a").await["id"], "kv4_a");
    assert_eq!(create("kv4_a", "kv4_b").await["id"], "kv4_b"); // depth 2
    let mut rx = kernel.ensure_inbox(&AgentId::from("kv4_b"));
    assert_eq!(
        create("kv4_b", "kv4_by_operator").await["id"],
        "kv4_by_operator"
    );
    // a create line on itself: kv4_b creates under itself
    crate::test_support::grant(
        &kernel,
        "kv4_b",
        "kv4_b",
        json!({"create": {"who": ["*"], "what": {"counting.tools": {}}}}),
    );
    let by_self =
        crate::test_support::as_agent(AgentId::from("kv4_b"), create("kv4_b", "kv4_by_self")).await;
    assert_eq!(by_self["id"], "kv4_by_self", "{by_self}");
    let mut created = Vec::new();
    while let Some(item) = rx.try_recv() {
        if item.payload["type"] == "agent_created" {
            created.push(item.payload["id"].clone());
        }
    }
    assert_eq!(
        created,
        vec![json!("kv4_by_operator"), json!("kv4_by_self")]
    );
}

/// `create_agent` branches on the boot OUTCOME STATUS. A handler that
/// RAISED settles `failed` and is the one case python prints, on stdout, with
/// the result's `str()`; a boot that merely answered `{error:…}` (`errored`) or
/// completed prints nothing.
#[tokio::test]
async fn a_raised_child_boot_prints_python_line_and_an_errored_one_is_silent() {
    struct BootBundle;
    crate::stand_in_verbs!(bootbundle_verbs { BootV "boot" {  } });
    #[async_trait::async_trait]
    impl crate::Handler for BootBundle {
        type Call = bootbundle_verbs::Call;
        fn man(&self) -> &'static str {
            "A test stand-in."
        }
        fn name(&self) -> &str {
            "booty.tools"
        }
        fn verbs(&self) -> &'static [crate::Verb] {
            bootbundle_verbs::VERBS
        }
        fn dynamic_verbs(&self, agent: &crate::Agent) -> Vec<crate::DynVerb> {
            bootbundle_verbs::dynamic_verbs(agent)
        }
        fn record(&self) -> &'static crate::RecordDecl {
            crate::fixture_record!()
        }
        async fn handle(
            &self,
            id: &AgentId,
            call: Self::Call,
            _k: &Arc<Kernel>,
        ) -> Result<crate::Reply, crate::bundle::BundleError> {
            let payload = &call.payload();
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
    let rec = crate::test_support::send_settled(&kernel, vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"create_agent","parent_id":"kernel","handler_module":"booty.tools","id":"raiser_1"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(rec["id"], json!("raiser_1"));
    // An ERRORED boot is silent and equally non-fatal.
    let rec = crate::test_support::send_settled(&kernel, vec![Item::new(
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
    let _ = crate::test_support::send_settled(&kernel, vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"create_agent","parent_id":"kernel","handler_module":"counting.tools","id":"dup"}),
        )])
        .await
        .remove(0)
        .result;
    let r = crate::test_support::send_settled(&kernel, vec![Item::new(
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
    let r = crate::test_support::send_settled(&kernel, vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"create_agent","handler_module":"counting.tools","id":"x"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], "missing_arg");
    // the hint is the verb's declared argument list and the missing argument's doc
    let hint = r["hint"].as_str().unwrap();
    assert!(
        hint.starts_with(
            "create_agent takes — args: handler_module:string of at least 1 character(s) (req), parent_id:string of at least 1 character(s) (req)"
        ),
        "{hint}"
    );
    assert!(
        hint.contains("The agent that will OWN the new one"),
        "{hint}"
    );
    // `hint` is inserted BEFORE `arg`, which rides beside `verb`
    let keys: Vec<&String> = r.as_object().unwrap().keys().collect();
    assert_eq!(keys, vec!["error", "reason", "hint", "arg", "verb"]);
}

#[tokio::test]
async fn empty_required_args_are_refused_by_their_declaration() {
    let tmp = tempfile::TempDir::new().unwrap();
    let kernel = mk_rooted(&tmp);
    let empty_hm = crate::test_support::send_settled(&kernel, vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"create_agent","handler_module":"","parent_id":"kernel","id":"x"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(empty_hm["reason"], "bad_arg");
    assert_eq!(empty_hm["arg"], "handler_module");
    let empty_pid = crate::test_support::send_settled(&kernel, vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"create_agent","handler_module":"counting.tools","parent_id":"","id":"x"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(empty_pid["reason"], "bad_arg");
    assert_eq!(empty_pid["arg"], "parent_id");
}

#[tokio::test]
async fn create_refusal_precedence_matches_python() {
    // handler_module -> parent_id -> resolve parent -> reserved keys (sorted)
    // -> grant shape -> validate_meta -> guard id -> exists. Each probe below
    // trips exactly ONE gate earlier than the next one down the list would.
    let tmp = tempfile::TempDir::new().unwrap();
    let kernel = mk_rooted(&tmp);

    // Both handler_module AND parent_id absent -> the first declared REQUIRED argument
    // (handler_module) is refused first, by the shared entry's check of the declaration.
    let r = crate::test_support::send_settled(&kernel, vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"create_agent"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["arg"], "handler_module");
    // parent_id given, handler_module absent -> handler_module.
    let r = crate::test_support::send_settled(&kernel, vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"create_agent","parent_id":"kernel"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["arg"], "handler_module");

    // parent_id present but unresolvable AND a reserved key present -> parent
    // resolution wins (checked before the reserved-key sweep).
    let r = crate::test_support::send_settled(&kernel, vec![Item::new(
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
    let r = crate::test_support::send_settled(&kernel, vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"create_agent","handler_module":"counting.tools",
                   "parent_id":"kernel","root_path":"/evil","parent":"x"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["arg"], "parent");

    // Reserved keys clear but the `can` shape is bad: the target's record entry
    // refuses the field by name (the declared shape: a map of target → one level).
    let r = crate::test_support::send_settled(&kernel, vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"create_agent","handler_module":"counting.tools",
                   "parent_id":"kernel","can":"*","id":"granted"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], "bad_arg");
    assert_eq!(r["arg"], "can");

    // An id that is not one safe path segment (or is all digits) is refused by the
    // declaration of `id`, before any agent exists.
    for bad_id in ["..", "a/b", ".hidden", "a|b", "123"] {
        let r = crate::test_support::send_settled(&kernel, vec![Item::new(
                AgentId::from("kernel"),
                json!({"type":"create_agent","handler_module":"counting.tools",
                       "parent_id":"kernel","id":bad_id}),
            )])
            .await
            .remove(0)
            .result;
        assert_eq!(r["reason"], "bad_arg", "{bad_id}: {r}");
        assert_eq!(r["arg"], "id", "{bad_id}: {r}");
    }
}

#[tokio::test]
async fn update_agent_refuses_substrate_wiring_through_the_targets_bundle() {
    // The record's fields are judged by the TARGET's bundle (its shared record
    // entry): `handler_module` is fixed at create, and `root_path`/`ctx`/`parent`
    // name substrate wiring — each refused by name, `parent` (alphabetically first of
    // the sorted reserved keys) before `root_path`.
    let tmp = tempfile::TempDir::new().unwrap();
    let kernel = mk_rooted(&tmp);
    let made = crate::test_support::send_settled(&kernel, vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"create_agent","parent_id":"kernel",
                   "handler_module":"counting.tools","id":"wired"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(made["id"], "wired", "{made}");
    let r = crate::test_support::send_settled(&kernel, vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"update_agent","id":"wired","handler_module":"x.tools"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], "bad_arg");
    assert_eq!(r["arg"], "handler_module");

    let r = crate::test_support::send_settled(&kernel, vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"update_agent","id":"wired","root_path":"/evil","parent":"x"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], "bad_arg");
    assert_eq!(r["arg"], "parent");

    // where an agent sits is the tree's, never its record's
    let r = crate::test_support::send_settled(&kernel, vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"update_agent","id":"wired","parent_id":"kernel_state"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], "bad_arg");
    assert_eq!(r["arg"], "parent_id");

    // an agent that is not there is the unknown agent
    let r = crate::test_support::send_settled(&kernel, vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"update_agent","id":"ghost","display_name":"x"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], "unknown_agent");

    let r = crate::test_support::send_settled(&kernel, vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"update_agent","id":""}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], "bad_arg");
    assert_eq!(r["arg"], "id");
}

#[tokio::test]
async fn delete_agent_refuses_the_root_with_python_text() {
    let tmp = tempfile::TempDir::new().unwrap();
    let kernel = mk_rooted(&tmp);
    let r = crate::test_support::send_settled(&kernel, vec![Item::new(
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
async fn delete_agent_empty_id_is_refused_by_its_declaration() {
    let tmp = tempfile::TempDir::new().unwrap();
    let kernel = mk_rooted(&tmp);
    let r = crate::test_support::send_settled(&kernel, vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"delete_agent","id":""}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], "bad_arg");
    assert_eq!(r["arg"], "id");
}

#[tokio::test]
async fn delete_lock_refusal_is_python_text_and_key_order() {
    let tmp = tempfile::TempDir::new().unwrap();
    let kernel = mk_rooted(&tmp);
    let _ = crate::test_support::send_settled(&kernel, vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"create_agent","parent_id":"kernel","handler_module":"counting.tools","id":"P","delete_lock":true}),
        )])
        .await
        .remove(0)
        .result;
    let r = crate::test_support::send_settled(&kernel, vec![Item::new(
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

#[tokio::test]
async fn delete_lock_on_a_grandchild_refuses_the_cascade() {
    // `delete_lock` is the record's declared bool: a lock on a GRANDCHILD refuses the
    // cascade, and a number is no lock — it is refused as the wrong type.
    let tmp = tempfile::TempDir::new().unwrap();
    let kernel = mk_rooted(&tmp);
    let r = crate::test_support::send_settled(&kernel, vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"create_agent","parent_id":"kernel","handler_module":"counting.tools",
                   "id":"N","delete_lock":1}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(r["arg"], "delete_lock", "{r}");
    for (id, parent, lock) in [("P", "kernel", json!(false)), ("C", "P", json!(true))] {
        let r = crate::test_support::send_settled(&kernel, vec![Item::new(
                AgentId::from("kernel"),
                json!({"type":"create_agent","parent_id":parent,"handler_module":"counting.tools","id":id,"delete_lock":lock}),
            )])
            .await
            .remove(0)
            .result;
        assert!(r.get("error").is_none(), "{r}");
    }
    let r = crate::test_support::send_settled(&kernel, vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"delete_agent","id":"P"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], "locked", "{r}");
    assert_eq!(r["blocked_by"], "C");
    assert!(kernel.agents.contains_key(&AgentId::from("C")));
}

#[tokio::test]
async fn lifecycle_events_name_an_empty_display_name_by_id() {
    // `display_name or id`: "" is no name in added/removed and the snapshot.
    let tmp = tempfile::TempDir::new().unwrap();
    let kernel = mk_rooted(&tmp);
    let seen: Arc<std::sync::Mutex<Vec<Value>>> = Arc::default();
    let sink = Arc::clone(&seen);
    let _tap = kernel.add_state_subscriber(Arc::new(move |ev, _body| {
        sink.lock().unwrap().push(ev.clone());
    }));
    let _ = crate::test_support::send_settled(&kernel, vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"create_agent","parent_id":"kernel","handler_module":"counting.tools","id":"Q","display_name":""}),
        )])
        .await;
    let snap = kernel.state_snapshot();
    let row = snap.iter().find(|r| r["agent_id"] == "Q").expect("Q row");
    assert_eq!(row["name"], "Q");
    let _ = crate::test_support::send_settled(&kernel, vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"delete_agent","id":"Q"}),
        )])
        .await;
    let mut kinds = Vec::new();
    for ev in seen.lock().unwrap().iter() {
        if ev["agent_id"] == "Q" && (ev["kind"] == "added" || ev["kind"] == "removed") {
            assert_eq!(ev["name"], "Q", "{ev}");
            kinds.push(ev["kind"].as_str().unwrap().to_string());
        }
    }
    assert_eq!(kinds, vec!["added", "removed"]);
}

/// Records what `keep_memory` said inside each `on_delete` hook.
struct KeepProbe {
    seen: Arc<std::sync::Mutex<Vec<(String, bool)>>>,
}
#[async_trait]
impl crate::Handler for KeepProbe {
    type Call = crate::test_support::NoCall;
    fn man(&self) -> &'static str {
        "A test stand-in."
    }
    fn name(&self) -> &str {
        "keep_probe"
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
    async fn on_delete(
        &self,
        id: &AgentId,
        k: &Arc<Kernel>,
    ) -> Result<(), crate::bundle::BundleError> {
        self.seen
            .lock()
            .unwrap()
            .push((id.0.clone(), keep_memory(k, id)));
        Ok(())
    }
}

#[tokio::test]
async fn delete_agent_keep_memory_flag_is_scoped_to_one_delete() {
    let tmp = tempfile::TempDir::new().unwrap();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut kernel = Kernel::new();
    kernel.bundles.register(
        "keep_probe.tools",
        KeepProbe {
            seen: Arc::clone(&seen),
        },
    );
    let kernel = Arc::new(kernel);
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
    for (id, parent) in [("p_1", "kernel"), ("c_1", "p_1"), ("q_1", "kernel")] {
        let _ = crate::test_support::send_settled(&kernel, vec![Item::new(
                AgentId::from("kernel"),
                json!({"type": "create_agent", "parent_id": parent,
                       "handler_module": "keep_probe.tools", "id": id}),
            )])
            .await
            .remove(0)
            .result;
    }
    let del = |payload: Value| {
        let kernel = Arc::clone(&kernel);
        async move {
            crate::test_support::send_settled(&kernel, vec![Item::new(AgentId::from("kernel_state"), payload)])
                .await
                .remove(0)
                .result
        }
    };
    let out = del(json!({"type": "delete_agent", "id": "p_1", "keep_memory": true})).await;
    assert_eq!(out["deleted"], true, "{out}");
    {
        let mut s = seen.lock().unwrap();
        s.sort();
        assert_eq!(
            *s,
            vec![("c_1".to_string(), true), ("p_1".to_string(), true)]
        );
        s.clear();
    }
    assert!(!keep_memory(&kernel, &AgentId::from("p_1")), "cleared");
    let out = del(json!({"type": "delete_agent", "id": "q_1"})).await;
    assert_eq!(out["deleted"], true, "{out}");
    assert_eq!(*seen.lock().unwrap(), vec![("q_1".to_string(), false)]);
}

// ─── create / update / delete checked on the operand (grant model v9) ───

/// A stand-in that acts for its ASKER: `make` creates through the asker's send,
/// `make_direct` through `create_agent` with `current_origin()`, `zap` deletes
/// and `touch` updates through the asker's send.
struct AskerBundle;

crate::stand_in_verbs!(asker_verbs {
    MakeV "make" { parent: String, id: String },
    MakeDirectV "make_direct" { parent: String, id: String },
    ZapV "zap" { id: String },
    TouchV "touch" { id: String, field: String, value: serde_json::Value },
});
#[async_trait]
impl crate::Handler for AskerBundle {
    type Call = asker_verbs::Call;
    fn man(&self) -> &'static str {
        "A test stand-in."
    }
    fn name(&self) -> &str {
        "asker"
    }
    fn verbs(&self) -> &'static [crate::Verb] {
        asker_verbs::VERBS
    }
    fn dynamic_verbs(&self, agent: &crate::Agent) -> Vec<crate::DynVerb> {
        asker_verbs::dynamic_verbs(agent)
    }
    fn record(&self) -> &'static crate::RecordDecl {
        crate::fixture_record!()
    }
    async fn handle(
        &self,
        _id: &AgentId,
        call: Self::Call,
        k: &Arc<Kernel>,
    ) -> Result<crate::Reply, crate::bundle::BundleError> {
        let p = call.payload();
        let create = json!({"type": "create_agent", "parent_id": p["parent"],
                            "handler_module": "counting.tools", "id": p["id"]});
        let to_kernel = |payload: Value| Item::new(AgentId::from("kernel"), payload);
        Ok(Some(match p["type"].as_str().unwrap_or("") {
            "make" => crate::test_support::settled_for_asker(k, vec![to_kernel(create)]).await.remove(0).result,
            "make_direct" => {
                let mut fields = create;
                if let Some(m) = fields.as_object_mut() {
                    m.remove("type");
                }
                let args: crate::declared::kernel::CreateAgent =
                    serde_json::from_value(fields).expect("create args");
                let asker = crate::send::current_origin();
                create_agent(k, asker.as_ref(), args).await
            }
            "zap" => {
                let del = json!({"type": "delete_agent", "id": p["id"]});
                crate::test_support::settled_for_asker(k, vec![to_kernel(del)]).await.remove(0).result
            }
            _ => {
                let mut upd = json!({"type": "update_agent", "id": p["id"]});
                upd[p["field"].as_str().unwrap_or("")] = p["value"].clone();
                crate::test_support::settled_for_asker(k, vec![to_kernel(upd)]).await.remove(0).result
            }
        }))
    }
}

/// A rooted kernel with a meta bundle `chatlike` (base `counting.tools`, record
/// `model` + `temperature`) and the asker stand-in.
fn mk_v9_kernel(tmp: &tempfile::TempDir) -> Arc<Kernel> {
    let mut kernel = Kernel::new();
    kernel
        .bundles
        .register("counting.tools", CountingBundle { deletes: Arc::new(AtomicUsize::new(0)) });
    kernel.bundles.register("asker.tools", AskerBundle);
    kernel
        .bundles
        .register_meta_yaml(
            "chatlike",
            "base: counting.tools\nsentence: A chat stand-in.\nrecord:\n  model: base-model\n  temperature: 0.5\n",
        )
        .expect("meta bundle");
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        tmp.path().to_path_buf(),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(root);
    kernel
}

/// The operator creates `id` of `bundle` under `parent`, with `can`.
async fn op_create(kernel: &Arc<Kernel>, parent: &str, id: &str, bundle: &str, can: Value) {
    let mut p = json!({"type": "create_agent", "parent_id": parent, "handler_module": bundle, "id": id});
    if !can.is_null() {
        p["can"] = can;
    }
    let r = crate::test_support::send_settled(kernel, vec![Item::new(AgentId::from("kernel"), p)])
        .await
        .remove(0)
        .result;
    assert_eq!(r["id"], json!(id), "{r}");
}

/// `payload` to `target`, sent as `agent`.
async fn send_as(kernel: &Arc<Kernel>, agent: &str, target: &str, payload: Value) -> Value {
    let k = Arc::clone(kernel);
    let t = AgentId::from(target);
    crate::test_support::as_agent(AgentId::from(agent), async move {
        crate::test_support::send_settled(&k, vec![Item::new(t, payload)]).await.remove(0).result
    })
    .await
}

fn create_line(who: Value, what: Value) -> Value {
    json!({"create": {"who": who, "what": what}})
}

fn meta_of(kernel: &Kernel, id: &str) -> Map<String, Value> {
    kernel.resolve_target(id).expect("agent").record().meta
}

#[tokio::test]
async fn forced_values_in_what_overwrite_the_creators_and_bundles() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk_v9_kernel(&tmp);
    op_create(&k, "kernel", "maker", "counting.tools", json!({
        "maker": create_line(json!(["*"]), json!({"chatlike": {"model": "forced-m", "temperature": "*"}})),
    }))
    .await;
    let r = send_as(&k, "maker", "kernel", json!({"type": "create_agent", "parent_id": "maker",
        "handler_module": "chatlike", "id": "c1", "model": "mine", "temperature": 0.9})).await;
    assert_eq!(r["id"], "c1", "{r}");
    let m = meta_of(&k, "c1");
    assert_eq!(m["model"], "forced-m", "the line beats the creator");
    assert_eq!(m["temperature"], 0.9, "a free field takes the creator's value");
    let r = send_as(&k, "maker", "kernel", json!({"type": "create_agent", "parent_id": "maker",
        "handler_module": "chatlike", "id": "c2"})).await;
    assert_eq!(r["id"], "c2", "{r}");
    assert_eq!(meta_of(&k, "c2")["model"], "forced-m", "the line beats the bundle");
}

#[tokio::test]
async fn an_ancestors_forced_value_beats_the_callers_free_one() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk_v9_kernel(&tmp);
    op_create(&k, "kernel", "target", "counting.tools", Value::Null).await;
    op_create(&k, "kernel", "lead", "counting.tools", json!({
        "target": create_line(json!(["*"]), json!({"chatlike": {"model": "lead-m", "temperature": "*"}})),
    }))
    .await;
    op_create(&k, "lead", "worker", "counting.tools", json!({
        "target": create_line(json!(["*"]), json!({"chatlike": {"model": "*", "temperature": "*"}})),
    }))
    .await;
    let r = send_as(&k, "worker", "kernel", json!({"type": "create_agent", "parent_id": "target",
        "handler_module": "chatlike", "id": "c1", "model": "worker-m"})).await;
    assert_eq!(r["id"], "c1", "{r}");
    assert_eq!(meta_of(&k, "c1")["model"], "lead-m");
}

#[tokio::test]
async fn a_field_no_line_names_is_refused_at_create_and_update() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk_v9_kernel(&tmp);
    op_create(&k, "kernel", "maker", "counting.tools", json!({
        "maker": create_line(json!(["*"]), json!({"counting.tools": {"description": "*"}})),
    }))
    .await;
    let r = send_as(&k, "maker", "kernel", json!({"type": "create_agent", "parent_id": "maker",
        "handler_module": "counting.tools", "id": "c1", "display_name": "x"})).await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(r["arg"], "display_name", "{r}");
    assert!(!k.exists("c1"));
    let r = send_as(&k, "maker", "kernel", json!({"type": "create_agent", "parent_id": "maker",
        "handler_module": "counting.tools", "id": "c1", "description": "ok"})).await;
    assert_eq!(r["id"], "c1", "{r}");
    // the same at an update: the freed field changes; the unnamed one is named
    // back to the creator, which sees its child
    let r = send_as(&k, "maker", "kernel",
        json!({"type": "update_agent", "id": "c1", "description": "again"})).await;
    assert_eq!(r["updated"], true, "{r}");
    let r = send_as(&k, "maker", "kernel",
        json!({"type": "update_agent", "id": "c1", "display_name": "x"})).await;
    assert_eq!(r["reason"], "not_allowed", "{r}");
    assert_eq!(r["arg"], "display_name", "{r}");
    assert!(meta_of(&k, "c1").get("display_name").is_none_or(|v| v != "x"), "{r}");
}

#[tokio::test]
async fn a_bundle_default_setting_an_unnamed_field_refuses_the_create() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk_v9_kernel(&tmp);
    op_create(&k, "kernel", "maker", "counting.tools", json!({
        "maker": create_line(json!(["*"]), json!({"chatlike": {"model": "*"}})),
    }))
    .await;
    // `chatlike` sets `temperature`, which no line names
    let r = send_as(&k, "maker", "kernel", json!({"type": "create_agent", "parent_id": "maker",
        "handler_module": "chatlike", "id": "c1"})).await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(r["arg"], "temperature", "{r}");
    assert!(!k.exists("c1"));
}

#[tokio::test]
async fn a_forced_field_is_changed_only_by_the_operator() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk_v9_kernel(&tmp);
    op_create(&k, "kernel", "maker", "counting.tools", json!({
        "maker": create_line(json!(["*"]), json!({"chatlike": {"model": "f", "temperature": "*", "description": "*"}})),
    }))
    .await;
    let r = send_as(&k, "maker", "kernel", json!({"type": "create_agent", "parent_id": "maker",
        "handler_module": "chatlike", "id": "c1"})).await;
    assert_eq!(r["id"], "c1", "{r}");
    let model = json!({"type": "update_agent", "id": "c1", "model": "other"});
    // neither the creator nor the agent itself: both see c1, so the field is named
    for who in ["maker", "c1"] {
        let r = send_as(&k, who, "kernel", model.clone()).await;
        assert_eq!(r["reason"], "not_allowed", "{who}: {r}");
        assert_eq!(r["arg"], "model", "{who}: {r}");
        assert_eq!(meta_of(&k, "c1")["model"], "f", "{who}");
    }
    // a free field the creator may change, and the agent its own
    let r = send_as(&k, "c1", "kernel",
        json!({"type": "update_agent", "id": "c1", "temperature": 0.1})).await;
    assert_eq!(r["updated"], true, "{r}");
    // the operator changes the forced one
    let r = crate::test_support::send_settled(&k, vec![Item::new(AgentId::from("kernel"), model)])
        .await
        .remove(0)
        .result;
    assert_eq!(r["updated"], true, "{r}");
    assert_eq!(meta_of(&k, "c1")["model"], "other");
}

#[tokio::test]
async fn a_bundle_writes_its_own_non_can_fields() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk_v9_kernel(&tmp);
    op_create(&k, "kernel", "self_w", "counting.tools", Value::Null).await;
    let r = send_as(&k, "self_w", "kernel",
        json!({"type": "update_agent", "id": "self_w", "description": "mine", "port": 1})).await;
    assert_eq!(r["updated"], true, "{r}");
    assert_eq!(meta_of(&k, "self_w")["port"], 1);
    // never its own `can`
    let r = send_as(&k, "self_w", "kernel", json!({"type": "update_agent", "id": "self_w",
        "can": {"kernel_state": {"call": ["*"]}}})).await;
    assert_eq!(r["reason"], "not_allowed", "{r}");
    assert_eq!(r["arg"], "can", "{r}");
    assert!(meta_of(&k, "self_w").get("can").is_none());
}

#[tokio::test]
async fn a_minted_id_takes_the_who_prefix() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk_v9_kernel(&tmp);
    op_create(&k, "kernel", "gb", "counting.tools", json!({
        "gb": create_line(json!(["gbr_*"]), json!({"counting.tools": {}})),
    }))
    .await;
    op_create(&k, "kernel", "any", "counting.tools", json!({
        "any": create_line(json!(["*"]), json!({"counting.tools": {}})),
    }))
    .await;
    let mint = json!({"type": "create_agent", "parent_id": "gb", "handler_module": "counting.tools"});
    let r = send_as(&k, "gb", "kernel", mint).await;
    let id = r["id"].as_str().unwrap_or_else(|| panic!("{r}"));
    assert!(id.starts_with("gbr_") && id.len() == "gbr_".len() + 6, "{id}");
    let r = send_as(&k, "any", "kernel",
        json!({"type": "create_agent", "parent_id": "any", "handler_module": "counting.tools"})).await;
    let id = r["id"].as_str().unwrap_or_else(|| panic!("{r}"));
    assert!(id.starts_with("counting_"), "{id}");
}

#[tokio::test]
async fn an_uninstalled_bundle_and_a_bundle_outside_what_answer_the_same() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk_v9_kernel(&tmp);
    op_create(&k, "kernel", "maker", "counting.tools", json!({
        "maker": create_line(json!(["*"]), json!({"counting.tools": {}})),
    }))
    .await;
    let mk = |hm: &str| json!({"type": "create_agent", "parent_id": "maker", "handler_module": hm, "id": "c1"});
    // the same refusal, naming only the bundle asked for: an agent is never told
    // which bundles are installed beyond its lines
    let uninstalled = send_as(&k, "maker", "kernel", mk("ghost.tools")).await;
    let outside = send_as(&k, "maker", "kernel", mk("chatlike")).await;
    for r in [&uninstalled, &outside] {
        assert_eq!(r["reason"], "not_allowed", "{r}");
        assert_eq!(r["arg"], "handler_module", "{r}");
    }
    let named = |r: &Value, b: &str| {
        crate::refusal::error_text(r).map(|t| t.replace(b, "<b>"))
    };
    assert_eq!(named(&uninstalled, "ghost.tools"), named(&outside, "chatlike"));
    assert!(!k.exists("c1"));
    // the operator is told the bundle is not installed
    let r = crate::test_support::send_settled(&k, vec![Item::new(AgentId::from("kernel"), mk("ghost.tools"))])
        .await
        .remove(0)
        .result;
    assert_eq!(r["arg"], "handler_module", "{r}");
}

#[tokio::test]
async fn create_reach_is_the_targets_whole_tree() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk_v9_kernel(&tmp);
    op_create(&k, "kernel", "space", "counting.tools", Value::Null).await;
    op_create(&k, "space", "garage", "counting.tools", Value::Null).await;
    op_create(&k, "kernel", "outside", "counting.tools", Value::Null).await;
    op_create(&k, "kernel", "leg", "counting.tools", json!({
        "space": create_line(json!(["*"]), json!("*")),
    }))
    .await;
    let mk = |parent: &str, id: &str| json!({"type": "create_agent", "parent_id": parent,
        "handler_module": "counting.tools", "id": id});
    assert_eq!(send_as(&k, "leg", "kernel", mk("space", "c1")).await["id"], "c1");
    assert_eq!(send_as(&k, "leg", "kernel", mk("garage", "c2")).await["id"], "c2");
    assert_eq!(
        send_as(&k, "leg", "kernel", mk("outside", "c3")).await,
        crate::refusal::unknown_agent("outside")
    );
}

#[tokio::test]
async fn rollbacks_delete_only_agents_made_in_the_same_operation() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk_v9_kernel(&tmp);
    op_create(&k, "kernel", "helper", "asker.tools", Value::Null).await;
    op_create(&k, "kernel", "kept", "counting.tools", Value::Null).await;
    op_create(&k, "kernel", "asker", "counting.tools", json!({
        "helper": {"call": ["make", "zap"]},
        "asker": create_line(json!(["*"]), json!({"counting.tools": {}})),
    }))
    .await;
    let r = send_as(&k, "asker", "helper", json!({"type": "make", "parent": "asker", "id": "made"})).await;
    assert_eq!(r["id"], "made", "{r}");
    // the rollback of what it made goes, as the asker
    let r = send_as(&k, "asker", "helper", json!({"type": "zap", "id": "made"})).await;
    assert_eq!(r["deleted"], true, "{r}");
    // anything else the asker's line does not select stays
    let r = send_as(&k, "asker", "helper", json!({"type": "zap", "id": "kept"})).await;
    assert_eq!(r["deleted"], false, "{r}");
    assert!(k.exists("kept"));
}

#[tokio::test]
async fn a_verb_acting_for_an_asker_is_checked_as_the_asker() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk_v9_kernel(&tmp);
    op_create(&k, "kernel", "helper", "asker.tools", Value::Null).await;
    // the asker's ancestor caps its line on the helper (rule 6): it holds it too
    op_create(&k, "kernel", "boss", "counting.tools", json!({
        "helper": {"call": ["make", "make_direct", "zap", "touch"]},
    }))
    .await;
    op_create(&k, "boss", "asker", "counting.tools", json!({
        "helper": {"call": ["make", "make_direct", "zap", "touch"]},
        "asker": create_line(json!(["*"]), json!({"counting.tools": {}})),
    }))
    .await;
    // the asker's create line covers its own children, through either path
    for (verb, id) in [("make", "m1"), ("make_direct", "m2")] {
        let r = send_as(&k, "asker", "helper", json!({"type": verb, "parent": "asker", "id": id})).await;
        assert_eq!(r["id"], id, "{verb}: {r}");
        assert_eq!(k.resolve_target(id).unwrap().parent_id, Some(AgentId::from("asker")));
    }
    // the helper's own position does not count: nothing under the helper (the
    // asker sees the helper, so the create is named back)
    for verb in ["make", "make_direct"] {
        let r = send_as(&k, "asker", "helper", json!({"type": verb, "parent": "helper", "id": "h1"})).await;
        assert_eq!(r["reason"], "not_allowed", "{verb}: {r}");
        assert_eq!(r["arg"], "handler_module", "{verb}: {r}");
        assert!(!k.exists("h1"), "{verb}");
    }
    // rule 5 included: not its own `can`, not itself, not an ancestor (hidden)
    let r = send_as(&k, "asker", "helper", json!({"type": "touch", "id": "asker",
        "field": "can", "value": {"kernel_state": {"call": ["*"]}}})).await;
    assert_eq!(r["reason"], "not_allowed", "{r}");
    assert_eq!(r["arg"], "can", "{r}");
    let r = send_as(&k, "asker", "helper", json!({"type": "touch", "id": "boss",
        "field": "description", "value": "x"})).await;
    assert_eq!(r, crate::refusal::unknown_agent("boss"));
    for id in ["asker", "boss"] {
        let r = send_as(&k, "asker", "helper", json!({"type": "zap", "id": id})).await;
        assert_eq!(r["deleted"], false, "{id}: {r}");
    }
}

/// A deleted id's lines stay: other agents' lines on an id reach whoever is
/// created under it next (ids may be reused).
#[tokio::test]
async fn a_deleted_ids_lines_stay_and_reach_a_reused_id() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk_v9_kernel(&tmp);
    op_create(&k, "kernel", "vault", "asker.tools", Value::Null).await;
    op_create(&k, "kernel", "reader", "counting.tools", json!({"vault": {"call": ["zap"]}})).await;
    let r = crate::test_support::send_settled(&k, vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "delete_agent", "id": "vault"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["deleted"], true, "{r}");
    assert_eq!(meta_of(&k, "reader")["can"], json!({"vault": {"call": ["zap"]}}));
    // the id comes back: the line reaches the new agent
    op_create(&k, "kernel", "vault", "asker.tools", Value::Null).await;
    assert!(k.may_call(&AgentId::from("reader"), "zap", &AgentId::from("vault")));
}

/// A `can` line outside the legal shape is refused at WRITE time, at create and at
/// update: a declared-shape miss names the field (`bad_arg`), a line the shape
/// check refuses is `invalid_meta`.
#[tokio::test]
async fn a_can_line_outside_the_legal_shape_is_refused_at_write_time() {
    let tmp = tempfile::TempDir::new().unwrap();
    let kernel = mk_rooted(&tmp);
    let r = crate::test_support::send_settled(&kernel, vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"create_agent","handler_module":"counting.tools",
                   "parent_id":"kernel","id":"hub"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(r.get("error").is_none(), "{r}");
    let update = |can: Value| {
        let kernel = Arc::clone(&kernel);
        async move {
            crate::test_support::send_settled(&kernel, vec![Item::new(
                    AgentId::from("kernel"),
                    json!({"type": "update_agent", "id": "hub", "can": can}),
                )])
                .await
                .remove(0)
                .result
        }
    };
    for can in [
        json!({"$loader": {"call": ["*"]}}),
        json!({"$subtree": {"call": ["list"]}}),
        json!({"*": {"call": ["*"]}}),
        json!({"g1|": {"call": ["*"]}}),
        json!({"g1|ai": {"call": ["*"]}}),
        json!({"$parent": {"update": ["x"]}}),
    ] {
        let r = update(can.clone()).await;
        assert_eq!(r["reason"], "invalid_meta", "{can}: {r}");
        assert!(meta_of(&kernel, "hub").get("can").is_none(), "{can}");
    }
    for can in [
        json!({"served": 7}),
        json!({"served": ["list"]}),
        json!({"hub": {"call": ["x"], "update": ["y"]}}),
        json!("*"),
    ] {
        let r = update(can.clone()).await;
        assert_eq!(r["reason"], "bad_arg", "{can}: {r}");
    }
}

// RULE 7 — the writer subset check (`crate::authz::glue`).

/// The operator's `payload` to the root.
async fn op_send(kernel: &Arc<Kernel>, payload: Value) -> Value {
    crate::test_support::send_settled(kernel, vec![Item::new(AgentId::from("kernel"), payload)])
        .await
        .remove(0)
        .result
}

/// A rooted kernel with `hub` and `svc`, a writer `w` whose create line on `hub`
/// selects `chatlike` with `what` (so `can: "*"` in it lets `w` write the `can` of
/// the `chatlike` agents under `hub`), plus `extra` lines, and one such recipient
/// `r` under `hub`.
async fn writer_kernel(tmp: &tempfile::TempDir, what: Value, extra: Value) -> Arc<Kernel> {
    let k = mk_v9_kernel(tmp);
    op_create(&k, "kernel", "hub", "counting.tools", Value::Null).await;
    op_create(&k, "kernel", "svc", "counting.tools", Value::Null).await;
    let mut can = json!({"hub": create_line(json!(["*"]), json!({"chatlike": what}))});
    if let (Some(c), Value::Object(e)) = (can.as_object_mut(), extra) {
        c.extend(e);
    }
    op_create(&k, "kernel", "w", "counting.tools", can).await;
    op_create(&k, "hub", "r", "chatlike", Value::Null).await;
    k
}

/// `update_agent` of `r`'s `can`.
fn give(can: Value) -> Value {
    json!({"type": "update_agent", "id": "r", "can": can})
}

#[tokio::test]
async fn a_writer_hands_out_only_what_it_holds() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = writer_kernel(&tmp, json!({"can": "*"}), json!({"svc": {"call": ["ping"]}})).await;
    let r = send_as(&k, "w", "kernel", give(json!({"svc": {"call": ["ping"]}}))).await;
    assert_eq!(r["updated"], true, "{r}");
    for can in [
        json!({"svc": {"call": ["ping", "pong"]}}),
        json!({"svc": {"call": ["*"]}}),
        json!({"svc": {"update": ["description"]}}),
    ] {
        let r = send_as(&k, "w", "kernel", give(can.clone())).await;
        assert_eq!(r["reason"], "not_allowed", "{can}: {r}");
        assert_eq!(r["arg"], "can", "{can}: {r}");
        assert_eq!(meta_of(&k, "r")["can"], json!({"svc": {"call": ["ping"]}}), "{can}");
    }
}

#[tokio::test]
async fn a_writer_never_hands_out_by_position() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk_v9_kernel(&tmp);
    op_create(&k, "kernel", "p", "counting.tools", json!({
        "p": create_line(json!(["*"]), json!({"chatlike": {"can": "*"}})),
    }))
    .await;
    op_create(&k, "p", "kid", "chatlike", Value::Null).await;
    // the parent calls itself by position, yet holds no written line on itself
    assert!(k.may_call(&AgentId::from("p"), "send", &AgentId::from("p")));
    let r = send_as(&k, "p", "kernel", json!({"type": "update_agent", "id": "kid",
        "can": {"$parent": {"call": ["send"]}}})).await;
    assert_eq!(r["reason"], "not_allowed", "{r}");
    assert!(meta_of(&k, "kid").get("can").is_none());
    assert!(!k.may_call(&AgentId::from("kid"), "send", &AgentId::from("p")));
}

#[tokio::test]
async fn a_lead_with_a_written_line_on_itself_gives_workers_parent_send() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk_v9_kernel(&tmp);
    op_create(&k, "kernel", "team_1", "counting.tools", Value::Null).await;
    // written by the operator: the lead's line on itself, and its create line
    op_create(&k, "team_1", "t1_lead", "counting.tools", json!({
        "team_1": create_line(json!(["t1_w*"]), json!({"counting.tools": {}})),
        "t1_lead": {"call": ["send"]},
    }))
    .await;
    let mk = |id: &str, verbs: Value| json!({"type": "create_agent", "parent_id": "t1_lead",
        "handler_module": "counting.tools", "id": id, "can": {"$parent": {"call": verbs}}});
    let r = send_as(&k, "t1_lead", "kernel", mk("t1_w1", json!(["send"]))).await;
    assert_eq!(r["id"], "t1_w1", "{r}");
    assert_eq!(meta_of(&k, "t1_w1")["can"], json!({"$parent": {"call": ["send"]}}));
    assert!(k.may_call(&AgentId::from("t1_w1"), "send", &AgentId::from("t1_lead")));
    // a verb its own line does not hold is not handed out
    let r = send_as(&k, "t1_lead", "kernel", mk("t1_w2", json!(["send", "stop"]))).await;
    assert_eq!(r["reason"], "not_allowed", "{r}");
    assert!(!k.exists("t1_w2"));
}

#[tokio::test]
async fn a_handed_out_parent_line_resolves_to_the_recipients_parent() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk_v9_kernel(&tmp);
    op_create(&k, "kernel", "hub", "counting.tools", Value::Null).await;
    op_create(&k, "hub", "p1", "counting.tools", Value::Null).await;
    op_create(&k, "hub", "p2", "counting.tools", Value::Null).await;
    op_create(&k, "p1", "r1", "chatlike", Value::Null).await;
    op_create(&k, "p2", "r2", "chatlike", Value::Null).await;
    op_create(&k, "kernel", "w", "counting.tools", json!({
        "hub": create_line(json!(["*"]), json!({"chatlike": {"can": "*"}})),
        "p1": {"call": ["send"]},
    }))
    .await;
    let parent_send = json!({"$parent": {"call": ["send"]}});
    let r = send_as(&k, "w", "kernel",
        json!({"type": "update_agent", "id": "r1", "can": parent_send.clone()})).await;
    assert_eq!(r["updated"], true, "{r}");
    assert!(k.may_call(&AgentId::from("r1"), "send", &AgentId::from("p1")));
    // the same line on r2 names p2, which the writer holds no `send` on
    let r = send_as(&k, "w", "kernel",
        json!({"type": "update_agent", "id": "r2", "can": parent_send})).await;
    assert_eq!(r["reason"], "not_allowed", "{r}");
    assert!(meta_of(&k, "r2").get("can").is_none());
}

/// Review of #761, H1: `update` includes calling every verb, so an `update` line
/// (an empty field list included) is handed out only by a writer holding every
/// verb on the target in writing.
#[tokio::test]
async fn an_update_line_needs_every_verb_held_even_with_no_fields() {
    let tmp = tempfile::TempDir::new().unwrap();
    // no line on svc: the unknown answer
    let k = writer_kernel(&tmp, json!({"can": "*"}), Value::Null).await;
    for can in [json!({"svc": {"update": []}}), json!({"svc": {"update": ["description"]}})] {
        let r = send_as(&k, "w", "kernel", give(can.clone())).await;
        assert_eq!(r, crate::refusal::unknown_agent("svc"), "{can}");
        assert!(meta_of(&k, "r").get("can").is_none(), "{can}");
        assert!(!k.may_call(&AgentId::from("r"), "zap", &AgentId::from("svc")));
    }
    // some verbs: not every verb (the line also frees the fields `chatlike` sets)
    let tmp = tempfile::TempDir::new().unwrap();
    let k = writer_kernel(&tmp, json!({"can": "*", "model": "*", "temperature": "*"}),
        json!({"svc": {"call": ["ping"]}})).await;
    let r = send_as(&k, "w", "kernel", give(json!({"svc": {"update": []}}))).await;
    assert_eq!(r["reason"], "not_allowed", "{r}");
    assert!(meta_of(&k, "r").get("can").is_none());
    // the same through a create's `can`
    let r = send_as(&k, "w", "kernel", json!({"type": "create_agent", "parent_id": "hub",
        "handler_module": "chatlike", "id": "r2", "can": {"svc": {"update": []}}})).await;
    assert_eq!(r["reason"], "not_allowed", "{r}");
    assert!(!k.exists("r2"));
    // every verb held: the empty update goes, a field it does not hold does not
    let tmp = tempfile::TempDir::new().unwrap();
    let k = writer_kernel(&tmp, json!({"can": "*"}), json!({"svc": {"call": ["*"]}})).await;
    let r = send_as(&k, "w", "kernel", give(json!({"svc": {"update": []}}))).await;
    assert_eq!(r["updated"], true, "{r}");
    let r = send_as(&k, "w", "kernel", give(json!({"svc": {"update": ["description"]}}))).await;
    assert_eq!(r["reason"], "not_allowed", "{r}");
    assert_eq!(meta_of(&k, "r")["can"], json!({"svc": {"update": []}}));
}

/// Review of #761, H2: a `$parent` line holds one node, so it never justifies a
/// line on that node's id, which reaches its whole tree.
#[tokio::test]
async fn a_parent_line_never_justifies_a_line_on_the_parents_id() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk_v9_kernel(&tmp);
    let chats = create_line(json!(["*"]), json!({"chatlike": {"can": "*"}}));
    op_create(&k, "kernel", "hub", "counting.tools", Value::Null).await;
    op_create(&k, "kernel", "p", "counting.tools", json!({"hub": chats.clone()})).await;
    op_create(&k, "p", "pk", "counting.tools", Value::Null).await;
    op_create(&k, "p", "w", "counting.tools", json!({
        "hub": chats,
        "$parent": {"call": ["send"]},
    }))
    .await;
    op_create(&k, "hub", "r", "chatlike", Value::Null).await;
    assert!(k.may_call(&AgentId::from("w"), "send", &AgentId::from("p")));
    assert!(!k.may_call(&AgentId::from("w"), "send", &AgentId::from("pk")));
    for can in [
        json!({"p": {"call": ["send"]}}),
        json!({"p": {"call": ["*"]}}),
        json!({"p": {"update": []}}),
    ] {
        let r = send_as(&k, "w", "kernel", give(can.clone())).await;
        assert_eq!(r["reason"], "not_allowed", "{can}: {r}");
        assert!(meta_of(&k, "r").get("can").is_none(), "{can}");
    }
    assert!(!k.may_call(&AgentId::from("r"), "send", &AgentId::from("pk")));
    // a written line on p's tree holds it
    let w = AgentId::from("w");
    let added = k.edit_can(&w, None, |c, _| {
        c.insert("p".to_string(), json!({"call": ["send"]}));
        Ok(true)
    });
    assert_eq!(added, Ok(true));
    let r = send_as(&k, "w", "kernel", give(json!({"p": {"call": ["send"]}}))).await;
    assert_eq!(r["updated"], true, "{r}");
}

/// Review of #761, H2: a create line that selects some ids or some bundles holds
/// the agents it selects, one by one, so it never justifies a `call`/`update` line
/// on one of them, which reaches that agent's whole tree. A create line with
/// `who: *` on every bundle holds the whole tree below its target.
#[tokio::test]
async fn a_selecting_create_line_never_justifies_a_line_on_a_selected_agent() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk_v9_kernel(&tmp);
    op_create(&k, "kernel", "hub", "counting.tools", Value::Null).await;
    op_create(&k, "hub", "c_1", "chatlike", Value::Null).await;
    // under the selected c_1: an id and a bundle the line does not select
    op_create(&k, "c_1", "x_9", "counting.tools", Value::Null).await;
    op_create(&k, "hub", "c_r", "chatlike", Value::Null).await;
    op_create(&k, "kernel", "w", "counting.tools", json!({
        "hub": create_line(json!(["c_*"]), json!({"chatlike": {"can": "*", "model": "*"}})),
    }))
    .await;
    let give_cr = |can: Value| json!({"type": "update_agent", "id": "c_r", "can": can});
    assert!(k.may_call(&AgentId::from("w"), "ping", &AgentId::from("c_1")));
    assert!(!k.may_call(&AgentId::from("w"), "ping", &AgentId::from("x_9")));
    for can in [
        json!({"c_1": {"call": ["ping"]}}),
        json!({"c_1": {"call": ["*"]}}),
        json!({"c_1": {"update": ["model"]}}),
    ] {
        let r = send_as(&k, "w", "kernel", give_cr(can.clone())).await;
        assert_eq!(r["reason"], "not_allowed", "{can}: {r}");
        assert!(meta_of(&k, "c_r").get("can").is_none(), "{can}");
    }
    assert!(!k.may_call(&AgentId::from("c_r"), "ping", &AgentId::from("x_9")));
    // the same with `who: *` on some bundles
    let w = AgentId::from("w");
    let set = |line: Value| {
        k.edit_can(&w, None, move |c, _| {
            c.insert("hub".to_string(), line);
            Ok(true)
        })
    };
    assert_eq!(
        set(create_line(json!(["*"]), json!({"chatlike": {"can": "*", "model": "*"}}))),
        Ok(true)
    );
    let r = send_as(&k, "w", "kernel", give_cr(json!({"c_1": {"call": ["ping"]}}))).await;
    assert_eq!(r["reason"], "not_allowed", "{r}");
    // `who: *` on every bundle: the whole tree below hub is held
    assert_eq!(set(create_line(json!(["*"]), json!({"*": {"can": "*", "model": "*"}}))), Ok(true));
    let r = send_as(&k, "w", "kernel", give_cr(json!({"c_1": {"call": ["ping"]}}))).await;
    assert_eq!(r["updated"], true, "{r}");
    let r = send_as(&k, "w", "kernel", give_cr(json!({"c_1": {"update": ["model"]}}))).await;
    assert_eq!(r["updated"], true, "{r}");
}

#[tokio::test]
async fn a_handed_out_create_line_keeps_every_forced_value() {
    let tmp = tempfile::TempDir::new().unwrap();
    let what = json!({"can": "*", "model": "forced-m", "temperature": "*"});
    let k = writer_kernel(&tmp, what, Value::Null).await;
    let line = |model: Value| json!({"hub": create_line(json!(["*"]),
        json!({"chatlike": {"model": model, "temperature": "*"}}))});
    let r = send_as(&k, "w", "kernel", give(line(json!("forced-m")))).await;
    assert_eq!(r["updated"], true, "{r}");
    for model in [json!("other"), json!("*")] {
        let r = send_as(&k, "w", "kernel", give(line(model.clone()))).await;
        assert_eq!(r["reason"], "not_allowed", "{model}: {r}");
        assert_eq!(meta_of(&k, "r")["can"], line(json!("forced-m")), "{model}");
    }
}

#[tokio::test]
async fn a_free_field_on_another_target_does_not_cover_a_forced_one() {
    let tmp = tempfile::TempDir::new().unwrap();
    let what = json!({"can": "*", "model": "forced-m", "temperature": "*"});
    // a line on `svc` (not above `hub`) frees `model`
    let extra = json!({"svc": create_line(json!(["*"]),
        json!({"chatlike": {"model": "*", "temperature": "*"}}))});
    let k = writer_kernel(&tmp, what, extra).await;
    for model in [json!("*"), json!("mine")] {
        let r = send_as(&k, "w", "kernel", give(json!({"hub": create_line(json!(["*"]),
            json!({"chatlike": {"model": model, "temperature": "*"}}))}))).await;
        assert_eq!(r["reason"], "not_allowed", "{model}: {r}");
        assert!(meta_of(&k, "r").get("can").is_none(), "{model}");
    }
}

#[tokio::test]
async fn a_handed_out_create_line_that_leaves_out_a_forced_field_is_refused() {
    let tmp = tempfile::TempDir::new().unwrap();
    let what = json!({"can": "*", "model": "forced-m", "temperature": "*"});
    let k = writer_kernel(&tmp, what, Value::Null).await;
    for fields in [json!({"temperature": "*"}), json!({})] {
        let r = send_as(&k, "w", "kernel", give(json!({"hub": create_line(json!(["*"]),
            json!({"chatlike": fields}))}))).await;
        assert_eq!(r["reason"], "not_allowed", "{fields}: {r}");
        assert!(meta_of(&k, "r").get("can").is_none(), "{fields}");
    }
}

#[tokio::test]
async fn a_handed_out_create_line_may_force_a_free_field() {
    let tmp = tempfile::TempDir::new().unwrap();
    let what = json!({"can": "*", "model": "*", "temperature": "*"});
    let k = writer_kernel(&tmp, what, Value::Null).await;
    for fields in [json!({"model": "narrow-m"}), json!({"model": "*", "temperature": 0.1})] {
        let can = json!({"hub": create_line(json!(["*"]), json!({"chatlike": fields}))});
        let r = send_as(&k, "w", "kernel", give(can.clone())).await;
        assert_eq!(r["updated"], true, "{fields}: {r}");
        assert_eq!(meta_of(&k, "r")["can"], can);
    }
}

#[tokio::test]
async fn a_field_the_ancestor_cap_removes_cannot_be_handed_out() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk_v9_kernel(&tmp);
    op_create(&k, "kernel", "hub", "counting.tools", Value::Null).await;
    // the boss's line names no `temperature`; the writer's below it does
    op_create(&k, "kernel", "boss", "counting.tools", json!({
        "hub": create_line(json!(["*"]), json!({"chatlike": {"can": "*", "model": "*"}})),
    }))
    .await;
    op_create(&k, "boss", "w", "counting.tools", json!({
        "hub": create_line(json!(["*"]),
            json!({"chatlike": {"can": "*", "model": "*", "temperature": "*"}})),
    }))
    .await;
    op_create(&k, "hub", "r", "chatlike", Value::Null).await;
    let r = send_as(&k, "w", "kernel", give(json!({"hub": create_line(json!(["*"]),
        json!({"chatlike": {"temperature": "*"}}))}))).await;
    assert_eq!(r["reason"], "not_allowed", "{r}");
    assert!(meta_of(&k, "r").get("can").is_none());
    // a field both hold goes
    let r = send_as(&k, "w", "kernel", give(json!({"hub": create_line(json!(["*"]),
        json!({"chatlike": {"model": "*"}}))}))).await;
    assert_eq!(r["updated"], true, "{r}");
}

#[tokio::test]
async fn a_forced_can_in_what_is_checked_as_grant_lines_once_when_written() {
    let tmp = tempfile::TempDir::new().unwrap();
    let what = json!({"can": "*", "model": "*", "temperature": "*"});
    let k = writer_kernel(&tmp, what, json!({"svc": {"call": ["get"]}})).await;
    op_create(&k, "kernel", "vault", "counting.tools", Value::Null).await;
    let line = |can: Value| json!({"hub": create_line(json!(["*"]),
        json!({"chatlike": {"can": can, "model": "*", "temperature": "*"}}))});
    // `can: "*"` in the writer's own line holds no `can` value: each forced line
    // is checked as a line the writer hands out
    let r = send_as(&k, "w", "kernel", give(line(json!({"svc": {"call": ["get", "set"]}})))).await;
    assert_eq!(r["reason"], "not_allowed", "{r}");
    let r = send_as(&k, "w", "kernel", give(line(json!({"vault": {"call": ["x"]}})))).await;
    assert_eq!(r, crate::refusal::unknown_agent("vault"));
    assert!(meta_of(&k, "r").get("can").is_none());
    // (written last: from here r's own line forces `can` on the chatlike agents
    // under hub, r included, so only the operator changes r's `can`)
    let held = line(json!({"svc": {"call": ["get"]}}));
    let r = send_as(&k, "w", "kernel", give(held.clone())).await;
    assert_eq!(r["updated"], true, "{r}");
    assert_eq!(meta_of(&k, "r")["can"], held);
    // checked once: the writer losing its line does not undo the written one, and a
    // create through it is not checked again
    let w = AgentId::from("w");
    assert_eq!(k.edit_can(&w, None, |c, _| Ok(c.remove("svc").is_some())), Ok(true));
    let r = send_as(&k, "r", "kernel", json!({"type": "create_agent", "parent_id": "hub",
        "handler_module": "chatlike", "id": "c1"})).await;
    assert_eq!(r["id"], "c1", "{r}");
    assert_eq!(meta_of(&k, "c1")["can"], json!({"svc": {"call": ["get"]}}));
}

/// H2 (review of #761): a `$parent` line inside a forced `can` resolves to whichever
/// node of the create target's tree the child is made under, so the writer must hold
/// it tree-wide on that target; its own `$parent` line names one node only.
#[tokio::test]
async fn a_forced_parent_line_is_held_tree_wide_on_the_create_target() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk_v9_kernel(&tmp);
    let free = || create_line(json!(["*"]), json!({"chatlike": {"can": "*", "model": "*", "temperature": "*"}}));
    op_create(&k, "kernel", "pp", "counting.tools", Value::Null).await;
    op_create(&k, "pp", "q", "counting.tools", Value::Null).await;
    op_create(&k, "q", "deep", "counting.tools", Value::Null).await;
    // w: `$parent` (pp, one node) and a create line on pp
    op_create(&k, "pp", "w", "counting.tools", json!({
        "$parent": {"call": ["send"]}, "pp": free(),
    }))
    .await;
    // w2: a literal call line on pp (its whole tree) and a create line on q
    op_create(&k, "pp", "w2", "counting.tools", json!({
        "pp": {"call": ["send"]}, "q": free(),
    }))
    .await;
    // r: selected by both create lines, so both hold `update can` on it
    op_create(&k, "q", "r", "chatlike", Value::Null).await;
    let forcing = |target: &str| json!({target: create_line(json!(["*"]),
        json!({"chatlike": {"can": {"$parent": {"call": ["send"]}}, "model": "*", "temperature": "*"}}))});
    // a child made under any node of pp's tree would get `send` there: w holds it on pp alone
    let r = send_as(&k, "w", "kernel", give(forcing("pp"))).await;
    assert!(r.get("error").is_some(), "{r}");
    assert!(meta_of(&k, "r").get("can").is_none());
    // w2 holds `send` on pp's whole tree, so on q's
    let r = send_as(&k, "w2", "kernel", give(forcing("q"))).await;
    assert_eq!(r["updated"], true, "{r}");
    // and the child r makes under a deeper node gets `send` on that node, which w2 holds
    let r2 = send_as(&k, "r", "kernel", json!({"type": "create_agent", "parent_id": "deep",
        "handler_module": "chatlike", "id": "c_deep"})).await;
    assert_eq!(r2["id"], "c_deep", "{r2}");
    assert_eq!(meta_of(&k, "c_deep")["can"], json!({"$parent": {"call": ["send"]}}));
}

#[tokio::test]
async fn a_line_on_a_missing_id_is_refused_for_an_agent_writer() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = writer_kernel(&tmp, json!({"can": "*"}), Value::Null).await;
    let ghost = json!({"ghost": {"call": ["x"]}});
    let r = send_as(&k, "w", "kernel", give(ghost.clone())).await;
    assert_eq!(r, crate::refusal::unknown_agent("ghost"));
    assert!(meta_of(&k, "r").get("can").is_none());
    // the operator writes it
    let r = op_send(&k, give(ghost.clone())).await;
    assert_eq!(r["updated"], true, "{r}");
    assert_eq!(meta_of(&k, "r")["can"], ghost);
}

#[tokio::test]
async fn narrowing_a_can_is_always_allowed() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = writer_kernel(&tmp, json!({"can": "*"}), Value::Null).await;
    // an operator-written line the writer does not hold
    let r = op_send(&k, give(json!({"svc": {"call": ["get", "set"]}}))).await;
    assert_eq!(r["updated"], true, "{r}");
    let r = send_as(&k, "w", "kernel", give(json!({"svc": {"call": ["get"]}}))).await;
    assert_eq!(r["updated"], true, "{r}");
    assert_eq!(meta_of(&k, "r")["can"], json!({"svc": {"call": ["get"]}}));
    // widening it again is checked
    let r = send_as(&k, "w", "kernel", give(json!({"svc": {"call": ["get", "set"]}}))).await;
    assert_eq!(r, crate::refusal::unknown_agent("svc"));
    // and dropping it is narrowing too
    let r = send_as(&k, "w", "kernel", give(json!({}))).await;
    assert_eq!(r["updated"], true, "{r}");
}

/// A forced `can` is checked when its line is written (Q23): rewriting a stored line
/// that frees every field into one that forces a `can` is not narrowing, so the forced
/// lines are checked as the writer's.
#[tokio::test]
async fn forcing_a_can_into_a_stored_line_is_checked_as_a_hand_out() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = writer_kernel(&tmp, json!({"can": "*"}), Value::Null).await;
    let wide = json!({"hub": {"create": {"who": ["*"], "what": "*"}}});
    let r = op_send(&k, give(wide.clone())).await;
    assert_eq!(r["updated"], true, "{r}");
    let forced = json!({"hub": {"create": {"who": ["*"],
        "what": {"chatlike": {"can": {"svc": {"call": ["get"]}}}}}}});
    let r = send_as(&k, "w", "kernel", give(forced)).await;
    assert_eq!(r, crate::refusal::unknown_agent("svc"), "w holds nothing on svc");
    assert_eq!(meta_of(&k, "r")["can"], wide);
}

#[tokio::test]
async fn a_refused_write_on_an_unheld_id_is_the_unknown_answer() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = writer_kernel(&tmp, json!({"can": "*"}), Value::Null).await;
    op_create(&k, "kernel", "w2", "counting.tools", Value::Null).await;
    // a line on an existing id the writer holds nothing on answers as a missing id
    let r = send_as(&k, "w", "kernel", give(json!({"svc": {"call": ["x"]}}))).await;
    assert_eq!(r, crate::refusal::unknown_agent("svc"));
    assert!(r.get("side").is_none() && r.get("agents").is_none(), "{r}");
    // a recipient the writer holds nothing on: the unknown answer for it
    let r = send_as(&k, "w2", "kernel", give(json!({"hub": {"call": ["x"]}}))).await;
    assert_eq!(r, crate::refusal::unknown_agent("r"));
    // the same through `edit_can`
    let text = |id: &str| crate::refusal::error_text(&crate::refusal::unknown_agent(id));
    let r = AgentId::from("r");
    let add = |t: &'static str| {
        move |c: &mut Map<String, Value>, _: &Map<String, Value>| -> Result<bool, String> {
            c.insert(t.to_string(), json!({"call": ["x"]}));
            Ok(true)
        }
    };
    assert_eq!(k.edit_can(&r, Some(&AgentId::from("w2")), add("hub")).err(), text("r"));
    assert_eq!(k.edit_can(&r, Some(&AgentId::from("w")), add("svc")).err(), text("svc"));
    assert_eq!(k.edit_can(&r, Some(&AgentId::from("w")), add("ghost")).err(), text("ghost"));
    assert!(meta_of(&k, "r").get("can").is_none());
}

#[tokio::test]
async fn the_operator_writes_any_first_lines() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk_v9_kernel(&tmp);
    let first = json!({
        "ghost": {"call": ["*"]},
        "kernel_state": create_line(json!(["*"]), json!("*")),
    });
    op_create(&k, "kernel", "a", "counting.tools", first.clone()).await;
    assert_eq!(meta_of(&k, "a")["can"], first);
    let r = op_send(&k, json!({"type": "update_agent", "id": "a",
        "can": {"ghost2": {"update": ["*"]}}})).await;
    assert_eq!(r["updated"], true, "{r}");
    let added = k.edit_can(&AgentId::from("a"), None, |c, _| {
        c.insert("ghost3".to_string(), json!({"call": ["x"]}));
        Ok(true)
    });
    assert_eq!(added, Ok(true));
    assert_eq!(
        meta_of(&k, "a")["can"],
        json!({"ghost2": {"update": ["*"]}, "ghost3": {"call": ["x"]}})
    );
}

#[tokio::test]
async fn bundle_defaults_can_lines_are_checked_as_the_creators_write() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk_v9_kernel(&tmp);
    k.bundles
        .register_meta_yaml(
            "keyed",
            "base: counting.tools\nsentence: A keyed stand-in.\nrecord:\n  can:\n    svc:\n      call:\n        - get\n",
        )
        .expect("meta bundle");
    op_create(&k, "kernel", "svc", "counting.tools", Value::Null).await;
    op_create(&k, "kernel", "maker", "counting.tools", json!({
        "maker": create_line(json!(["*"]), json!({"keyed": {}})),
    }))
    .await;
    op_create(&k, "kernel", "keeper", "counting.tools", json!({
        "keeper": create_line(json!(["*"]), json!({"keyed": {}})),
        "svc": {"call": ["get"]},
    }))
    .await;
    let mk = |parent: &str, id: &str| json!({"type": "create_agent", "parent_id": parent,
        "handler_module": "keyed", "id": id});
    // the bundle's default line is the creator's write: the maker holds no `get`
    let r = send_as(&k, "maker", "kernel", mk("maker", "k1")).await;
    assert_eq!(r, crate::refusal::unknown_agent("svc"));
    assert!(!k.exists("k1"));
    let r = send_as(&k, "keeper", "kernel", mk("keeper", "k2")).await;
    assert_eq!(r["id"], "k2", "{r}");
    assert_eq!(meta_of(&k, "k2")["can"], json!({"svc": {"call": ["get"]}}));
}

/// M2/M3: a create sent for "whoever asked" ([`Kernel::call_for`], test side
/// [`crate::test_support::settled_for_asker`]) from work with NO ORIGIN SCOPE (a bare `tokio::spawn`) is
/// nobody's: the unknown answer, and nothing is created. In the in-process
/// shell's scope it is the operator's.
#[tokio::test]
async fn an_asker_create_with_no_origin_scope_creates_nothing() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = mk_v9_kernel(&tmp);
    let create = || {
        Item::new(
            AgentId::from("kernel"),
            json!({"type": "create_agent", "parent_id": "kernel_state",
                   "handler_module": "counting.tools", "id": "orphan"}),
        )
    };
    let kk = Arc::clone(&k);
    let item = create();
    let r = tokio::spawn(async move { crate::test_support::settled_for_asker(&kk, vec![item]).await.remove(0).result })
        .await
        .unwrap();
    assert_eq!(r, crate::refusal::unknown_agent("kernel"));
    assert!(!k.exists("orphan"));
    let r = crate::send::as_operator(crate::test_support::settled_for_asker(&k, vec![create()]))
        .await
        .remove(0)
        .result;
    assert_eq!(r["id"], "orphan", "{r}");
}

// ─── rule 7 / Q39: a verb that creates for its asker is ALSO checked as the asker ───

/// `kernel.check_asker_create(parent, record)` inside a dispatch of `me` asked for by
/// `asker` (`None`: the operator).
async fn asker_check(
    kernel: &Arc<Kernel>,
    asker: Option<&str>,
    me: &str,
    parent: &str,
    record: Value,
) -> Result<AskerCreate, Value> {
    let rec = record.as_object().cloned().unwrap_or_default();
    let k = Arc::clone(kernel);
    let p = AgentId::from(parent);
    crate::send::CURRENT_ORIGIN
        .scope(
            asker.map(AgentId::from),
            crate::send::as_agent(&AgentId::from(me), async move { k.check_asker_create(&p, &rec) }),
        )
        .await
}

#[tokio::test]
async fn the_asker_check_holds_the_asker_to_its_own_lines_beside_the_creators() {
    let tmp = tempfile::TempDir::new().unwrap();
    let kernel = mk_rooted(&tmp);
    for id in ["canvas", "open", "forcing", "asker", "narrow", "insists", "keys"] {
        let r = crate::test_support::send_settled(&kernel, vec![Item::new(
                AgentId::from("kernel"),
                json!({"type": "create_agent", "parent_id": "kernel", "handler_module": "counting.tools", "id": id}),
            )])
            .await
            .remove(0)
            .result;
        assert!(r.get("error").is_none(), "{r}");
    }
    let g = |holder: &str, target: &str, level: Value| crate::test_support::grant(&kernel, holder, target, level);
    // the creators: `canvas` forces a `can`, `open` frees `can`, `forcing` forces `note`
    g("canvas", "canvas", json!({"create": {"who": ["*"], "what": {"counting.tools": {"note": "*", "can": {"keys": {"call": ["get"]}}}}}}));
    g("canvas", "keys", json!({"call": ["get"]}));
    g("open", "open", json!({"create": {"who": ["*"], "what": {"counting.tools": {"note": "*", "can": "*"}}}}));
    g("open", "keys", json!({"call": ["get"]}));
    g("forcing", "forcing", json!({"create": {"who": ["*"], "what": {"counting.tools": {"note": "fixed"}}}}));
    // the askers: `asker` may create anything there; `narrow` only `n_*` with no field;
    // `insists` forces another `note`
    for t in ["canvas", "open", "forcing"] {
        g("asker", t, json!({"create": {"who": ["*"], "what": "*"}}));
        g("narrow", t, json!({"create": {"who": ["n_*"], "what": {"counting.tools": {}}}}));
        g("insists", t, json!({"create": {"who": ["*"], "what": {"counting.tools": {"note": "mine"}}}}));
    }
    let rec = |v: Value| {
        let mut m = json!({"handler_module": "counting.tools"});
        m.as_object_mut().unwrap().extend(v.as_object().cloned().unwrap_or_default());
        m
    };

    // both allow it: the id as asked, nothing forced by the asker
    let ok = asker_check(&kernel, Some("asker"), "canvas", "canvas", rec(json!({"id": "x1", "note": "hi"}))).await.expect("allowed");
    assert_eq!(ok, AskerCreate { id: "x1".into(), forced: Map::new() });

    // a `can` line the asker supplies must be held by the asker, though the creator holds it
    let r = asker_check(&kernel, Some("asker"), "open", "open", rec(json!({"id": "x2", "can": {"keys": {"call": ["get"]}}}))).await;
    assert!(r.is_err(), "the asker holds nothing on keys: {r:?}");
    // a `can` the creator's line forces is the line's: the asker's is not checked
    let r = asker_check(&kernel, Some("asker"), "canvas", "canvas", rec(json!({"id": "x3", "can": {"keys": {"call": ["get"]}}}))).await;
    assert!(r.is_ok(), "{r:?}");

    // the asker's `who`: an id outside it is named back (the asker sees canvas, and
    // its line makes that bundle under other ids); no id mints from it
    let r = asker_check(&kernel, Some("narrow"), "canvas", "canvas", rec(json!({"id": "x4"}))).await.expect_err("outside who");
    assert_eq!(r["reason"], "not_allowed", "{r}");
    assert_eq!(r["arg"], "id", "{r}");
    let minted = asker_check(&kernel, Some("narrow"), "canvas", "canvas", rec(json!({}))).await.expect("minted");
    assert!(minted.id.starts_with("n_"), "{minted:?}");
    // a field the asker's line does not name is refused, though the creator frees it
    let r = asker_check(&kernel, Some("narrow"), "canvas", "canvas", rec(json!({"id": "n_1", "note": "x"}))).await;
    assert!(r.is_err(), "{r:?}");
    // ...unless the creator's line forces it: the line's value, handed out by its writer
    let r = asker_check(&kernel, Some("narrow"), "forcing", "forcing", rec(json!({"id": "n_2", "note": "x"}))).await;
    assert!(r.is_ok(), "{r:?}");
    // the asker's forced values come back, for the create to carry
    let forced = asker_check(&kernel, Some("insists"), "open", "open", rec(json!({"id": "x5"}))).await.expect("forced");
    assert_eq!(forced.forced.get("note"), Some(&json!("mine")));
    // two lines that force different values: no child satisfies both
    let r = asker_check(&kernel, Some("insists"), "forcing", "forcing", rec(json!({"id": "x6"}))).await;
    assert!(r.is_err(), "{r:?}");

    // the operator asking passes; no origin scope at all is refused
    let op = asker_check(&kernel, None, "canvas", "canvas", rec(json!({"id": "x7", "can": {"keys": {"call": ["get"]}}}))).await;
    assert!(op.is_ok(), "{op:?}");
    let bare = kernel.check_asker_create(&AgentId::from("canvas"), rec(json!({"id": "x8"})).as_object().unwrap());
    assert_eq!(bare.expect_err("no scope"), crate::refusal::unknown_agent("canvas"));
    // nothing was created by a check
    for id in ["x1", "x2", "x3", "n_1", "n_2", "x5", "x7"] {
        assert!(!kernel.exists(id), "{id}");
    }
}

/// #783: a caller that sees the target is told what its lines lack, by the one
/// `not_allowed` shape naming the verb, field or create; a caller that sees
/// nothing still gets the unknown answer, with no id beyond the one it named.
async fn seen_kernel(tmp: &tempfile::TempDir) -> Arc<Kernel> {
    let k = mk_v9_kernel(tmp);
    op_create(&k, "kernel", "hub", "counting.tools", Value::Null).await;
    op_create(&k, "kernel", "w", "counting.tools", json!({
        "hub": create_line(json!(["*"]), json!({"asker.tools": {}})),
    }))
    .await;
    op_create(&k, "kernel", "stranger", "counting.tools", Value::Null).await;
    let r = send_as(&k, "w", "kernel", json!({"type": "create_agent", "parent_id": "hub",
        "handler_module": "asker.tools", "id": "c1"})).await;
    assert_eq!(r["id"], "c1", "{r}");
    // the creator's lines are then narrowed: one verb on its child, and a create
    // line that no longer selects it
    let r = op_send(&k, json!({"type": "update_agent", "id": "w", "can": {
        "c1": {"call": ["make"]},
        "hub": create_line(json!(["*"]), json!({"counting.tools": {}})),
    }})).await;
    assert_eq!(r["updated"], true, "{r}");
    k
}

#[tokio::test]
async fn a_creator_calling_a_verb_its_line_lacks_on_its_child_is_told_the_verb() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = seen_kernel(&tmp).await;
    let r = send_as(&k, "w", "c1", json!({"type": "zap", "id": "x"})).await;
    assert_eq!(r["reason"], "not_allowed", "{r}");
    assert_eq!(r["arg"], "zap", "{r}");
    assert_eq!(r["error"], "'w' holds no call 'zap' on 'c1'", "{r}");
    assert!(r["hint"].as_str().is_some_and(|h| h.contains("wider line")), "{r}");
    // a verb c1 does not answer at all keeps the dispatch's named answer, listing
    // only the verbs w may call there
    let r = send_as(&k, "w", "c1", json!({"type": "nope"})).await;
    assert_eq!(r["reason"], "unhandled", "{r}");
    assert_eq!(r["verb"], "nope", "{r}");
    let text = crate::refusal::error_text(&r).unwrap_or_default();
    assert!(text.contains("make"), "{r}");
    for hidden in ["zap", "touch", "make_direct", "reflect"] {
        assert!(!text.contains(hidden), "{hidden} leaked: {r}");
    }
    // a kernel verb sent to c1 is answered where it lives
    let r = send_as(&k, "w", "c1", json!({"type": "create_agent", "parent_id": "c1",
        "handler_module": "asker.tools"})).await;
    assert_eq!(r["reason"], "not_here", "{r}");
}

#[tokio::test]
async fn an_update_of_a_structure_field_names_the_field() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = seen_kernel(&tmp).await;
    for (who, field, value) in [
        ("w", "handler_module", json!("counting.tools")),
        ("c1", "handler_module", json!("counting.tools")),
        ("w", "parent", json!("kernel")),
    ] {
        let r = send_as(&k, who, "kernel",
            json!({"type": "update_agent", "id": "c1", field: value})).await;
        assert_eq!(r["reason"], "not_allowed", "{who} {field}: {r}");
        assert_eq!(r["arg"], field, "{who} {field}: {r}");
        assert_eq!(
            r["error"],
            format!("update_agent: '{field}' can never be updated"),
            "{who} {field}: {r}"
        );
    }
    assert_eq!(k.resolve_target("c1").unwrap().handler_module.as_deref(), Some("asker.tools"));
}

#[tokio::test]
async fn a_stranger_still_gets_the_unknown_answer_with_no_id_leak() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = seen_kernel(&tmp).await;
    let unknown = crate::refusal::unknown_agent("c1");
    for payload in [
        json!({"type": "zap", "id": "x"}),
        json!({"type": "nope"}),
    ] {
        assert_eq!(send_as(&k, "stranger", "c1", payload.clone()).await, unknown, "{payload}");
    }
    for field in ["handler_module", "description", "can"] {
        let r = send_as(&k, "stranger", "kernel",
            json!({"type": "update_agent", "id": "c1", field: "x"})).await;
        assert_eq!(r, unknown, "{field}");
    }
    let r = send_as(&k, "stranger", "kernel", json!({"type": "create_agent", "parent_id": "hub",
        "handler_module": "asker.tools", "id": "c9"})).await;
    assert_eq!(r, crate::refusal::unknown_agent("hub"));
    // and the creator, past its line, is named the part it lacks
    let r = send_as(&k, "w", "kernel", json!({"type": "create_agent", "parent_id": "hub",
        "handler_module": "asker.tools", "id": "c9"})).await;
    assert_eq!(r["reason"], "not_allowed", "{r}");
    assert_eq!(r["arg"], "handler_module", "{r}");
    assert!(!k.exists("c9"));
}

#[tokio::test]
async fn a_delete_past_the_line_is_named_to_a_caller_that_sees_the_target() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = seen_kernel(&tmp).await;
    let del = json!({"type": "delete_agent", "id": "c1"});
    // the creator sees c1 (its `make` line) but no create line selects it now
    for who in ["w", "c1"] {
        let r = send_as(&k, who, "kernel", del.clone()).await;
        assert_eq!(r["reason"], "not_allowed", "{who}: {r}");
        assert_eq!(r["arg"], "delete", "{who}: {r}");
        assert_eq!(r["deleted"], false, "{who}: {r}");
    }
    // a stranger: the unknown answer, as for a missing id
    let r = send_as(&k, "stranger", "kernel", del).await;
    let mut unknown = crate::refusal::unknown_agent("c1");
    crate::refusal::with(&mut unknown, "deleted", json!(false));
    assert_eq!(r, unknown);
    assert!(k.exists("c1"));
}
