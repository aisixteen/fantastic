//! Unit tests: every one is driven through `kernel.send`, as production is. A
//! `caller.tools` agent stands for any agent that makes calls (a lead, a worker).

use super::*;
use fantastic_kernel::Agent;
use std::path::Path;
use tempfile::TempDir;

/// An agent that makes the call it is told to (`call {target, payload}`) AS ITSELF,
/// so the gate and `spawn`'s caller see a real agent. Every other verb answers ok.
/// Every `send` a caller agent received: (agent, text).
static SENT: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

/// Every `pause` a caller agent received: the agent id.
static PAUSED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

struct CallerBundle;

#[async_trait]
impl Bundle for CallerBundle {
    fn name(&self) -> &str {
        "caller"
    }
    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        match verb {
            "boot" | "shutdown" => Ok(None),
            "reflect" => Ok(Some(
                json!({"id": agent_id.as_str(), "sentence": "a caller"}),
            )),
            "call" => {
                let target = payload["target"].as_str().unwrap_or("");
                let out = kernel
                    .send(vec![Item::new(
                        AgentId::from(target),
                        payload["payload"].clone(),
                    )])
                    .await
                    .remove(0);
                Ok(Some(out.result))
            }
            "send" => {
                let text = payload["text"].as_str().unwrap_or("").to_string();
                SENT.lock()
                    .unwrap()
                    .push((agent_id.as_str().to_string(), text));
                Ok(Some(json!({"ok": true, "verb": "send"})))
            }
            other => {
                if other == "pause" {
                    PAUSED.lock().unwrap().push(agent_id.as_str().to_string());
                }
                Ok(Some(json!({"ok": true, "verb": other})))
            }
        }
    }
}

/// Stands for ai_core without the dependency: `fork {at}` makes a SIBLING with the
/// source's record minus `callable_by`/`delete_lock`, as ai_core's fork does. `at` 99
/// is refused `cuts_turn`, 98 `no_such_row`.
struct ForkBundle;

#[async_trait]
impl Bundle for ForkBundle {
    fn name(&self) -> &str {
        "forkable"
    }
    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        if verb == "send" {
            let text = payload["text"].as_str().unwrap_or("").to_string();
            SENT.lock()
                .unwrap()
                .push((agent_id.as_str().to_string(), text));
            return Ok(Some(json!({"ok": true, "verb": "send"})));
        }
        if verb != "fork" {
            return Ok(None);
        }
        let at = payload["at"].as_i64().unwrap_or(0);
        let refuse = |reason: &str| {
            Ok(Some(json!({
                "error": format!("fork: refused ({reason})"), "reason": reason,
                "hint": "a hint", "at": at,
            })))
        };
        if at == 99 {
            return refuse("cuts_turn");
        }
        if at == 98 {
            return refuse("no_such_row");
        }
        let src = agent_of(kernel, agent_id).unwrap();
        let mut meta = meta_of(&src);
        for k in ["callable_by", "delete_lock"] {
            meta.remove(k);
        }
        meta.insert("forked_from".into(), json!(agent_id.as_str()));
        meta.insert("handler_module".into(), json!("forkable.tools"));
        meta.insert(
            "parent_id".into(),
            json!(src.parent_id.as_ref().map_or("kernel", |p| p.as_str())),
        );
        meta.insert("type".into(), json!("create_agent"));
        let root = kernel.root().unwrap();
        let made =
            fantastic_kernel::lifecycle::create_from_payload(kernel, &root, &Value::Object(meta))
                .await;
        let id = made["id"].clone();
        Ok(Some(json!({"id": id, "copied": 3, "at": at})))
    }
}

/// A tool whose record refuses every `callable_by` edit while it carries
/// `refuse_grants` (stands for a bundle whose `validate_meta` says no).
struct PickyBundle;

#[async_trait]
impl Bundle for PickyBundle {
    fn name(&self) -> &str {
        "picky"
    }
    async fn handle(
        &self,
        _agent_id: &AgentId,
        _payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        Ok(None)
    }
    fn validate_meta(
        &self,
        meta: &Map<String, Value>,
        record: &Map<String, Value>,
    ) -> Option<String> {
        (meta.contains_key("callable_by") && record.get("refuse_grants") == Some(&json!(true)))
            .then(|| "callable_by refused".to_string())
    }
}

const WRITER: &str = "---\nname: writer\npurpose: Writes the first draft of a text.\ntier: fast\nmodel: from-the-card\ngrants:\n  $registry: [roles, report]\n  notes: [read]\n---\nYou write clearly and briefly.\n";
const REVIEWER: &str = "---\nname: reviewer\npurpose: Reads a draft and says what to fix.\ntier: fast\n---\nYou review drafts.\n";
const SUBLEAD: &str = "---\nname: sublead\npurpose: Runs a branch of the work.\ntier: fast\ngrants:\n  $registry: [roles, spawn, get_count, report]\n  notes: [read]\n---\nYou run one branch.\n";

fn write_cards(dir: &Path, cards: &[(&str, &str)]) {
    std::fs::create_dir_all(dir).unwrap();
    for (n, t) in cards {
        std::fs::write(dir.join(n), t).unwrap();
    }
}

fn new_kernel() -> Kernel {
    let mut kernel = Kernel::new();
    kernel.bundles.register(
        fantastic_core::HANDLER_MODULE,
        fantastic_core::KernelStateBundle::default(),
    );
    kernel.bundles.register("caller.tools", CallerBundle);
    kernel.bundles.register("picky.tools", PickyBundle);
    kernel.bundles.register("forkable.tools", ForkBundle);
    kernel
        .bundles
        .register(HANDLER_MODULE, AgenticResourcesBundle);
    kernel
}

fn root_of(kernel: &Kernel, tmp: &TempDir) -> Arc<Agent> {
    let root = Agent::new(
        AgentId::from("kernel_state"),
        Some(fantastic_core::HANDLER_MODULE.to_string()),
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    root
}

async fn send(kernel: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    kernel
        .send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
        .result
}

async fn create(kernel: &Arc<Kernel>, payload: Value) -> Value {
    let mut p = payload;
    p["type"] = json!("create_agent");
    p["parent_id"] = json!("kernel");
    let r = send(kernel, "kernel_state", p).await;
    assert!(r.get("error").is_none(), "create: {r}");
    r
}

/// Per-test ids (the tempdir name is unique): the bundle keeps no state, but ids
/// stay unique across parallel tests anyway.
fn uid(tmp: &TempDir, base: &str) -> String {
    let n = tmp
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .replace('.', "_");
    format!("{base}_{n}")
}

struct Rig {
    kernel: Arc<Kernel>,
    reg: String,
    callers: Vec<String>,
    dir: std::path::PathBuf,
}

/// A kernel with `callers` (caller.tools agents allowed to call the registry) and
/// an enabled registry over `cards`, with tiers `fast` and `lead` on `caller.tools`.
async fn rig(tmp: &TempDir, callers: &[&str], cards: &[(&str, &str)]) -> Rig {
    let kernel = Arc::new(new_kernel());
    root_of(&kernel, tmp);
    let dir = tmp.path().join("roles");
    write_cards(&dir, cards);
    let reg = uid(tmp, "ar");
    let ids: Vec<String> = callers.iter().map(|c| uid(tmp, c)).collect();
    for c in &ids {
        create(
            &kernel,
            json!({"handler_module": "caller.tools", "id": c,
                   "can_call": {reg.clone(): "*", "notes": "*"}}),
        )
        .await;
    }
    let mut cb = Map::new();
    for c in &ids {
        cb.insert(c.clone(), json!("*"));
    }
    let tier = json!({"handler_module": "caller.tools", "model": "m1", "endpoint": "http://x"});
    create(
        &kernel,
        json!({"handler_module": HANDLER_MODULE, "id": reg,
               "role_dirs": [dir.to_string_lossy()],
               "tiers": {"fast": tier.clone(), "lead": tier},
               "callable_by": cb}),
    )
    .await;
    Rig {
        kernel,
        reg,
        callers: ids,
        dir,
    }
}

impl Rig {
    async fn op(&self, payload: Value) -> Value {
        send(&self.kernel, &self.reg, payload).await
    }
    /// `who` calls the registry (or any `target`) as itself.
    async fn call(&self, who: &str, target: &str, payload: Value) -> Value {
        send(
            &self.kernel,
            who,
            json!({"type": "call", "target": target, "payload": payload}),
        )
        .await
    }
    async fn spawn_as(&self, who: &str, payload: Value) -> Value {
        let mut p = payload;
        p["type"] = json!("spawn");
        self.call(who, &self.reg, p).await
    }
    fn meta(&self, id: &str) -> Map<String, Value> {
        meta_of(&self.kernel.agents.get(&AgentId::from(id)).unwrap())
    }
    async fn count(&self, role: Option<&str>) -> Value {
        match role {
            Some(r) => self.op(json!({"type": "get_count", "role": r})).await,
            None => self.op(json!({"type": "get_count"})).await,
        }
    }
}

fn id_of(v: &Value) -> String {
    v["id"]
        .as_str()
        .unwrap_or_else(|| panic!("{v}"))
        .to_string()
}

async fn deleted(r: &Rig, id: &str) -> Value {
    send(
        &r.kernel,
        "kernel_state",
        json!({"type": "delete_agent", "id": id}),
    )
    .await
}

// ── cards ───────────────────────────────────────────────────────────

#[test]
fn card_parses_header_and_directive() {
    let c = cards::parse_card(WRITER, "writer.md").unwrap();
    assert_eq!(c.name, "writer");
    assert_eq!(c.purpose, "Writes the first draft of a text.");
    assert_eq!(c.tier, "fast");
    assert_eq!(c.grants["notes"], json!(["read"]));
    assert_eq!(c.grants["$registry"], json!(["roles", "report"]));
    assert_eq!(c.directive, "You write clearly and briefly.");
}

#[test]
fn bad_header_refused_with_reason() {
    assert_eq!(
        cards::parse_card("no header here", "a.md").unwrap_err(),
        "card a.md: missing header"
    );
    let no_tier = "---\nname: x\npurpose: p\n---\nbody\n";
    assert_eq!(
        cards::parse_card(no_tier, "b.md").unwrap_err(),
        "card b.md: tier required"
    );
    let bad_grants = "---\nname: x\npurpose: p\ntier: fast\ngrants: [roles]\n---\nbody\n";
    assert_eq!(
        cards::parse_card(bad_grants, "c.md").unwrap_err(),
        "card c.md: grants must be a map of \"*\" or verb lists"
    );
    let bad_verbs = "---\nname: x\npurpose: p\ntier: fast\ngrants:\n  notes: 5\n---\nbody\n";
    assert!(cards::parse_card(bad_verbs, "d.md")
        .unwrap_err()
        .contains("grants must be a map"));
    // listed in `errors`, not loaded; a duplicate name is refused too
    let tmp = TempDir::new().unwrap();
    write_cards(
        tmp.path(),
        &[
            ("a.md", "no header"),
            ("b.md", no_tier),
            ("w1.md", WRITER),
            ("w2.md", WRITER),
        ],
    );
    let (cards, errors) = cards::load_dir(&[tmp.path().to_string_lossy().to_string()]);
    let names: Vec<&str> = cards.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["project_lead", "writer"]);
    assert!(
        errors.contains(&"card a.md: missing header".to_string()),
        "{errors:?}"
    );
    assert!(
        errors.contains(&"card b.md: tier required".to_string()),
        "{errors:?}"
    );
    assert!(
        errors.contains(&"card w2.md: duplicate role writer".to_string()),
        "{errors:?}"
    );
}

#[tokio::test]
async fn roles_lists_every_card_with_purpose() {
    let tmp = TempDir::new().unwrap();
    let r = rig(
        &tmp,
        &[],
        &[
            ("writer.md", WRITER),
            ("reviewer.md", REVIEWER),
            ("bad.md", "x"),
        ],
    )
    .await;
    let out = r.op(json!({"type": "roles"})).await;
    let roles = out["roles"].as_array().unwrap();
    let find = |n: &str| roles.iter().find(|x| x["name"] == n).cloned();
    assert_eq!(
        find("writer").unwrap()["purpose"],
        "Writes the first draft of a text."
    );
    assert_eq!(
        find("reviewer").unwrap()["purpose"],
        "Reads a draft and says what to fix."
    );
    assert!(
        find("project_lead").is_some(),
        "the shipped card is listed: {out}"
    );
    assert_eq!(out["errors"], json!(["card bad.md: missing header"]));
    // the docs carry the same list; a file added later is a role added
    let d = send(
        &r.kernel,
        "kernel",
        json!({"type": "docs", "of": r.reg, "tier": "detailed"}),
    )
    .await;
    let doc = d["verbs"]["roles"].as_str().unwrap_or_default().to_string();
    assert!(
        doc.contains("writer: Writes the first draft of a text."),
        "{d}"
    );
    assert!(doc.contains("project_lead: "), "{d}");
    write_cards(
        &r.dir,
        &[(
            "extra.md",
            "---\nname: extra\npurpose: Added later.\ntier: fast\n---\nd\n",
        )],
    );
    let again = r.op(json!({"type": "roles"})).await;
    assert!(again["roles"]
        .as_array()
        .unwrap()
        .iter()
        .any(|x| x["name"] == "extra"));
}

#[tokio::test]
async fn role_returns_full_card_and_unknown_is_not_found() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp, &[], &[("writer.md", WRITER)]).await;
    let c = r.op(json!({"type": "role", "name": "writer"})).await;
    assert_eq!(c["name"], "writer");
    assert_eq!(c["tier"], "fast");
    assert_eq!(c["directive"], "You write clearly and briefly.");
    assert_eq!(c["grants"]["notes"], json!(["read"]));
    let n = r.op(json!({"type": "role", "name": "ghost"})).await;
    assert_eq!(n["reason"], "not_found", "{n}");
}

// ── spawn ───────────────────────────────────────────────────────────

#[tokio::test]
async fn spawn_creates_child_with_directive_tier_grants() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp, &["lead"], &[("writer.md", WRITER)]).await;
    let lead = &r.callers[0];
    let out = r
        .spawn_as(lead, json!({"role": "writer", "name": "scribe"}))
        .await;
    assert_eq!(out["role"], "writer", "{out}");
    assert_eq!(out["tier"], "fast");
    let id = out["id"].as_str().unwrap();
    let a = r.kernel.agents.get(&AgentId::from(id)).unwrap();
    assert_eq!(
        a.parent_id.as_ref().unwrap().as_str(),
        lead.as_str(),
        "a child of the caller"
    );
    let m = meta_of(&a);
    assert_eq!(m["system_prompt"], "You write clearly and briefly.");
    assert_eq!(m["model"], "m1", "the tier's model, never the card's");
    assert_eq!(m["endpoint"], "http://x");
    assert_eq!(a.handler_module.as_deref(), Some("caller.tools"));
    assert_eq!(m["spawned_by"], json!(r.reg));
    assert_eq!(m["role"], "writer");
    assert_eq!(m["name"], "scribe");
    assert_eq!(m["can_call"]["notes"], json!(["read"]));
    assert_eq!(m["can_call"][&r.reg], json!(["roles", "report"]));
}

#[tokio::test]
async fn spawn_opens_the_tiers_memory_store_to_the_child() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp, &["lead"], &[("writer.md", WRITER)]).await;
    let mem = uid(&tmp, "mem");
    create(
        &r.kernel,
        json!({"handler_module": "caller.tools", "id": mem}),
    )
    .await;
    let tier = json!({"handler_module": "caller.tools", "memory_store_id": mem});
    let mut patch = Map::new();
    patch.insert("tiers".to_string(), json!({"fast": tier}));
    r.kernel.update(&AgentId::from(r.reg.as_str()), patch);
    let id = r.spawn_as(&r.callers[0], json!({"role": "writer"})).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        r.meta(&id)["can_call"][&mem],
        "*",
        "the child may use its store"
    );
    assert_eq!(
        r.meta(&mem)["callable_by"][&id],
        "*",
        "and the store accepts the child"
    );
}

#[tokio::test]
async fn spawn_unknown_tier_refused() {
    let tmp = TempDir::new().unwrap();
    let card = "---\nname: odd\npurpose: Odd.\ntier: nowhere\n---\nd\n";
    let r = rig(&tmp, &["lead"], &[("odd.md", card)]).await;
    let out = r.spawn_as(&r.callers[0], json!({"role": "odd"})).await;
    assert_eq!(out["reason"], "unknown_tier", "{out}");
    assert!(
        out["error"].as_str().unwrap().contains("'nowhere'"),
        "{out}"
    );
    let none = r.spawn_as(&r.callers[0], json!({"role": "ghost"})).await;
    assert_eq!(none["reason"], "not_found");
    assert_eq!(r.op(json!({"type": "fleet"})).await["roles"], json!({}));
}

#[tokio::test]
async fn per_role_limit_refuses_past_cap() {
    let tmp = TempDir::new().unwrap();
    let r = rig(
        &tmp,
        &["lead"],
        &[("writer.md", WRITER), ("reviewer.md", REVIEWER)],
    )
    .await;
    let lead = &r.callers[0];
    assert_eq!(
        r.op(json!({"type": "set_limit", "role": "writer", "max": 2}))
            .await["ok"],
        true
    );
    for _ in 0..2 {
        assert!(r
            .spawn_as(lead, json!({"role": "writer"}))
            .await
            .get("id")
            .is_some());
    }
    let out = r.spawn_as(lead, json!({"role": "writer"})).await;
    assert_eq!(out["reason"], "limit", "{out}");
    assert_eq!(out["scope"], "role");
    assert_eq!(out["role"], "writer");
    assert_eq!(out["running"], 2);
    assert_eq!(out["max"], 2);
    assert!(out["error"].as_str().unwrap().contains("writer"));
    // another role is not capped
    assert!(r
        .spawn_as(lead, json!({"role": "reviewer"}))
        .await
        .get("id")
        .is_some());
}

#[tokio::test]
async fn total_limit_refuses_past_cap() {
    let tmp = TempDir::new().unwrap();
    let r = rig(
        &tmp,
        &["lead"],
        &[("writer.md", WRITER), ("reviewer.md", REVIEWER)],
    )
    .await;
    let lead = &r.callers[0];
    r.op(json!({"type": "set_limit", "max": 2})).await;
    assert!(r
        .spawn_as(lead, json!({"role": "writer"}))
        .await
        .get("id")
        .is_some());
    assert!(r
        .spawn_as(lead, json!({"role": "reviewer"}))
        .await
        .get("id")
        .is_some());
    let out = r.spawn_as(lead, json!({"role": "writer"})).await;
    assert_eq!(out["reason"], "limit", "{out}");
    assert_eq!(out["scope"], "total");
    assert_eq!(
        (out["running"].clone(), out["max"].clone()),
        (json!(2), json!(2))
    );
    assert_eq!(r.count(None).await, json!({"running": 2, "max": 2}));
}

#[tokio::test]
async fn set_limit_null_clears() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp, &["lead"], &[("writer.md", WRITER)]).await;
    let lead = &r.callers[0];
    r.op(json!({"type": "set_limit", "role": "writer", "max": 1}))
        .await;
    r.spawn_as(lead, json!({"role": "writer"})).await;
    assert_eq!(
        r.spawn_as(lead, json!({"role": "writer"})).await["reason"],
        "limit"
    );
    assert_eq!(
        r.op(json!({"type": "set_limit", "role": "writer", "max": null}))
            .await["ok"],
        true
    );
    assert_eq!(
        r.count(Some("writer")).await,
        json!({"running": 1, "max": null})
    );
    assert!(r
        .spawn_as(lead, json!({"role": "writer"}))
        .await
        .get("id")
        .is_some());
    for bad in [json!(-1), json!(1.5), json!("2"), json!(true)] {
        let out = r.op(json!({"type": "set_limit", "max": bad})).await;
        assert_eq!(out["reason"], "bad_arg", "{bad}: {out}");
    }
    assert_eq!(
        r.op(json!({"type": "set_limit"})).await["reason"],
        "missing_arg"
    );
}

#[tokio::test]
async fn get_count_after_spawn_delete_restart() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp, &["lead"], &[("writer.md", WRITER)]).await;
    let lead = &r.callers[0];
    r.op(json!({"type": "set_limit", "role": "writer", "max": 5}))
        .await;
    let a = r.spawn_as(lead, json!({"role": "writer"})).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    r.spawn_as(lead, json!({"role": "writer"})).await;
    assert_eq!(
        r.count(Some("writer")).await,
        json!({"running": 2, "max": 5})
    );
    let gone = send(
        &r.kernel,
        "kernel_state",
        json!({"type": "delete_agent", "id": a}),
    )
    .await;
    assert_eq!(gone["deleted"], true, "{gone}");
    assert_eq!(r.count(Some("writer")).await["running"], 1);
    // a restart: a second kernel loaded from the persisted records
    let state = r.kernel.save();
    let k2 = Arc::new(new_kernel());
    k2.load(state).unwrap();
    let after = send(&k2, &r.reg, json!({"type": "get_count", "role": "writer"})).await;
    assert_eq!(
        after,
        json!({"running": 1, "max": 5}),
        "rebuilt from the records"
    );
    assert_eq!(
        send(&k2, &r.reg, json!({"type": "get_count"})).await["running"],
        1
    );
}

#[tokio::test]
async fn fleet_groups_by_role_with_ids() {
    let tmp = TempDir::new().unwrap();
    let r = rig(
        &tmp,
        &["lead"],
        &[("writer.md", WRITER), ("reviewer.md", REVIEWER)],
    )
    .await;
    let lead = &r.callers[0];
    let w1 = r.spawn_as(lead, json!({"role": "writer"})).await["id"].clone();
    let w2 = r.spawn_as(lead, json!({"role": "writer"})).await["id"].clone();
    let v1 = r.spawn_as(lead, json!({"role": "reviewer"})).await["id"].clone();
    create(
        &r.kernel,
        json!({"handler_module": "caller.tools", "id": uid(&tmp, "stray"), "role": "writer"}),
    )
    .await;
    let f = r.op(json!({"type": "fleet"})).await;
    let ids = |role: &str| -> Vec<Value> {
        let mut v: Vec<Value> = f["roles"][role]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["id"].clone())
            .collect();
        v.sort_by_key(ToString::to_string);
        v
    };
    let mut want = vec![w1, w2];
    want.sort_by_key(ToString::to_string);
    assert_eq!(ids("writer"), want, "the stray agent is not listed: {f}");
    assert_eq!(ids("reviewer"), vec![v1]);
    assert_eq!(
        f["roles"]["writer"][0],
        json!({"id": f["roles"]["writer"][0]["id"].clone()})
    );
}

#[test]
fn disabled_by_default() {
    let mut reg = BundleRegistry::new();
    assert!(!register_if_enabled(&mut reg, None));
    assert!(!register_if_enabled(&mut reg, Some("0")));
    assert!(!register_if_enabled(&mut reg, Some("")));
    assert!(reg.get(HANDLER_MODULE).is_none(), "off: no handler");
    assert!(register_if_enabled(&mut reg, Some("1")));
    assert!(reg.get(HANDLER_MODULE).is_some());
    assert!(
        enabled_value(Some("TRUE")) && enabled_value(Some("yes")) && !enabled_value(Some("no"))
    );
}

#[tokio::test]
async fn disabled_without_registry_handler_or_config() {
    // flag off: the handler is not in the kernel's registry, so the agent is unknown
    let tmp = TempDir::new().unwrap();
    let mut off = Kernel::new();
    off.bundles.register(
        fantastic_core::HANDLER_MODULE,
        fantastic_core::KernelStateBundle::default(),
    );
    let off = Arc::new(off);
    root_of(&off, &tmp);
    let id = uid(&tmp, "ar");
    create(&off, json!({"handler_module": HANDLER_MODULE, "id": id})).await;
    let out = send(&off, &id, json!({"type": "roles"})).await;
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .contains("cannot be imported in this runtime"),
        "{out}"
    );
    // flag on, but no config on the record: every verb refuses `disabled`
    let kernel = Arc::new(new_kernel());
    root_of(&kernel, &tmp);
    create(&kernel, json!({"handler_module": HANDLER_MODULE, "id": id})).await;
    for v in [
        "roles",
        "role",
        "spawn",
        "report",
        "fleet",
        "set_limit",
        "get_count",
    ] {
        let out = send(&kernel, &id, json!({"type": v})).await;
        assert_eq!(
            out,
            json!({
                "error": "agentic_resources is disabled",
                "reason": "disabled",
                "hint": "give the record role_dirs",
            }),
            "{v}"
        );
    }
}

// ── the extended surface ────────────────────────────────────────────

#[tokio::test]
async fn spawn_grants_parent_pause_resume_stop_both_ends() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp, &["lead", "sib"], &[("writer.md", REVIEWER)]).await;
    let (lead, sib) = (&r.callers[0], &r.callers[1]);
    let id = r.spawn_as(lead, json!({"role": "reviewer"})).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let want = json!(["pause", "resume", "stop"]);
    assert_eq!(r.meta(lead)["can_call"][&id], want);
    assert_eq!(r.meta(&id)["callable_by"][lead], want);
    // nothing wider on the child's side: no other caller is let in by the spawn
    let cb = r.meta(&id)["callable_by"].as_object().unwrap().clone();
    let mut keys: Vec<&String> = cb.keys().collect();
    keys.sort();
    let mut expect = ["$self".to_string(), lead.clone()];
    expect.sort();
    assert_eq!(keys, expect.iter().collect::<Vec<_>>());
    // the lead controls it
    for verb in ["pause", "resume", "stop"] {
        let out = r.call(lead, &id, json!({"type": verb})).await;
        assert_eq!(out["verb"], verb, "{out}");
    }
    // a verb outside the three is refused, and a sibling cannot pause it
    assert_eq!(
        r.call(lead, &id, json!({"type": "send"})).await["reason"],
        "unauthorized"
    );
    assert_eq!(
        r.call(sib, &id, json!({"type": "pause"})).await["reason"],
        "unauthorized"
    );
}

#[tokio::test]
async fn spawn_task_string_is_first_message_and_object_refused() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp, &["lead"], &[("writer.md", WRITER)]).await;
    let lead = &r.callers[0];
    // a task object (the old task line) is refused
    let before = r.kernel.agents.len();
    let bad = r
        .spawn_as(
            lead,
            json!({"role": "writer", "task": {"label": "draft intro"}}),
        )
        .await;
    assert_eq!(bad["reason"], "bad_arg", "{bad}");
    assert_eq!(r.kernel.agents.len(), before, "nothing was created");
    let c = r
        .spawn_as(lead, json!({"role": "writer", "task": "do it"}))
        .await;
    assert!(r.meta(c["id"].as_str().unwrap()).get("task").is_none());
    // the string is the child's first message, delivered without holding the spawn
    let cid = c["id"].as_str().unwrap().to_string();
    for _ in 0..100 {
        if SENT
            .lock()
            .unwrap()
            .iter()
            .any(|(a, t)| *a == cid && t == "do it")
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(SENT
        .lock()
        .unwrap()
        .iter()
        .any(|(a, t)| *a == cid && t == "do it"));
}

#[tokio::test]
async fn report_messages_parent_and_stores_nothing() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp, &["lead", "other"], &[("writer.md", WRITER)]).await;
    let (lead, other) = (&r.callers[0], &r.callers[1]);
    let mut inbox = r.kernel.ensure_inbox(&AgentId::from(lead.as_str()));
    let id = r.spawn_as(lead, json!({"role": "writer"})).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let rep = r
        .call(
            &id,
            &r.reg,
            json!({"type": "report", "state": "done", "result": {"words": 120}}),
        )
        .await;
    assert_eq!(rep, json!({"ok": true}), "{rep}");
    assert!(r.meta(&id).get("task").is_none(), "nothing is stored");
    let mut got = None;
    while let Some(item) = inbox.try_recv() {
        if item.payload["type"] == "report" {
            got = Some(item.payload);
        }
    }
    assert_eq!(
        got.unwrap(),
        json!({"type": "report", "from": id, "state": "done", "result": {"words": 120}})
    );
    // anyone else is refused, and so is a state outside the six
    let no = r
        .call(other, &r.reg, json!({"type": "report", "state": "done"}))
        .await;
    assert_eq!(no["reason"], "not_spawned", "{no}");
    assert_eq!(
        r.op(json!({"type": "report", "state": "done"})).await["reason"],
        "not_spawned"
    );
    let bad = r
        .call(&id, &r.reg, json!({"type": "report", "state": "finished"}))
        .await;
    assert_eq!(bad["reason"], "bad_arg", "{bad}");
    assert!(r.meta(&id).get("task").is_none());
}

#[tokio::test]
async fn delegated_limits_bound_subtree() {
    let tmp = TempDir::new().unwrap();
    let r = rig(
        &tmp,
        &["boss"],
        &[("writer.md", WRITER), ("sublead.md", SUBLEAD)],
    )
    .await;
    let boss = &r.callers[0];
    let sub = r
        .spawn_as(boss, json!({"role": "sublead", "limits": {"total": 2}}))
        .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(r.meta(&sub)["limits"], json!({"total": 2}));
    // more than it was given is refused; the sub-lead's own slot is not its to hand on
    let big = r
        .spawn_as(&sub, json!({"role": "sublead", "limits": {"total": 5}}))
        .await;
    assert_eq!(
        (big["reason"].clone(), big["scope"].clone()),
        (json!("limit"), json!("request")),
        "{big}"
    );
    let ok = r
        .spawn_as(&sub, json!({"role": "sublead", "limits": {"total": 1}}))
        .await;
    assert!(ok.get("id").is_some(), "{ok}");
    assert!(r
        .spawn_as(&sub, json!({"role": "writer"}))
        .await
        .get("id")
        .is_some());
    let third = r.spawn_as(&sub, json!({"role": "writer"})).await;
    assert_eq!(third["reason"], "limit", "{third}");
    assert_eq!(third["scope"], "subtree_total");
    assert_eq!(
        (third["running"].clone(), third["max"].clone()),
        (json!(2), json!(2))
    );
    // the deepest sub-lead was given 1: it cannot spawn past that either, and the
    // ancestors' quota is the bound of the whole subtree
    let deep = ok["id"].as_str().unwrap();
    let blocked = r.spawn_as(deep, json!({"role": "writer"})).await;
    assert_eq!(blocked["reason"], "limit", "{blocked}");
    // the boss itself carries no quota
    assert!(r
        .spawn_as(boss, json!({"role": "writer"}))
        .await
        .get("id")
        .is_some());
}

#[tokio::test]
async fn get_count_counts_every_spawned_agent() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp, &["lead"], &[("writer.md", WRITER)]).await;
    let lead = &r.callers[0];
    r.op(json!({"type": "set_limit", "max": 2})).await;
    let a = r.spawn_as(lead, json!({"role": "writer"})).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    r.spawn_as(lead, json!({"role": "writer"})).await;
    assert_eq!(r.count(None).await, json!({"running": 2, "max": 2}));
    let third = r.spawn_as(lead, json!({"role": "writer"})).await;
    assert_eq!(third["reason"], "limit", "{third}");
    // a report frees nothing; only delete_agent does
    r.call(&a, &r.reg, json!({"type": "report", "state": "done"}))
        .await;
    assert_eq!(r.count(None).await["running"], 2);
    assert_eq!(
        r.spawn_as(lead, json!({"role": "writer"})).await["reason"],
        "limit"
    );
    let gone = send(
        &r.kernel,
        "kernel_state",
        json!({"type": "delete_agent", "id": a}),
    )
    .await;
    assert_eq!(gone["deleted"], true);
    assert_eq!(r.count(None).await["running"], 1);
    assert!(r
        .spawn_as(lead, json!({"role": "writer"}))
        .await
        .get("id")
        .is_some());
}

#[tokio::test]
async fn no_auto_cleanup_on_done() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp, &["lead"], &[("writer.md", WRITER)]).await;
    let lead = &r.callers[0];
    let a = r.spawn_as(lead, json!({"role": "writer"})).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let b = r.spawn_as(lead, json!({"role": "writer"})).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let before = r.kernel.agents.len();
    r.call(
        &a,
        &r.reg,
        json!({"type": "report", "state": "done", "result": "ok"}),
    )
    .await;
    r.call(
        &b,
        &r.reg,
        json!({"type": "report", "state": "failed", "result": "no"}),
    )
    .await;
    assert_eq!(r.kernel.agents.len(), before, "nothing was deleted");
    let m = r.meta(&a);
    assert_eq!(
        (m["spawned_by"].clone(), m["role"].clone()),
        (json!(r.reg), json!("writer"))
    );
    assert!(m.get("task").is_none());
    assert_eq!(r.count(None).await["running"], 2, "the count stays");
    let f = r.op(json!({"type": "fleet"})).await;
    assert_eq!(
        f["roles"]["writer"].as_array().unwrap().len(),
        2,
        "still listed: {f}"
    );
    // only an explicit delete removes one
    let gone = send(
        &r.kernel,
        "kernel_state",
        json!({"type": "delete_agent", "id": a}),
    )
    .await;
    assert_eq!(gone["deleted"], true);
    assert_eq!(r.kernel.agents.len(), before - 1);
}

#[tokio::test]
async fn project_lead_card_ships() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp, &[], &[]).await;
    let c = r.op(json!({"type": "role", "name": "project_lead"})).await;
    let text = format!(
        "{} {}",
        c["purpose"].as_str().unwrap(),
        c["directive"].as_str().unwrap()
    )
    .to_lowercase();
    for want in [
        "roles",
        "get_count",
        "spawn",
        "await_set",
        "report",
        "never clean up",
        "fork_points",
    ] {
        assert!(text.contains(want), "the directive names {want}: {text}");
    }
    for code in [
        "code",
        "compile",
        "repository",
        "git ",
        "test suite",
        "pull request",
    ] {
        assert!(!text.contains(code), "not code-oriented, found {code:?}");
    }
    let grants = c["grants"][REGISTRY_TOKEN].as_array().unwrap().clone();
    for v in ["roles", "role", "spawn", "get_count", "fleet", "report"] {
        assert!(grants.contains(&json!(v)), "grants {v}");
    }
    // and it spawns (the lead tier is config)
    let lead = rig(&TempDir::new().unwrap(), &["boss"], &[]).await;
    assert!(lead
        .spawn_as(&lead.callers[0], json!({"role": "project_lead"}))
        .await
        .get("id")
        .is_some());
}

#[test]
fn readme_is_python_verbatim() {
    let py = Path::new(env!("CARGO_MANIFEST_DIR")).join(
        "../../../../python/bundled_agents/agentic_resources/src/agentic_resources/readme.md",
    );
    let src = std::fs::read_to_string(&py).unwrap_or_else(|e| panic!("{}: {e}", py.display()));
    assert_eq!(src, README, "readme has drifted from {}", py.display());
    let pycard = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../python/bundled_agents/agentic_resources/src/agentic_resources/cards/project_lead.md");
    assert_eq!(
        std::fs::read_to_string(pycard).unwrap(),
        cards::PROJECT_LEAD
    );
}

// ── spawn from a fork point ─────────────────────────────────────────

/// A rig whose tier carries a `memory_store_id`, plus a source agent that stands
/// for an agent with history: its own prompt, store, grants and stale registry keys.
async fn fork_rig(tmp: &TempDir, callers: &[&str], cards: &[(&str, &str)]) -> (Rig, String) {
    let r = rig(tmp, callers, cards).await;
    let tier = json!({"handler_module": "caller.tools", "model": "m1", "endpoint": "http://x",
                      "memory_store_id": "tier_store"});
    let mut patch = Map::new();
    patch.insert(
        "tiers".to_string(),
        json!({"fast": tier.clone(), "lead": tier}),
    );
    r.kernel.update(&AgentId::from(r.reg.as_str()), patch);
    let src = uid(tmp, "src");
    create(
        &r.kernel,
        json!({"handler_module": "forkable.tools", "id": src,
               "system_prompt": "SOURCE PROMPT", "model": "src-model",
               "memory_store_id": "src_store",
               "can_call": {"src_store": "*", "secret_agent": "*"},
               "spawned_by": "stale_reg", "role": "old_role", "tier": "old",
               "task": {"label": "old task", "state": "done", "waits_on": [], "result": null},
               "limits": {"total": 9}, "lead": "stale_lead"}),
    )
    .await;
    for c in &r.callers {
        grant_fork(&r, c, &src);
    }
    (r, src)
}

fn grant_fork(r: &Rig, who: &str, src: &str) {
    let s = src.to_string();
    r.kernel
        .edit_can_call(&AgentId::from(who), move |m| {
            grant_verbs(m, &s, &json!(["fork"]));
            true
        })
        .unwrap();
}

fn forks_of(r: &Rig, src: &str) -> Vec<String> {
    r.kernel
        .agents
        .iter()
        .filter(|e| {
            meta_of(e.value())
                .get("forked_from")
                .and_then(Value::as_str)
                == Some(src)
        })
        .map(|e| e.value().id.as_str().to_string())
        .collect()
}

fn from(src: &str, at: i64) -> Value {
    json!({"agent": src, "at": at})
}

#[tokio::test]
async fn spawn_from_forks_and_applies_the_role_card() {
    let tmp = TempDir::new().unwrap();
    let (r, src) = fork_rig(&tmp, &["lead"], &[("writer.md", WRITER)]).await;
    let lead = &r.callers[0];
    let out = r
        .spawn_as(lead, json!({"role": "writer", "from": from(&src, 5)}))
        .await;
    assert_eq!(out["role"], "writer", "{out}");
    assert_eq!(out["tier"], "fast");
    assert_eq!(out["from"], from(&src, 5));
    assert_eq!(out["copied"], 3);
    let id = out["id"].as_str().unwrap();
    assert_eq!(forks_of(&r, &src), [id], "made by fork");
    let m = r.meta(id);
    assert_eq!(m["system_prompt"], "You write clearly and briefly.");
    assert_eq!(m["role"], "writer");
    assert_eq!(m["tier"], "fast");
    assert_eq!(m["spawned_by"], json!(r.reg));
    assert_eq!(m["model"], "m1", "the tier's model, not the source's");
    assert_eq!(m["lead"], json!(lead));
    assert_eq!(m["can_call"]["notes"], json!(["read"]));
    assert_eq!(m["can_call"][&r.reg], json!(["roles", "report"]));
    assert_eq!(
        r.meta(&r.reg)["callable_by"][id],
        json!(["roles", "report"]),
        "the registry hears its report"
    );
    // the name is applied when given
    let named = r
        .spawn_as(
            lead,
            json!({"role": "writer", "from": from(&src, 5), "name": "scribe"}),
        )
        .await;
    assert_eq!(r.meta(named["id"].as_str().unwrap())["name"], "scribe");
}

#[tokio::test]
async fn spawn_from_keeps_the_source_store_and_drops_its_other_grants() {
    let tmp = TempDir::new().unwrap();
    let (r, src) = fork_rig(&tmp, &["lead"], &[("writer.md", WRITER)]).await;
    let id = r
        .spawn_as(
            &r.callers[0],
            json!({"role": "writer", "from": from(&src, 5)}),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let m = r.meta(&id);
    assert_eq!(m["memory_store_id"], "src_store", "not the tier's");
    assert_eq!(m["can_call"]["src_store"], "*");
    assert!(
        m["can_call"].get("secret_agent").is_none(),
        "{}",
        m["can_call"]
    );
    assert!(m["can_call"].get("tier_store").is_none());
    let keys: Vec<&String> = m["can_call"].as_object().unwrap().keys().collect();
    assert_eq!(keys.len(), 3, "store, notes, registry: {keys:?}");
}

#[tokio::test]
async fn spawn_from_drops_the_copied_task_and_overwrites_limits() {
    let tmp = TempDir::new().unwrap();
    let (r, src) = fork_rig(&tmp, &["lead"], &[("writer.md", WRITER)]).await;
    let lead = &r.callers[0];
    let bare = r
        .spawn_as(lead, json!({"role": "writer", "from": from(&src, 5)}))
        .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let m = r.meta(&bare);
    assert!(
        m.get("task").is_none(),
        "the source's task is dropped: {m:?}"
    );
    assert!(m.get("limits").is_none(), "{m:?}");
    let given = r
        .spawn_as(
            lead,
            json!({"role": "writer", "from": from(&src, 5),
                   "limits": {"per_role": 1}}),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let m = r.meta(&given);
    assert!(m.get("task").is_none(), "{m:?}");
    assert_eq!(m["limits"], json!({"per_role": 1}));
    assert_eq!(m["role"], "writer", "never the source's role");
    // the source itself is untouched
    assert_eq!(r.meta(&src)["task"]["label"], "old task");
    assert_eq!(r.meta(&src)["limits"], json!({"total": 9}));
}

#[tokio::test]
async fn spawn_from_task_string_is_sent_as_first_message() {
    let tmp = TempDir::new().unwrap();
    let (r, src) = fork_rig(&tmp, &["lead"], &[("writer.md", WRITER)]).await;
    let text = format!("branch task {}", uid(&tmp, "t"));
    let out = r
        .spawn_as(
            &r.callers[0],
            json!({"role": "writer", "from": from(&src, 5), "task": text}),
        )
        .await;
    let id = out["id"].as_str().unwrap().to_string();
    for _ in 0..100 {
        if SENT
            .lock()
            .unwrap()
            .iter()
            .any(|(a, t)| *a == id && *t == text)
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("the child never got its first message");
}

#[tokio::test]
async fn spawn_from_passes_fork_refusals_through() {
    let tmp = TempDir::new().unwrap();
    let (r, src) = fork_rig(&tmp, &["lead"], &[("writer.md", WRITER)]).await;
    let lead = &r.callers[0];
    for (at, reason) in [(99, "cuts_turn"), (98, "no_such_row")] {
        let out = r
            .spawn_as(lead, json!({"role": "writer", "from": from(&src, at)}))
            .await;
        assert_eq!(out["reason"], reason, "{out}");
        assert_eq!(out["at"], at);
        assert_eq!(out["hint"], "a hint");
        assert!(out["error"].as_str().unwrap().starts_with("fork:"), "{out}");
        assert!(forks_of(&r, &src).is_empty());
        assert_eq!(r.op(json!({"type": "fleet"})).await["roles"], json!({}));
        assert_eq!(r.count(None).await["running"], 0);
        let reg = r.meta(&r.reg);
        assert!(reg["can_call"].get(&src).is_none(), "no leftover grant");
        assert!(r.meta(&src)["callable_by"].get(&r.reg).is_none());
    }
}

#[tokio::test]
async fn spawn_from_respects_limits_before_forking() {
    let tmp = TempDir::new().unwrap();
    let (r, src) = fork_rig(&tmp, &["lead"], &[("writer.md", WRITER)]).await;
    let lead = &r.callers[0];
    r.op(json!({"type": "set_limit", "role": "writer", "max": 1}))
        .await;
    let first = r
        .spawn_as(lead, json!({"role": "writer", "from": from(&src, 5)}))
        .await;
    assert!(first.get("id").is_some(), "{first}");
    let out = r
        .spawn_as(lead, json!({"role": "writer", "from": from(&src, 5)}))
        .await;
    assert_eq!(out["reason"], "limit", "{out}");
    assert_eq!(out["scope"], "role");
    assert_eq!(forks_of(&r, &src).len(), 1, "fork was not called again");
}

#[tokio::test]
async fn spawn_from_requires_the_caller_to_hold_fork() {
    let tmp = TempDir::new().unwrap();
    let (r, src) = fork_rig(&tmp, &["lead", "other"], &[("writer.md", WRITER)]).await;
    let (lead, other) = (r.callers[0].clone(), r.callers[1].clone());
    // `other` loses what the rig gave it
    r.kernel
        .edit_can_call(&AgentId::from(other.as_str()), |m| m.remove(&src).is_some())
        .unwrap();
    let out = r
        .spawn_as(&other, json!({"role": "writer", "from": from(&src, 5)}))
        .await;
    assert_eq!(out["reason"], "not_allowed", "{out}");
    assert!(
        out["error"].as_str().unwrap().contains("may not fork"),
        "{out}"
    );
    assert!(forks_of(&r, &src).is_empty());
    assert!(r.meta(&r.reg)["can_call"].get(&src).is_none());
    let ok = r
        .spawn_as(&lead, json!({"role": "writer", "from": from(&src, 5)}))
        .await;
    assert!(ok.get("id").is_some(), "{ok}");
}

#[tokio::test]
async fn spawn_from_gives_the_parent_pause_resume_stop() {
    let tmp = TempDir::new().unwrap();
    let (r, src) = fork_rig(&tmp, &["lead"], &[("writer.md", WRITER)]).await;
    let lead = &r.callers[0];
    let id = r
        .spawn_as(lead, json!({"role": "writer", "from": from(&src, 5)}))
        .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        r.meta(lead)["can_call"][&id],
        json!(["pause", "resume", "stop"])
    );
    assert_eq!(
        r.meta(&id)["callable_by"][lead],
        json!(["pause", "resume", "stop"])
    );
}

#[tokio::test]
async fn spawn_from_counts_in_fleet_and_get_count_and_survives_restart() {
    let tmp = TempDir::new().unwrap();
    let (r, src) = fork_rig(&tmp, &["lead"], &[("writer.md", WRITER)]).await;
    let lead = &r.callers[0];
    let id = r
        .spawn_as(lead, json!({"role": "writer", "from": from(&src, 5)}))
        .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let f = r.op(json!({"type": "fleet"})).await;
    assert_eq!(f["roles"]["writer"], json!([{"id": id}]));
    assert_eq!(r.count(Some("writer")).await["running"], 1);
    let state = r.kernel.save();
    let k2 = Arc::new(new_kernel());
    k2.load(state).unwrap();
    let after = send(&k2, &r.reg, json!({"type": "get_count", "role": "writer"})).await;
    assert_eq!(after["running"], 1, "rebuilt from the records");
    let gone = send(
        &r.kernel,
        "kernel_state",
        json!({"type": "delete_agent", "id": id}),
    )
    .await;
    assert_eq!(gone["deleted"], true, "{gone}");
    assert_eq!(r.count(Some("writer")).await["running"], 0);
}

#[tokio::test]
async fn spawn_from_counts_against_the_leads_subtree_quota() {
    let tmp = TempDir::new().unwrap();
    let (r, src) = fork_rig(
        &tmp,
        &["boss"],
        &[("writer.md", WRITER), ("sublead.md", SUBLEAD)],
    )
    .await;
    let sub = r
        .spawn_as(
            &r.callers[0],
            json!({"role": "sublead", "limits": {"total": 1}}),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    grant_fork(&r, &sub, &src);
    let first = r
        .spawn_as(&sub, json!({"role": "writer", "from": from(&src, 5)}))
        .await;
    assert!(first.get("id").is_some(), "{first}");
    let second = r
        .spawn_as(&sub, json!({"role": "writer", "from": from(&src, 5)}))
        .await;
    assert_eq!(second["reason"], "limit", "{second}");
    assert_eq!(second["scope"], "subtree_total", "{second}");
    assert_eq!(forks_of(&r, &src).len(), 1);
}

#[tokio::test]
async fn spawn_from_bad_shape_refused() {
    let tmp = TempDir::new().unwrap();
    let (r, src) = fork_rig(&tmp, &["lead"], &[("writer.md", WRITER)]).await;
    let lead = &r.callers[0];
    for bad_from in [
        json!("x"),
        json!({"at": 1}),
        json!({"agent": src}),
        json!({"agent": src, "at": "1"}),
        json!({"agent": "", "at": 1}),
    ] {
        let out = r
            .spawn_as(lead, json!({"role": "writer", "from": bad_from}))
            .await;
        assert!(
            out["error"].as_str().unwrap_or("").contains("from"),
            "{bad_from}: {out}"
        );
        assert!(out["error"]
            .as_str()
            .unwrap()
            .contains("{agent:str, at:int}"));
    }
    assert!(forks_of(&r, &src).is_empty());
    // without `from` a spawn is unchanged
    let plain = r.spawn_as(lead, json!({"role": "writer"})).await;
    assert!(
        plain.get("id").is_some() && plain.get("from").is_none(),
        "{plain}"
    );
}

// ── the card's tool grants (#436) ───────────────────────────────────

const WIDE: &str = "---\nname: wide\npurpose: Reads and writes notes.\ntier: fast\ngrants:\n  $registry: [roles, report]\n  notes: [read, write]\n---\nYou write.\n";
const STAR: &str = "---\nname: star\npurpose: Anything on notes.\ntier: fast\ngrants:\n  notes: \"*\"\n---\nYou do all.\n";

async fn notes_tool(r: &Rig) {
    create(
        &r.kernel,
        json!({"handler_module": "caller.tools", "id": "notes"}),
    )
    .await;
}

/// `who` now holds exactly `verbs` on `key` (replacing the rig's grant).
fn hold(r: &Rig, who: &str, key: &str, verbs: Value) {
    let k = key.to_string();
    r.kernel
        .edit_can_call(&AgentId::from(who), move |m| {
            m.insert(k, verbs);
            true
        })
        .unwrap();
}

fn keys_of(r: &Rig, id: &str, field: &str) -> Vec<String> {
    let mut k: Vec<String> = r.meta(id)[field]
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    k.sort();
    k
}

#[tokio::test]
async fn spawn_opens_the_cards_tool_grants_on_both_ends() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp, &["lead"], &[("writer.md", WRITER)]).await;
    notes_tool(&r).await;
    let before = keys_of(&r, "notes", "callable_by");
    let id = id_of(&r.spawn_as(&r.callers[0], json!({"role": "writer"})).await);
    assert_eq!(r.meta(&id)["can_call"]["notes"], json!(["read"]));
    assert_eq!(r.meta("notes")["callable_by"][&id], json!(["read"]));
    let mut want = before;
    want.push(id);
    want.sort();
    assert_eq!(
        keys_of(&r, "notes", "callable_by"),
        want,
        "no other new key"
    );
}

#[tokio::test]
async fn spawned_worker_calls_the_granted_tool_without_directive_setup() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp, &["lead"], &[("writer.md", WRITER)]).await;
    notes_tool(&r).await;
    let id = id_of(&r.spawn_as(&r.callers[0], json!({"role": "writer"})).await);
    let ok = r.call(&id, "notes", json!({"type": "read"})).await;
    assert_eq!(ok["verb"], "read", "{ok}");
    let no = r.call(&id, "notes", json!({"type": "write"})).await;
    assert_eq!(no["reason"], "unauthorized", "{no}");
}

#[tokio::test]
async fn spawn_from_opens_the_cards_tool_grants_on_both_ends() {
    let tmp = TempDir::new().unwrap();
    let (r, src) = fork_rig(&tmp, &["lead"], &[("writer.md", WRITER)]).await;
    notes_tool(&r).await;
    hold(&r, &src, "notes", json!("*"));
    let before = keys_of(&r, "notes", "callable_by");
    let id = id_of(
        &r.spawn_as(
            &r.callers[0],
            json!({"role": "writer", "from": from(&src, 5)}),
        )
        .await,
    );
    assert_eq!(
        r.meta(&id)["can_call"]["notes"],
        json!(["read"]),
        "not wider than the card"
    );
    assert_eq!(r.meta("notes")["callable_by"][&id], json!(["read"]));
    let mut want = before;
    want.push(id.clone());
    want.sort();
    assert_eq!(keys_of(&r, "notes", "callable_by"), want);
}

#[tokio::test]
async fn spawn_tool_grant_with_star_verbs_keeps_star() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp, &["lead"], &[("star.md", STAR), ("writer.md", WRITER)]).await;
    notes_tool(&r).await;
    let a = id_of(&r.spawn_as(&r.callers[0], json!({"role": "star"})).await);
    let b = id_of(&r.spawn_as(&r.callers[0], json!({"role": "star"})).await);
    assert_eq!(r.meta("notes")["callable_by"][&a], json!("*"));
    assert_eq!(r.meta("notes")["callable_by"][&b], json!("*"));
    // a grant already there is merged into, never duplicated
    let w = id_of(&r.spawn_as(&r.callers[0], json!({"role": "writer"})).await);
    assert_eq!(r.meta("notes")["callable_by"][&w], json!(["read"]));
    assert_eq!(r.meta("notes")["callable_by"][&a], json!("*"));
}

#[tokio::test]
async fn spawn_without_the_tool_agent_still_spawns() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp, &["lead"], &[("writer.md", WRITER)]).await;
    let id = id_of(&r.spawn_as(&r.callers[0], json!({"role": "writer"})).await);
    assert_eq!(r.meta(&id)["can_call"]["notes"], json!(["read"]));
    assert!(r.kernel.agents.get(&AgentId::from("notes")).is_none());
}

#[tokio::test]
async fn refused_tool_grant_undoes_the_spawn() {
    let tmp = TempDir::new().unwrap();
    let picky = uid(&tmp, "zpicky");
    let card = format!(
        "---\nname: two\npurpose: Two tools.\ntier: fast\ngrants:\n  $registry: [roles, report]\n  notes: [read]\n  {picky}: [read]\n---\nx\n"
    );
    let r = rig(&tmp, &["lead"], &[("two.md", card.as_str())]).await;
    let lead = r.callers[0].clone();
    notes_tool(&r).await;
    create(
        &r.kernel,
        json!({"handler_module": "picky.tools", "id": picky, "refuse_grants": true}),
    )
    .await;
    hold(&r, &lead, &picky, json!("*"));
    let notes_before = r.meta("notes")["callable_by"].clone();
    let reg_before = keys_of(&r, &r.reg, "callable_by");
    let lead_before = keys_of(&r, &lead, "can_call");
    let agents_before = r.kernel.agents.len();
    let out = r
        .spawn_as(&lead, json!({"role": "two", "task": "go"}))
        .await;
    assert_eq!(out["reason"], "grant_failed", "{out}");
    assert_eq!(r.kernel.agents.len(), agents_before, "no child left");
    assert_eq!(r.op(json!({"type": "fleet"})).await["roles"], json!({}));
    assert_eq!(r.count(None).await["running"], 0);
    assert_eq!(
        r.meta("notes")["callable_by"],
        notes_before,
        "first edit undone"
    );
    assert_eq!(keys_of(&r, &r.reg, "callable_by"), reg_before);
    assert_eq!(keys_of(&r, &lead, "can_call"), lead_before);
}

#[tokio::test]
async fn delete_agent_removes_the_spawned_agents_tool_entries() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp, &["lead"], &[("writer.md", WRITER)]).await;
    notes_tool(&r).await;
    let lead = r.callers[0].clone();
    let keep = id_of(&r.spawn_as(&lead, json!({"role": "writer"})).await);
    let id = id_of(&r.spawn_as(&lead, json!({"role": "writer"})).await);
    assert!(r.meta("notes")["callable_by"].get(&id).is_some());
    let out = deleted(&r, &id).await;
    assert!(out.get("error").is_none(), "{out}");
    assert!(r.meta("notes")["callable_by"].get(&id).is_none());
    assert!(r.meta(&r.reg)["callable_by"].get(&id).is_none());
    assert!(r.meta(&lead)["can_call"].get(&id).is_none());
    assert_eq!(r.meta("notes")["callable_by"][&keep], json!(["read"]));
}

#[tokio::test]
async fn delete_of_a_subtree_removes_every_spawned_agents_tool_entries() {
    let tmp = TempDir::new().unwrap();
    let r = rig(
        &tmp,
        &["lead"],
        &[("sublead.md", SUBLEAD), ("writer.md", WRITER)],
    )
    .await;
    notes_tool(&r).await;
    let sub = id_of(&r.spawn_as(&r.callers[0], json!({"role": "sublead"})).await);
    let w = id_of(&r.spawn_as(&sub, json!({"role": "writer"})).await);
    assert!(r.meta("notes")["callable_by"].get(&sub).is_some());
    assert!(r.meta("notes")["callable_by"].get(&w).is_some());
    deleted(&r, &sub).await;
    assert!(r.meta("notes")["callable_by"].get(&sub).is_none());
    assert!(r.meta("notes")["callable_by"].get(&w).is_none());
}

#[tokio::test]
async fn spawn_refuses_a_grant_the_spawner_does_not_hold() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp, &["lead"], &[("wide.md", WIDE)]).await;
    notes_tool(&r).await;
    let lead = r.callers[0].clone();
    hold(&r, &lead, "notes", json!(["read"]));
    let notes_before = r.meta("notes")["callable_by"].clone();
    let n = r.kernel.agents.len();
    let out = r.spawn_as(&lead, json!({"role": "wide"})).await;
    assert_eq!(out["reason"], "not_allowed", "{out}");
    let why = out["error"].as_str().unwrap();
    assert!(
        why.contains(lead.as_str()) && why.contains("write") && why.contains("notes"),
        "{why}"
    );
    assert_eq!(r.kernel.agents.len(), n, "nothing created");
    assert_eq!(r.meta("notes")["callable_by"], notes_before);
    assert_eq!(r.op(json!({"type": "fleet"})).await["roles"], json!({}));
}

#[tokio::test]
async fn spawn_refuses_star_grant_when_spawner_holds_only_verbs() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp, &["lead"], &[("star.md", STAR)]).await;
    notes_tool(&r).await;
    hold(&r, &r.callers[0], "notes", json!(["read"]));
    let out = r.spawn_as(&r.callers[0], json!({"role": "star"})).await;
    assert_eq!(out["reason"], "not_allowed", "{out}");
}

#[tokio::test]
async fn spawn_refuses_a_tool_the_spawner_has_no_grant_on() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp, &["lead"], &[("writer.md", WRITER)]).await;
    notes_tool(&r).await;
    r.kernel
        .edit_can_call(&AgentId::from(r.callers[0].as_str()), |m| {
            m.remove("notes").is_some()
        })
        .unwrap();
    let out = r.spawn_as(&r.callers[0], json!({"role": "writer"})).await;
    assert_eq!(out["reason"], "not_allowed", "{out}");
}

#[tokio::test]
async fn spawn_from_refuses_escalation_before_forking() {
    let tmp = TempDir::new().unwrap();
    let (r, src) = fork_rig(&tmp, &["lead"], &[("wide.md", WIDE)]).await;
    notes_tool(&r).await;
    hold(&r, &r.callers[0], "notes", json!(["read"]));
    let out = r
        .spawn_as(
            &r.callers[0],
            json!({"role": "wide", "from": from(&src, 5)}),
        )
        .await;
    assert_eq!(out["reason"], "not_allowed", "{out}");
    assert!(forks_of(&r, &src).is_empty(), "nothing forked");
}

#[tokio::test]
async fn operator_spawn_has_no_escalation_check() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp, &[], &[("wide.md", WIDE)]).await;
    notes_tool(&r).await;
    let id = id_of(&r.op(json!({"type": "spawn", "role": "wide"})).await);
    assert_eq!(r.meta(&id)["can_call"]["notes"], json!(["read", "write"]));
    assert_eq!(
        r.meta("notes")["callable_by"][&id],
        json!(["read", "write"])
    );
}

#[tokio::test]
async fn subleads_cannot_hand_on_more_than_they_hold() {
    let tmp = TempDir::new().unwrap();
    let r = rig(
        &tmp,
        &["lead"],
        &[
            ("sublead.md", SUBLEAD),
            ("wide.md", WIDE),
            ("writer.md", WRITER),
        ],
    )
    .await;
    notes_tool(&r).await;
    let sub = id_of(&r.spawn_as(&r.callers[0], json!({"role": "sublead"})).await);
    let no = r.spawn_as(&sub, json!({"role": "wide"})).await;
    assert_eq!(no["reason"], "not_allowed", "{no}");
    let ok = r.spawn_as(&sub, json!({"role": "writer"})).await;
    assert!(ok.get("id").is_some(), "{ok}");
}

#[tokio::test]
async fn registry_token_grants_are_not_escalation_checked() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp, &["lead"], &[]).await;
    let lead = r.callers[0].clone();
    hold(&r, &lead, &r.reg, json!(["spawn"]));
    let out = r.spawn_as(&lead, json!({"role": "project_lead"})).await;
    assert!(out.get("id").is_some(), "{out}");
}

// ── every existing agent counts (#424) ──────────────────────────────

fn paused_ids() -> Vec<String> {
    PAUSED.lock().unwrap().clone()
}

async fn spawn_id(r: &Rig, who: &str, role: &str) -> String {
    let out = r.spawn_as(who, json!({"role": role})).await;
    out["id"]
        .as_str()
        .unwrap_or_else(|| panic!("{out}"))
        .to_string()
}

#[tokio::test]
async fn get_count_counts_all_existing_agents() {
    let tmp = TempDir::new().unwrap();
    let r = rig(
        &tmp,
        &["lead"],
        &[("writer.md", WRITER), ("reviewer.md", REVIEWER)],
    )
    .await;
    let lead = &r.callers[0];
    let a = spawn_id(&r, lead, "writer").await;
    let b = spawn_id(&r, lead, "writer").await;
    spawn_id(&r, lead, "reviewer").await;
    r.call(lead, &a, json!({"type": "pause"})).await;
    r.call(&b, &r.reg, json!({"type": "report", "state": "done"}))
        .await;
    assert_eq!(r.count(None).await["running"], 3);
    assert_eq!(r.count(Some("writer")).await["running"], 2);
}

#[tokio::test]
async fn paused_agent_keeps_its_slot_and_spawn_is_refused() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp, &["lead"], &[("writer.md", WRITER)]).await;
    let lead = &r.callers[0];
    r.op(json!({"type": "set_limit", "max": 2})).await;
    let a = spawn_id(&r, lead, "writer").await;
    spawn_id(&r, lead, "writer").await;
    r.call(lead, &a, json!({"type": "pause"})).await;
    let third = r.spawn_as(lead, json!({"role": "writer"})).await;
    assert_eq!(third["reason"], "limit", "{third}");
    assert_eq!(third["scope"], "total");
    assert_eq!(
        (third["running"].clone(), third["max"].clone()),
        (json!(2), json!(2))
    );
}

#[tokio::test]
async fn done_report_does_not_pause_the_sender() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp, &["lead"], &[("writer.md", WRITER)]).await;
    let lead = &r.callers[0];
    let a = spawn_id(&r, lead, "writer").await;
    for state in ["done", "failed"] {
        let rep = r
            .call(&a, &r.reg, json!({"type": "report", "state": state}))
            .await;
        assert_eq!(rep, json!({"ok": true}), "{rep}");
    }
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        !paused_ids().contains(&a),
        "no pause was sent to the reporter"
    );
    assert_eq!(r.count(None).await["running"], 1);
    let f = r.op(json!({"type": "fleet"})).await;
    assert_eq!(f["roles"]["writer"].as_array().unwrap().len(), 1, "{f}");
}

#[tokio::test]
async fn delete_agent_is_the_only_way_to_free_a_slot() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp, &["lead"], &[("writer.md", WRITER)]).await;
    let lead = &r.callers[0];
    r.op(json!({"type": "set_limit", "max": 2})).await;
    let a = spawn_id(&r, lead, "writer").await;
    spawn_id(&r, lead, "writer").await;
    r.call(lead, &a, json!({"type": "pause"})).await;
    r.call(&a, &r.reg, json!({"type": "report", "state": "done"}))
        .await;
    assert_eq!(
        r.spawn_as(lead, json!({"role": "writer"})).await["reason"],
        "limit"
    );
    send(
        &r.kernel,
        "kernel_state",
        json!({"type": "delete_agent", "id": a}),
    )
    .await;
    assert_eq!(r.count(None).await["running"], 1);
    assert!(r
        .spawn_as(lead, json!({"role": "writer"}))
        .await
        .get("id")
        .is_some());
    assert_eq!(r.count(None).await["running"], 2);
}

#[tokio::test]
async fn delegated_quota_counts_paused_and_done_agents() {
    let tmp = TempDir::new().unwrap();
    let r = rig(
        &tmp,
        &["boss"],
        &[("writer.md", WRITER), ("sublead.md", SUBLEAD)],
    )
    .await;
    let boss = &r.callers[0];
    // total quota 1: a paused sub-agent keeps the only slot
    let sub = r
        .spawn_as(boss, json!({"role": "sublead", "limits": {"total": 1}}))
        .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let w = spawn_id(&r, &sub, "writer").await;
    r.call(boss, &w, json!({"type": "pause"})).await;
    let no = r.spawn_as(&sub, json!({"role": "writer"})).await;
    assert_eq!(no["reason"], "limit", "{no}");
    assert_eq!(no["scope"], "subtree_total");
    // per-role quota: a done sub-agent keeps its role slot
    let sub2 = r
        .spawn_as(boss, json!({"role": "sublead", "limits": {"per_role": 1}}))
        .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let w2 = spawn_id(&r, &sub2, "writer").await;
    r.call(&w2, &r.reg, json!({"type": "report", "state": "done"}))
        .await;
    let no2 = r.spawn_as(&sub2, json!({"role": "writer"})).await;
    assert_eq!(no2["reason"], "limit", "{no2}");
    assert_eq!(no2["scope"], "subtree_role");
}

#[tokio::test]
async fn get_count_ignores_a_resume_arg_and_registry_grants_only_report() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp, &["lead"], &[("writer.md", WRITER)]).await;
    let lead = &r.callers[0];
    let a = spawn_id(&r, lead, "writer").await;
    // the old per-agent check is gone: an unknown `resume` arg is just ignored
    let plain = r.count(None).await;
    let with = r.op(json!({"type": "get_count", "resume": a})).await;
    assert_eq!(with, plain);
    assert_eq!(with, json!({"running": 1, "max": null}));
    let nope = r
        .op(json!({"type": "get_count", "resume": "no_such_agent"}))
        .await;
    assert_eq!(nope, plain);
    // the registry is only let in for `report`
    assert_eq!(r.meta(&a)["can_call"][&r.reg], json!(["roles", "report"]));
    assert_eq!(
        r.meta(&r.reg)["callable_by"][&a],
        json!(["roles", "report"])
    );
}

// ── the refusal shape: `error` is a sentence, `reason` is the code ──────────

/// A refusal in the kernel's shape: `error` a sentence (never the bare code),
/// `reason` the code.
fn assert_shape(out: &Value, code: &str) {
    assert_eq!(out["reason"], code, "{out}");
    let sentence = out["error"].as_str().unwrap_or_else(|| panic!("{out}"));
    assert!(sentence.contains(' ') && sentence != code, "{out}");
}

#[tokio::test]
async fn limit_refusal_error_is_a_sentence_reason_is_limit() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp, &["lead"], &[("writer.md", WRITER)]).await;
    let lead = &r.callers[0];
    r.op(json!({"type": "set_limit", "role": "writer", "max": 1}))
        .await;
    r.spawn_as(lead, json!({"role": "writer"})).await;
    let out = r.spawn_as(lead, json!({"role": "writer"})).await;
    assert_shape(&out, "limit");
    assert!(out["error"].as_str().unwrap().contains("is at its limit"));
    assert_ne!(out["error"], "limit");
    assert!(!out["hint"].as_str().unwrap().is_empty(), "{out}");
    assert_eq!(out["scope"], "role");
    assert_eq!(out["role"], "writer");
    assert_eq!(out["running"], 1);
    assert_eq!(out["max"], 1);
}

#[tokio::test]
async fn limit_refusal_total_and_subtree_scopes_keep_extras() {
    let tmp = TempDir::new().unwrap();
    let r = rig(
        &tmp,
        &["boss"],
        &[("writer.md", WRITER), ("sublead.md", SUBLEAD)],
    )
    .await;
    let boss = &r.callers[0];
    // total
    r.op(json!({"type": "set_limit", "max": 1})).await;
    r.spawn_as(boss, json!({"role": "writer"})).await;
    let total = r.spawn_as(boss, json!({"role": "writer"})).await;
    assert_shape(&total, "limit");
    assert_eq!(total["scope"], "total");
    assert!(total.get("lead").is_none(), "{total}");
    r.op(json!({"type": "set_limit", "max": null})).await;
    // request: more than the sub-lead was given
    let sub = id_of(
        &r.spawn_as(boss, json!({"role": "sublead", "limits": {"total": 2}}))
            .await,
    );
    let req = r
        .spawn_as(&sub, json!({"role": "sublead", "limits": {"total": 5}}))
        .await;
    assert_shape(&req, "limit");
    assert_eq!(req["scope"], "request");
    assert_eq!(req["lead"], sub.as_str());
    // subtree_total
    r.spawn_as(&sub, json!({"role": "writer"})).await;
    r.spawn_as(&sub, json!({"role": "writer"})).await;
    let st = r.spawn_as(&sub, json!({"role": "writer"})).await;
    assert_shape(&st, "limit");
    assert_eq!(st["scope"], "subtree_total");
    assert_eq!(st["lead"], sub.as_str());
    // subtree_role
    let sub2 = id_of(
        &r.spawn_as(boss, json!({"role": "sublead", "limits": {"per_role": 1}}))
            .await,
    );
    r.spawn_as(&sub2, json!({"role": "writer"})).await;
    let sr = r.spawn_as(&sub2, json!({"role": "writer"})).await;
    assert_shape(&sr, "limit");
    assert_eq!(sr["scope"], "subtree_role");
    assert_eq!(sr["lead"], sub2.as_str());
}

#[tokio::test]
async fn not_allowed_refusal_reason_is_code() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp, &["lead"], &[("wide.md", WIDE)]).await;
    notes_tool(&r).await;
    let lead = r.callers[0].clone();
    hold(&r, &lead, "notes", json!(["read"]));
    let out = r.spawn_as(&lead, json!({"role": "wide"})).await;
    assert_shape(&out, "not_allowed");
    let why = out["error"].as_str().unwrap();
    assert!(
        why.contains(lead.as_str()) && why.contains("write") && why.contains("notes"),
        "{out}"
    );
    assert!(out["hint"].is_string(), "{out}");
}

#[tokio::test]
async fn fork_not_allowed_refusal_reason_is_code() {
    let tmp = TempDir::new().unwrap();
    let (r, src) = fork_rig(&tmp, &["lead", "other"], &[("writer.md", WRITER)]).await;
    let other = r.callers[1].clone();
    r.kernel
        .edit_can_call(&AgentId::from(other.as_str()), |m| m.remove(&src).is_some())
        .unwrap();
    let out = r
        .spawn_as(&other, json!({"role": "writer", "from": from(&src, 5)}))
        .await;
    assert_shape(&out, "not_allowed");
    assert!(out["error"].as_str().unwrap().contains("fork"), "{out}");
}

#[tokio::test]
async fn grant_failed_refusal_reason_is_code() {
    let tmp = TempDir::new().unwrap();
    let picky = uid(&tmp, "zpicky");
    let card = format!(
        "---\nname: two\npurpose: Two tools.\ntier: fast\ngrants:\n  $registry: [roles, report]\n  notes: [read]\n  {picky}: [read]\n---\nx\n"
    );
    let r = rig(&tmp, &["lead"], &[("two.md", card.as_str())]).await;
    let lead = r.callers[0].clone();
    notes_tool(&r).await;
    create(
        &r.kernel,
        json!({"handler_module": "picky.tools", "id": picky, "refuse_grants": true}),
    )
    .await;
    hold(&r, &lead, &picky, json!("*"));
    let before = r.kernel.agents.len();
    let out = r.spawn_as(&lead, json!({"role": "two"})).await;
    assert_shape(&out, "grant_failed");
    assert!(
        out["error"].as_str().unwrap().starts_with("spawn:"),
        "{out}"
    );
    assert!(out["hint"].is_string(), "{out}");
    assert_eq!(r.kernel.agents.len(), before, "nothing left behind");
    assert_eq!(r.count(None).await["running"], 0);
}

#[tokio::test]
async fn unknown_tier_refusal_keeps_tier_extra() {
    let tmp = TempDir::new().unwrap();
    let card = "---\nname: odd\npurpose: Odd.\ntier: nowhere\n---\nd\n";
    let r = rig(&tmp, &["lead"], &[("odd.md", card)]).await;
    let out = r.spawn_as(&r.callers[0], json!({"role": "odd"})).await;
    assert_shape(&out, "unknown_tier");
    assert_eq!(out["tier"], "nowhere");
}

#[tokio::test]
async fn bad_tier_refusal_reason_is_code() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp, &["lead"], &[("writer.md", WRITER)]).await;
    let mut patch = Map::new();
    patch.insert("tiers".to_string(), json!({"fast": {"model": "m"}}));
    r.kernel.update(&AgentId::from(r.reg.as_str()), patch);
    let out = r.spawn_as(&r.callers[0], json!({"role": "writer"})).await;
    assert_shape(&out, "bad_tier");
}

#[tokio::test]
async fn not_spawned_refusal_reason_is_code() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp, &["lead", "other"], &[("writer.md", WRITER)]).await;
    let other = &r.callers[1];
    // not spawned by anyone: the operator has no origin agent
    let op = r.op(json!({"type": "report", "state": "done"})).await;
    assert_shape(&op, "not_spawned");
    // an agent that exists but was not spawned by this registry
    let no = r
        .call(other, &r.reg, json!({"type": "report", "state": "done"}))
        .await;
    assert_shape(&no, "not_spawned");
    assert!(no["hint"].is_string(), "{no}");
}

#[test]
fn no_root_refusal_reason_is_code() {
    // the kernel always has a root in a test rig, so assert the helper's shape
    let out = fail(refusal::NO_ROOT, "the kernel has no root".to_string());
    assert_shape(&out, "no_root");
}

#[tokio::test]
async fn disabled_refusal_has_reason_disabled() {
    let tmp = TempDir::new().unwrap();
    let kernel = Arc::new(new_kernel());
    root_of(&kernel, &tmp);
    let id = uid(&tmp, "ar");
    create(&kernel, json!({"handler_module": HANDLER_MODULE, "id": id})).await;
    let out = send(&kernel, &id, json!({"type": "roles"})).await;
    assert_eq!(out["reason"], "disabled", "{out}");
    assert!(out["error"]
        .as_str()
        .unwrap()
        .contains("agentic_resources is disabled"));
    assert!(out["hint"].is_string(), "{out}");
}

#[test]
fn every_refusal_code_is_in_the_closed_set() {
    for code in [
        refusal::LIMIT,
        refusal::NOT_ALLOWED,
        refusal::GRANT_FAILED,
        refusal::UNKNOWN_TIER,
        refusal::BAD_TIER,
        refusal::NO_ROOT,
        refusal::NOT_SPAWNED,
        refusal::DISABLED,
    ] {
        assert!(refusal::CODES.contains(&code), "{code}");
        assert_eq!(refusal::refusal(code, "x")["reason"], code);
    }
}
