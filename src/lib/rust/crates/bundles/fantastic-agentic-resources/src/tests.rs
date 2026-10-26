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
            other => Ok(Some(json!({"ok": true, "verb": other}))),
        }
    }
}

const WRITER: &str = "---\nname: writer\npurpose: Writes the first draft of a text.\ntier: fast\nmodel: from-the-card\ngrants:\n  $registry: [roles, report]\n  notes: [read]\n---\nYou write clearly and briefly.\n";
const REVIEWER: &str = "---\nname: reviewer\npurpose: Reads a draft and says what to fix.\ntier: fast\n---\nYou review drafts.\n";
const SUBLEAD: &str = "---\nname: sublead\npurpose: Runs a branch of the work.\ntier: fast\ngrants:\n  $registry: [roles, spawn, get_count, report]\n---\nYou run one branch.\n";

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
                   "can_call": {reg.clone(): "*"}}),
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
    assert_eq!(out["error"], "unknown_tier", "{out}");
    assert!(
        out["reason"].as_str().unwrap().contains("'nowhere'"),
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
    assert_eq!(out["error"], "limit", "{out}");
    assert_eq!(out["scope"], "role");
    assert_eq!(out["role"], "writer");
    assert_eq!(out["running"], 2);
    assert_eq!(out["max"], 2);
    assert!(out["reason"].as_str().unwrap().contains("writer"));
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
    assert_eq!(out["error"], "limit", "{out}");
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
        r.spawn_as(lead, json!({"role": "writer"})).await["error"],
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
    assert_eq!(f["roles"]["writer"][0]["state"], "running");
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
            json!({"error": "agentic_resources is disabled"}),
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
async fn spawn_task_line_stored() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp, &["lead"], &[("writer.md", WRITER)]).await;
    let lead = &r.callers[0];
    let a = r
        .spawn_as(
            lead,
            json!({"role": "writer", "task": {"label": "draft intro"}}),
        )
        .await;
    assert_eq!(
        r.meta(a["id"].as_str().unwrap())["task"],
        json!({"label": "draft intro", "state": "running", "waits_on": [], "result": null})
    );
    let b = r
        .spawn_as(
            lead,
            json!({"role": "writer", "task": {"label": "polish", "waits_on": [a["id"]]}}),
        )
        .await;
    let t = r.meta(b["id"].as_str().unwrap())["task"].clone();
    assert_eq!(t["state"], "waiting");
    assert_eq!(t["waits_on"], json!([a["id"]]));
    assert_eq!(t["result"], Value::Null);
    // a task with no label is refused; a string task is a first message, not a line
    assert_eq!(
        r.spawn_as(lead, json!({"role": "writer", "task": {}}))
            .await["reason"],
        "bad_arg"
    );
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
async fn report_sets_state_and_messages_parent() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp, &["lead", "other"], &[("writer.md", WRITER)]).await;
    let (lead, other) = (&r.callers[0], &r.callers[1]);
    let mut inbox = r.kernel.ensure_inbox(&AgentId::from(lead.as_str()));
    let id = r
        .spawn_as(lead, json!({"role": "writer", "task": {"label": "x"}}))
        .await["id"]
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
    let t = r.meta(&id)["task"].clone();
    assert_eq!(
        (t["state"].clone(), t["result"].clone()),
        (json!("done"), json!({"words": 120}))
    );
    assert_eq!(t["label"], "x");
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
    assert_eq!(no["error"], "not_spawned", "{no}");
    assert_eq!(
        r.op(json!({"type": "report", "state": "done"})).await["error"],
        "not_spawned"
    );
    let bad = r
        .call(&id, &r.reg, json!({"type": "report", "state": "finished"}))
        .await;
    assert_eq!(bad["reason"], "bad_arg", "{bad}");
    assert_eq!(r.meta(&id)["task"]["state"], "done");
}

fn find_node<'a>(v: &'a Value, id: &str) -> Option<&'a Value> {
    if v.get("id").and_then(Value::as_str) == Some(id) {
        return Some(v);
    }
    match v {
        Value::Object(m) => m.values().find_map(|c| find_node(c, id)),
        Value::Array(a) => a.iter().find_map(|c| find_node(c, id)),
        _ => None,
    }
}

#[tokio::test]
async fn task_tree_visible_in_reflect_tree_all() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp, &["lead"], &[("writer.md", WRITER)]).await;
    let lead = &r.callers[0];
    let a = r
        .spawn_as(lead, json!({"role": "writer", "task": {"label": "intro"}}))
        .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let b = r
        .spawn_as(
            lead,
            json!({"role": "writer", "task": {"label": "body", "waits_on": [a.clone()]}}),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let tree = send(
        &r.kernel,
        "kernel_state",
        json!({"type": "reflect", "tree": "all"}),
    )
    .await;
    let lead_node = find_node(&tree, lead).expect("lead in the tree");
    let na = find_node(lead_node, &a).expect("child a under the lead");
    let nb = find_node(lead_node, &b).expect("child b under the lead");
    let line = |n: &Value| n.get("task").cloned();
    assert_eq!(line(na).unwrap()["label"], "intro", "{na}");
    assert_eq!(line(nb).unwrap()["state"], "waiting");
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
        (big["error"].clone(), big["scope"].clone()),
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
    assert_eq!(third["error"], "limit", "{third}");
    assert_eq!(third["scope"], "subtree_total");
    assert_eq!(
        (third["running"].clone(), third["max"].clone()),
        (json!(2), json!(2))
    );
    // the deepest sub-lead was given 1: it cannot spawn past that either, and the
    // ancestors' quota is the bound of the whole subtree
    let deep = ok["id"].as_str().unwrap();
    let blocked = r.spawn_as(deep, json!({"role": "writer"})).await;
    assert_eq!(blocked["error"], "limit", "{blocked}");
    // the boss itself carries no quota
    assert!(r
        .spawn_as(boss, json!({"role": "writer"}))
        .await
        .get("id")
        .is_some());
}

#[tokio::test]
async fn get_count_counts_active_only() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp, &["lead"], &[("writer.md", WRITER)]).await;
    let lead = &r.callers[0];
    r.op(json!({"type": "set_limit", "max": 2})).await;
    let a = r
        .spawn_as(lead, json!({"role": "writer", "task": {"label": "a"}}))
        .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let b = r
        .spawn_as(
            lead,
            json!({"role": "writer", "task": {"label": "b", "waits_on": [a.clone()]}}),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        r.count(None).await["running"],
        2,
        "running and waiting both count"
    );
    assert_eq!(
        r.spawn_as(lead, json!({"role": "writer"})).await["error"],
        "limit"
    );
    r.call(&a, &r.reg, json!({"type": "report", "state": "paused"}))
        .await;
    assert_eq!(r.count(None).await, json!({"running": 1, "max": 2}));
    assert!(
        r.spawn_as(lead, json!({"role": "writer"}))
            .await
            .get("id")
            .is_some(),
        "a freed slot"
    );
    r.call(&b, &r.reg, json!({"type": "report", "state": "done"}))
        .await;
    assert_eq!(r.count(None).await["running"], 1);
    r.call(&a, &r.reg, json!({"type": "report", "state": "running"}))
        .await;
    assert_eq!(
        r.count(None).await["running"],
        2,
        "resumed work uses a slot again"
    );
}

#[tokio::test]
async fn no_auto_cleanup_on_done() {
    let tmp = TempDir::new().unwrap();
    let r = rig(&tmp, &["lead"], &[("writer.md", WRITER)]).await;
    let lead = &r.callers[0];
    let a = r
        .spawn_as(lead, json!({"role": "writer", "task": {"label": "a"}}))
        .await["id"]
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
    assert_eq!(
        (m["task"]["label"].clone(), m["task"]["state"].clone()),
        (json!("a"), json!("done"))
    );
    assert_eq!(r.count(None).await["running"], 0, "the count dropped");
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
