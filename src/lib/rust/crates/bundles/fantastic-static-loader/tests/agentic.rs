//! THE AGENTIC RUNNER — rust's implementation over the SHARED corpus.
//!
//! Reads `contract/agentic/{configs,specs}` at the repo root, the same files
//! python's runner reads (`src/lib/python/tests/agentic/_engine.py`) and swift's
//! will. A scenario is DATA; the runner is the only per-runtime part.
//!
//! `$root` in a config or spec means THIS runtime's root — `kernel_state` here,
//! `kernel_state` in python. Parity asserts the per-runtime value, never
//! equality.

use fantastic_file::FileBundle;
use fantastic_kernel::{with_operator, with_sender, Agent, AgentId, Item, Kernel};
use fantastic_static_loader::{StaticLoaderBundle, HANDLER_MODULE};
use fantastic_text_file::TextFileBundle;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// THIS runtime's root id.
const ROOT: &str = "kernel_state";

fn contract_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(6)
        .expect("repo root above src/lib/rust/crates/bundles/<crate>")
        .join("contract")
}

fn yaml_to_json(v: serde_yaml::Value) -> Value {
    serde_json::to_value(v).expect("yaml -> json")
}

/// `$root` anywhere — key or value — becomes this runtime's root id.
fn subst_root(v: &Value) -> Value {
    match v {
        Value::String(s) if s == "$root" => json!(ROOT),
        Value::Array(a) => Value::Array(a.iter().map(subst_root).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, x)| {
                    let key = if k == "$root" {
                        ROOT.to_string()
                    } else {
                        k.clone()
                    };
                    (key, subst_root(x))
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

struct Scenario {
    name: String,
    spec: Value,
    config: Value,
}

fn load_scenarios() -> Vec<Scenario> {
    let specs_dir = contract_dir().join("agentic/specs");
    let mut paths: Vec<_> = std::fs::read_dir(&specs_dir)
        .unwrap_or_else(|e| panic!("no corpus at {specs_dir:?}: {e}"))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "yaml").unwrap_or(false))
        .collect();
    paths.sort();
    let mut out = Vec::new();
    for p in paths {
        let spec = subst_root(&yaml_to_json(
            serde_yaml::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap(),
        ));
        let cfg_name = spec["config"].as_str().unwrap_or("").to_string();
        let cfg_path = contract_dir().join("agentic/configs").join(&cfg_name);
        let config = subst_root(&yaml_to_json(
            serde_yaml::from_str(&std::fs::read_to_string(&cfg_path).unwrap()).unwrap(),
        ));
        out.push(Scenario {
            name: p.file_stem().unwrap().to_string_lossy().to_string(),
            spec,
            config,
        });
    }
    out
}

/// The probe the cancel scenarios need: a verb that stays IN FLIGHT until it is
/// cancelled. No shipped bundle offers one without a subprocess or a socket, and
/// inventing a kernel mechanism to make a test expressible is not on — so each
/// runtime ships this same small probe, exactly as each ships the C21 echo.
struct LifecycleProbe;

#[async_trait::async_trait]
impl fantastic_kernel::bundle::Bundle for LifecycleProbe {
    fn name(&self) -> &str {
        "lifecycle_probe"
    }
    async fn handle(
        &self,
        id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<fantastic_kernel::bundle::Reply, fantastic_kernel::bundle::BundleError> {
        match payload.get("type").and_then(Value::as_str).unwrap_or("") {
            "boot" | "shutdown" => Ok(None),
            "reflect" => Ok(Some(json!({
                "id": id.0,
                "sentence": "a probe agent whose `slow` verb stays in flight",
                "verbs": {"slow": "no args. Blocks until cancelled."},
            }))),
            "slow" => {
                // Long enough that the cancel always wins the race; the step
                // that kills it does not wait this out.
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                Ok(Some(json!({"finished": true})))
            }
            other => Ok(Some(json!({
                "error": format!("lifecycle_probe: unknown type {other:?}"),
                "reason": "unhandled",
                "verb": other,
            }))),
        }
    }
}

fn mk_kernel() -> Arc<Kernel> {
    // DISK mode, rooted at the running dir. `load` computes each restored
    // agent's root_path from `storage.workdir()`, so an in-memory kernel hands
    // every agent an empty path and `find_store` can never match a declared
    // provider again — the tree comes back but its STORE does not.
    let mut kernel = Kernel::with_storage(fantastic_kernel::StorageMode::Disk(
        std::env::current_dir().unwrap(),
    ));
    kernel.bundles.register(HANDLER_MODULE, StaticLoaderBundle);
    kernel
        .bundles
        .register("lifecycle_probe.tools", LifecycleProbe);
    kernel.bundles.register("file_bridge.tools", FileBundle);
    kernel
        .bundles
        .register(fantastic_text_file::HANDLER_MODULE, TextFileBundle);
    kernel
        .bundles
        .register("exec_bridge.tools", fantastic_exec_bridge::ExecBridgeBundle);
    let kernel = Arc::new(kernel);
    // The root's root_path IS the store root the configs declare (`.fantastic`).
    // `find_store` matches a candidate provider's `root` meta against THIS path,
    // so a bare cwd here would make every declared store undiscoverable and the
    // whole tree would silently stay in RAM.
    let root = Agent::new(
        AgentId::from(ROOT),
        None,
        None,
        Map::new(),
        std::env::current_dir().unwrap().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    kernel
}

struct Hydration {
    kernel: Arc<Kernel>,
    _fixture: tempfile::TempDir,
    creates: BTreeMap<String, Value>,
    /// The EXPLICIT boot reply per loader. `create_agent` boots its child and
    /// SWALLOWS the exception, so a manifest that parses but cannot build is
    /// invisible on the create — an explicit boot is the only loud channel.
    boots: BTreeMap<String, Value>,
}

/// Write each loader's manifest and create the loader — `create_agent` boots it,
/// which hydrates the roster.
///
/// The manifest lands UNDER THE RUNNING DIR: `fs` is the one disk surface and a
/// cold bring-up read is not exempt, so a manifest under `/tmp` is refused.
async fn hydrate(sc: &Scenario) -> Hydration {
    let kernel = mk_kernel();
    let fixture = tempfile::Builder::new()
        .prefix(".agentic_")
        .tempdir_in(std::env::current_dir().unwrap())
        .unwrap();
    let mut creates = BTreeMap::new();
    let mut boots = BTreeMap::new();

    for loader in sc.config["loaders"].as_array().cloned().unwrap_or_default() {
        let lid = loader["id"].as_str().unwrap_or("loader").to_string();
        let file = loader["file"].as_str().unwrap_or("fantastic.yaml");
        let path = fixture.path().join(file);
        if let Some(manifest) = loader.get("manifest") {
            std::fs::write(&path, serde_yaml::to_string(manifest).unwrap()).unwrap();
        } else if let Some(text) = loader.get("manifest_text").and_then(Value::as_str) {
            std::fs::write(&path, text).unwrap();
        }
        // `missing_manifest: true` writes NOTHING — the loader points at a path
        // that is not there, and refusing that is the assertion.
        let rel = path
            .strip_prefix(std::env::current_dir().unwrap())
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();

        let mut payload = json!({
            "type": "create_agent",
            "parent_id": "kernel",
            "handler_module": HANDLER_MODULE,
            "id": lid,
            "manifest": rel,
        });
        if let Some(meta) = loader.get("meta").and_then(Value::as_object) {
            for (k, v) in meta {
                payload[k] = v.clone();
            }
        }
        let rec = kernel
            .send(vec![Item::new(AgentId::from(ROOT), payload)])
            .await
            .remove(0)
            .result;
        let built = rec.get("error").is_none();
        creates.insert(lid.clone(), rec);
        if built {
            let boot = operator(&kernel, &lid, json!({"type": "boot"})).await;
            boots.insert(lid, boot);
        }
    }
    Hydration {
        kernel,
        _fixture: fixture,
        creates,
        boots,
    }
}

/// Send AS an agent — GATED. The only evidence about reach: outside a dispatch
/// there is no sender, which makes a bare send an OPERATOR send and therefore
/// worthless as evidence about an agent's authority.
async fn reach(kernel: &Arc<Kernel>, sender: &str, target: &str, payload: Value) -> Value {
    let k = Arc::clone(kernel);
    let t = target.to_string();
    with_sender(AgentId::from(sender), async move {
        dispatch(&k, &t, payload).await
    })
    .await
}

async fn operator(kernel: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    let k = Arc::clone(kernel);
    let t = target.to_string();
    with_operator(async move { dispatch(&k, &t, payload).await }).await
}

/// One dispatch, CHANNEL CHOSEN BY VERB. The stream verbs
/// (`read_stream`/`write_stream`) carry RAW BYTES a JSON envelope cannot hold,
/// so they ride the kernel's BINARY channel; everything else is a plain send.
/// `bytes_utf8` in a write spec becomes the outbound blob; a read reply folds
/// its returned blob back into `bytes` (utf-8) so the `contains:{bytes:…}`
/// matcher sees streamed content — the mirror of python `_engine`'s
/// `_decode_bytes_fields` + its bytes-path decode. Runs under whatever
/// sender/operator context the caller set, so `send_with_binary`'s gate applies.
async fn dispatch(k: &Arc<Kernel>, target: &str, mut payload: Value) -> Value {
    // Binary channel when the payload carries content (`bytes_utf8` — the
    // text_file byte verbs: write/patch/append) or is a stream verb; the
    // mirror of python, where real bytes ride the payload in-process.
    let is_stream = matches!(
        payload.get("type").and_then(Value::as_str),
        Some("read_stream" | "write_stream")
    ) || payload.get("bytes_utf8").is_some();
    if !is_stream {
        return k
            .send(vec![Item::new(AgentId::from(target), payload)])
            .await
            .remove(0)
            .result;
    }
    let blob = payload
        .as_object_mut()
        .and_then(|o| o.remove("bytes_utf8"))
        .and_then(|v| v.as_str().map(|s| s.as_bytes().to_vec()))
        .unwrap_or_default();
    let (mut reply, body) = k
        .send_with_binary(&AgentId::from(target), payload, blob)
        .await;
    if let Some(body) = body {
        if let Some(o) = reply.as_object_mut() {
            o.insert(
                "bytes".into(),
                Value::String(String::from_utf8_lossy(&body).into_owned()),
            );
        }
    }
    reply
}

/// The ONE machine-readable question about a reply. Never `"error" in r`: a
/// bundle error and a gate denial are different animals, and only the gate sets
/// `reason: unauthorized`.
fn denied(reply: &Value) -> bool {
    reply.get("reason").and_then(Value::as_str) == Some("unauthorized")
}

fn denial_side(reply: &Value) -> Option<&str> {
    reply.get("side").and_then(Value::as_str)
}

/// Resolve a dotted path, `*` meaning "every element of this list".
fn dig<'a>(v: &'a Value, path: &str) -> Vec<&'a Value> {
    let mut cur = vec![v];
    for part in path.split('.') {
        let mut next = Vec::new();
        for c in cur {
            if part == "*" {
                if let Some(a) = c.as_array() {
                    next.extend(a.iter());
                }
            } else if let Some(x) = c.get(part) {
                next.push(x);
            }
        }
        cur = next;
    }
    cur
}

// ─── the step executor ──────────────────────────────────────────

/// Substitute `$name.field` references bound by earlier steps. A `start` binds
/// its task, so a later `cancel` can name the pid the KERNEL minted — without
/// bindings a spec could not name what it had just created.
fn resolve(v: &Value, bound: &BTreeMap<String, Value>) -> Value {
    match v {
        Value::String(s) if s.starts_with('$') => {
            let refr = &s[1..];
            let (name, field) = refr.split_once('.').unwrap_or((refr, ""));
            let target = bound
                .get(name)
                .unwrap_or_else(|| panic!("${refr}: nothing bound under {name:?}"));
            if field.is_empty() {
                return target.clone();
            }
            dig(target, field)
                .first()
                .cloned()
                .cloned()
                .unwrap_or_else(|| panic!("${refr}: {field:?} not in {target}"))
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| resolve(x, bound)).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, x)| (k.clone(), resolve(x, bound)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Python-parity value equality for the corpus matchers. JSON separates bool
/// from number, but the specs are authored against python, where `True == 1`
/// and `False == 0`. A spec that expects `written: true` means "a truthy count
/// came back" and passes in python via that equivalence; mirror it (true↔1,
/// false↔0) so the shared DATA reads identically here. Otherwise plain `==`.
fn py_eq(g: &Value, want: &Value) -> bool {
    if g == want {
        return true;
    }
    match (g, want) {
        (Value::Number(n), Value::Bool(b)) | (Value::Bool(b), Value::Number(n)) => {
            n.as_i64() == Some(i64::from(*b))
        }
        _ => false,
    }
}

/// Apply one step's matchers. Panics naming the step, the matcher and the reply
/// — a spec failure has to be readable without opening the runner.
fn match_step(name: &str, expect: &Value, reply: &Value) {
    let fail = |why: String| -> ! {
        panic!("{name}: {why}\n  reply: {reply}");
    };
    if let Some(allow) = expect.get("allow").and_then(Value::as_bool) {
        if allow && denied(reply) {
            fail("expected the edge to be OPEN, it was denied".into());
        }
        if !allow && !denied(reply) {
            fail("expected a DENIAL, the call went through".into());
        }
    }
    match expect.get("side") {
        // `side: null` — the refusal must carry NO side. That is what tells an
        // io SEAL from the call gate: both answer `unauthorized`, but only the
        // gate names a half, because only the gate is about the caller.
        Some(Value::Null) => {
            if let Some(got) = denial_side(reply) {
                fail(format!("expected NO side (an io seal), got {got:?}"));
            }
        }
        Some(Value::String(side)) if denial_side(reply) != Some(side.as_str()) => {
            fail(format!(
                "expected the {side:?} half to refuse, got {:?}",
                denial_side(reply)
            ));
        }
        _ => {}
    }
    if let Some(reason) = expect.get("reason").and_then(Value::as_str) {
        if reply.get("reason").and_then(Value::as_str) != Some(reason) {
            fail(format!("expected reason {reason:?}"));
        }
    }
    if let Some(want) = expect.get("status").and_then(Value::as_str) {
        if reply.get("status").and_then(Value::as_str) != Some(want) {
            fail(format!("expected status {want:?}"));
        }
    }
    if let Some(sub) = expect.get("error_contains").and_then(Value::as_str) {
        let err = reply.get("error").and_then(Value::as_str).unwrap_or("");
        if !err.contains(sub) {
            fail(format!("error does not contain {sub:?}"));
        }
    }
    if let Some(m) = expect.get("contains").and_then(Value::as_object) {
        for (path, want) in m {
            if !dig(reply, path).into_iter().any(|g| py_eq(g, want)) {
                fail(format!("{path} does not contain {want}"));
            }
        }
    }
    if let Some(m) = expect.get("not_contains").and_then(Value::as_object) {
        for (path, unwanted) in m {
            if dig(reply, path).into_iter().any(|g| py_eq(g, unwanted)) {
                fail(format!("{path} contains {unwanted} and must not"));
            }
        }
    }
}

/// `grant`/`revoke` — sugar over an OPERATOR `update_agent`.
///
/// Sugar, not a shortcut: `update_agent` REPLACES a field rather than merging it
/// (#6), so a spec that hand-wrote the payload would restate the whole map every
/// time and would silently drop the structural baseline off the live record.
async fn grant_step(h: &Hydration, args: &Value, remove: bool) -> Value {
    let agent_id = args["agent"].as_str().unwrap();
    let field = args["field"].as_str().unwrap();
    let other = args["to"].as_str().unwrap();
    let agent = h
        .kernel
        .agents
        .get(&AgentId::from(agent_id))
        .map(|e| Arc::clone(&e))
        .unwrap_or_else(|| panic!("grant: no agent {agent_id:?}"));
    let mut current = agent
        .meta
        .read()
        .expect("meta poisoned")
        .get(field)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if remove {
        current.remove(other);
    } else {
        let verbs = args.get("verbs").cloned().unwrap_or_else(|| json!(["*"]));
        current.insert(other.to_string(), verbs);
    }
    operator(
        &h.kernel,
        "kernel",
        json!({"type": "update_agent", "id": agent_id, field: Value::Object(current)}),
    )
    .await
}

/// Read the RUNNING system back: ids and parentage, one walk from the root.
fn dehydrate(kernel: &Arc<Kernel>) -> (Vec<String>, BTreeMap<String, Vec<String>>) {
    let mut ids = Vec::new();
    let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut queue = vec![kernel.root().expect("no root").id.clone()];
    while let Some(id) = queue.pop() {
        let Some(a) = kernel.agents.get(&id).map(|e| Arc::clone(&e)) else {
            continue;
        };
        ids.push(id.0.clone());
        let kids: Vec<String> = a.child_ids().into_iter().map(|c| c.0).collect();
        children.insert(id.0.clone(), kids.clone());
        queue.extend(kids.into_iter().map(|k| AgentId::from(k.as_str())));
    }
    ids.sort();
    (ids, children)
}

/// The manifest entries by id — the DECLARATION, never evidence about the
/// running system.
fn manifest_specs(config: &Value) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    for l in config["loaders"].as_array().cloned().unwrap_or_default() {
        for a in l["manifest"]["agents"]
            .as_array()
            .cloned()
            .unwrap_or_default()
        {
            if let Some(id) = a["id"].as_str() {
                out.insert(id.to_string(), a.clone());
            }
        }
    }
    out
}

/// The verbs `spec` declares for `other`. A `"*"` is REFUSED rather than
/// interpreted: a wildcard makes the per-verb intersection unobservable, and
/// that intersection is the whole point of the walk.
fn declared_verbs(spec: &Value, field: &str, other: &str) -> std::collections::BTreeSet<String> {
    match spec.get(field).and_then(|g| g.get(other)) {
        None => Default::default(),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        Some(other_v) => panic!(
            "reconcile_grants: {field}[{other}] is {other_v}; this sweep needs \
             explicit verb lists, a wildcard hides the intersection"
        ),
    }
}

fn payload_for(args: &Value, verb: &str) -> Value {
    let mut p = json!({"type": verb});
    if let Some(extra) = args
        .get("payloads")
        .and_then(|t| t.get(verb))
        .and_then(Value::as_object)
    {
        for (k, v) in extra {
            p[k] = v.clone();
        }
    }
    p
}

/// Execute a spec's steps in order.
async fn run_steps(h: &Hydration, sc: &Scenario, pinned: &mut BTreeMap<String, usize>) {
    let mut swapped: Option<Arc<Kernel>> = None;
    let rebooted = &mut swapped;
    // Sends fired by `start` and not yet awaited, plus what earlier steps bound.
    let mut tasks: BTreeMap<String, tokio::task::JoinHandle<Value>> = BTreeMap::new();
    let mut bound: BTreeMap<String, Value> = BTreeMap::new();
    let steps = sc.spec["expect"]["steps"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for (idx, step) in steps.iter().enumerate() {
        // A reboot REPLACED the kernel; everything after acts on the new one.
        let h = &Hydration {
            kernel: rebooted.clone().unwrap_or_else(|| Arc::clone(&h.kernel)),
            _fixture: tempfile::TempDir::new().unwrap(),
            creates: h.creates.clone(),
            boots: h.boots.clone(),
        };
        let label = step
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("<step>")
            .to_string();
        let expect = resolve(&step.get("expect").cloned().unwrap_or(json!({})), &bound);
        let reply: Value = if let Some(a) = step.get("reach") {
            reach(
                &h.kernel,
                a["from"].as_str().unwrap(),
                a["to"].as_str().unwrap(),
                a["send"].clone(),
            )
            .await
        } else if let Some(a) = step.get("operator") {
            let a = resolve(a, &bound);
            operator(
                &h.kernel,
                a["to"].as_str().unwrap(),
                a.get("send").cloned().unwrap_or(json!({})),
            )
            .await
        } else if let Some(a) = step.get("grant") {
            grant_step(h, a, false).await
        } else if let Some(a) = step.get("revoke") {
            grant_step(h, a, true).await
        } else if step.get("reboot").is_some() {
            // PERSIST + REHYDRATE, in place. The kernel is rebuilt from its own
            // saved state and each loader re-booted, so a step after this one is
            // asking about what SURVIVED — not about what is still in RAM.
            let snapshot = h.kernel.save_json();
            let fresh = mk_kernel();
            fresh
                .load_json(&snapshot)
                .unwrap_or_else(|e| panic!("{label}: reload refused the snapshot: {e:?}"));
            for lid in h.creates.keys() {
                let _ = operator(&fresh, lid, json!({"type": "boot"})).await;
            }
            *rebooted = Some(Arc::clone(&fresh));
            json!({"rebooted": true})
        } else if let Some(a) = step.get("assert_tree") {
            let (ids, _) = dehydrate(&h.kernel);
            let _ = a;
            json!({"ids": ids})
        } else if let Some(a) = step.get("reconcile_grants") {
            // For every pair either half mentions, the edge's verb set is the
            // INTERSECTION of the two declared lists, and a verb outside it is
            // refused by the half that omitted it — checked in order, so `side`
            // is not symmetric bookkeeping.
            let specs = manifest_specs(&sc.config);
            let mut probed = 0usize;
            for (sender, s_spec) in &specs {
                for (target, t_spec) in &specs {
                    if sender == target {
                        continue;
                    }
                    let egress = declared_verbs(s_spec, "can_call", target);
                    let ingress = declared_verbs(t_spec, "callable_by", sender);
                    let mut all: Vec<&String> = egress.union(&ingress).collect();
                    all.sort();
                    for verb in all {
                        probed += 1;
                        let reply = reach(&h.kernel, sender, target, payload_for(a, verb)).await;
                        let both = egress.contains(verb) && ingress.contains(verb);
                        assert_ne!(
                            denied(&reply),
                            both,
                            "{label}: {sender} -{verb}-> {target}: egress={egress:?} \
                             ingress={ingress:?} — expected {}, got {reply}",
                            if both { "reachable" } else { "refused" }
                        );
                        if both {
                            continue;
                        }
                        let want = if !egress.contains(verb) {
                            "egress"
                        } else {
                            "ingress"
                        };
                        assert_eq!(
                            denial_side(&reply),
                            Some(want),
                            "{label}: the half that omitted {verb:?} is {want:?}: {reply}"
                        );
                    }
                }
            }
            let floor = a.get("min_probes").and_then(Value::as_u64).unwrap_or(1) as usize;
            assert!(
                probed >= floor,
                "{label}: only {probed} declared triples (min {floor})"
            );
            json!({"swept": true})
        } else if let Some(a) = step.get("sweep_undeclared") {
            // The whole roster, every ordered pair. The reachable set must be
            // EXACTLY the structural baseline plus what the spec declared open
            // BEFORE this point — nothing rides along.
            let (ids, children) = dehydrate(&h.kernel);
            let universe: Vec<String> = ids.clone();
            let mut structural: std::collections::BTreeSet<(String, String)> =
                universe.iter().map(|a| (a.clone(), a.clone())).collect();
            for (parent, kids) in &children {
                for kid in kids {
                    structural.insert((parent.clone(), kid.clone()));
                    structural.insert((kid.clone(), parent.clone()));
                }
            }
            for verb in a["verbs"].as_array().cloned().unwrap_or_default() {
                let verb = verb.as_str().unwrap_or("reflect");
                let declared: std::collections::BTreeSet<(String, String)> = steps[..idx]
                    .iter()
                    .filter_map(|st| {
                        let r = st.get("reach")?;
                        (st.get("expect")?.get("allow")? == &json!(true)
                            && r["send"]["type"].as_str() == Some(verb))
                        .then(|| {
                            (
                                r["from"].as_str().unwrap_or("").to_string(),
                                r["to"].as_str().unwrap_or("").to_string(),
                            )
                        })
                    })
                    .collect();
                let want: std::collections::BTreeSet<_> =
                    structural.union(&declared).cloned().collect();
                let mut got: std::collections::BTreeSet<(String, String)> = Default::default();
                for sender in &universe {
                    for target in &universe {
                        let reply = reach(&h.kernel, sender, target, payload_for(a, verb)).await;
                        if denied(&reply) {
                            assert!(
                                matches!(denial_side(&reply), Some("egress") | Some("ingress")),
                                "{label}: verb {verb:?}: a refusal named no call-gate half \
                                 — {reply} (a sealed io leg denies with NO side, a DIFFERENT gate)"
                            );
                        } else {
                            got.insert((sender.clone(), target.clone()));
                        }
                    }
                }
                assert_eq!(
                    got,
                    want,
                    "{label}: verb {verb:?}: undeclared {:?}, declared-and-missing {:?}",
                    got.difference(&want).collect::<Vec<_>>(),
                    want.difference(&got).collect::<Vec<_>>()
                );
            }
            json!({"swept": true})
        } else if let Some(a) = step.get("start") {
            // Fire WITHOUT awaiting. The cancel rows need a dispatch that is
            // still running when the next step acts on it; nothing else in the
            // DSL can express "in flight".
            let k = Arc::clone(&h.kernel);
            let (from, to) = (
                a["from"].as_str().unwrap().to_string(),
                a["to"].as_str().unwrap().to_string(),
            );
            let send = a["send"].clone();
            let handle = tokio::spawn(async move {
                with_sender(AgentId::from(from.as_str()), async move {
                    let out = k
                        .send(vec![Item::new(AgentId::from(to.as_str()), send)])
                        .await
                        .remove(0);
                    json!({"pid": out.pid.map(|p| p.0), "status": format!("{:?}", out.status).to_lowercase(), "result": out.result})
                })
                .await
            });
            tasks.insert(a["name"].as_str().unwrap().to_string(), handle);
            json!({"started": true})
        } else if let Some(a) = step.get("await_task") {
            // Block until the started send is visible in `ps`. A fixed sleep is
            // a race that passes on a fast machine and fails on a loaded one.
            let want_agent = a.get("agent").and_then(Value::as_str).unwrap_or("");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let mut found: Option<Value> = None;
            while std::time::Instant::now() < deadline {
                let rows = operator(&h.kernel, "kernel", json!({"type": "ps"})).await;
                if let Some(list) = rows.get("tasks").and_then(Value::as_array) {
                    if let Some(row) = list
                        .iter()
                        .find(|t| t.get("agent").and_then(Value::as_str) == Some(want_agent))
                    {
                        found = Some(row.clone());
                        break;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            let row = found.unwrap_or_else(|| {
                panic!("{label}: no task for agent {want_agent:?} appeared in `ps`")
            });
            bound.insert(a["name"].as_str().unwrap().to_string(), row.clone());
            row
        } else if let Some(a) = step.get("collect") {
            let name = a["name"].as_str().unwrap().to_string();
            let handle = tasks
                .remove(&name)
                .unwrap_or_else(|| panic!("{label}: nothing started under {name:?}"));
            let outcome = handle.await.expect("started send panicked");
            // MERGE, never clobber: `await_task` bound the ps row (`id`) and
            // this binds the outcome (`pid`) — the SAME id, by the one-id-space
            // rule — so a later step can still name either.
            let mut merged = bound.get(&name).cloned().unwrap_or(json!({}));
            if let (Some(m), Some(o)) = (merged.as_object_mut(), outcome.as_object()) {
                for (k, v) in o {
                    m.insert(k.clone(), v.clone());
                }
            }
            bound.insert(name, merged);
            let mut r = outcome.get("result").cloned().unwrap_or(json!({}));
            if let Some(st) = outcome.get("status") {
                if let Some(o) = r.as_object_mut() {
                    o.insert("status".into(), st.clone());
                }
            }
            r
        } else {
            // Verbs this runner does not implement yet are SKIPPED BY NAME with
            // the reason visible, never silently: a step that quietly did
            // nothing would read as a passing assertion.
            let verb = step
                .as_object()
                .map(|o| {
                    o.keys()
                        .filter(|k| !matches!(k.as_str(), "name" | "expect" | "because"))
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default();
            eprintln!(
                "  [{}] SKIP {label}: `{verb}` not implemented here",
                sc.name
            );
            continue;
        };
        match_step(&label, &expect, &reply);
        if let Some(rid) = step.get("because").and_then(Value::as_str) {
            *pinned.entry(rid.to_string()).or_insert(0) += 1;
        }
    }
}

// ─── the suite ──────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_shared_agentic_corpus_holds_on_this_runtime() {
    // RUN INSIDE A TEMP DIR. The manifest read obeys the running-dir law (`fs`
    // is the one disk surface, and a cold bring-up read is not exempt), and the
    // discovered store roots at `<cwd>/.fantastic` — so a runner left in the
    // crate directory writes `.fantastic` INTO THE SOURCE TREE on every run.
    //
    // `chdir` is process-global and would be a race in a shared test binary;
    // this binary holds exactly one test, so it owns the cwd. Python gets the
    // same effect per-worker because xdist forks a process each.
    let scenarios = load_scenarios(); // resolved from CARGO_MANIFEST_DIR, before the chdir
    let workdir = tempfile::Builder::new()
        .prefix("agentic_run_")
        .tempdir()
        .unwrap();
    std::env::set_current_dir(workdir.path()).unwrap();
    assert!(!scenarios.is_empty(), "corpus loaded no scenarios");
    let mut pinned: BTreeMap<String, usize> = BTreeMap::new();
    let mut ran = 0usize;

    for sc in &scenarios {
        let declared = sc.spec["expect"]["hydration"]
            .as_str()
            .unwrap_or("ok")
            .to_string();
        let h = hydrate(sc).await;

        // PC-5 — the hydration verdict, on the LOUD channel. `create_agent`
        // boots its child and swallows the exception, so the create reply is the
        // only place a write-time refusal is visible.
        // PC-5 reads BOTH loud channels: `create_agent`'s reply (a write-time
        // refusal — the loader never became an agent) and the explicit boot's
        // (a manifest that parses but cannot build). They are different
        // verdicts and a config declares which one it expects.
        let loaders = sc.config["loaders"].as_array().cloned().unwrap_or_default();
        let all_created = loaders.iter().all(|l| {
            let lid = l["id"].as_str().unwrap_or("");
            h.creates
                .get(lid)
                .map(|r| r.get("error").is_none())
                .unwrap_or(false)
        });
        let all_booted = loaders.iter().all(|l| {
            let lid = l["id"].as_str().unwrap_or("");
            h.boots
                .get(lid)
                .map(|r| r.get("error").is_none() && r.get("booted") != Some(&json!(false)))
                .unwrap_or(false)
        });
        let built = all_created && all_booted;
        if declared == "ok" {
            assert!(
                built,
                "{}: declared hydration ok, creates said {:?}",
                sc.name, h.creates
            );
        } else {
            assert!(
                !built,
                "{}: declared hydration {declared:?} but every loader built",
                sc.name
            );
            continue; // nothing to reach in a system that was refused
        }
        run_steps(&h, sc, &mut pinned).await;
        ran += 1;
    }

    eprintln!("rust agentic: {ran}/{} scenarios ran", scenarios.len());
    let mut summary: Vec<String> = pinned.iter().map(|(k, v)| format!("{k}x{v}")).collect();
    summary.sort();
    eprintln!("rust agentic requirements pinned: {}", summary.join(", "));
    assert!(ran > 0, "no scenario hydrated");
}
