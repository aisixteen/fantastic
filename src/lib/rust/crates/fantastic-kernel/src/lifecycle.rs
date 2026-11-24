//! Agent lifecycle: create, delete (with depth-first cascade +
//! on_delete hooks + disk cleanup), update.
//!
//! `create_agent` is dispatched on a parent — the new agent becomes
//! its child under `<parent.root_path>/agents/<new_id>/`. Substrate
//! mints the id from the bundle name + a 6-hex token unless caller
//! supplies one.
//!
//! `delete_agent` is dispatched on the substrate (target id in
//! payload). It refuses if the agent carries `delete_lock: true`.
//! Otherwise it walks the subtree depth-first, calls each bundle's
//! `on_delete` hook BEFORE unregistering from `kernel.agents`, and
//! rmtrees the dir (unless the agent is ephemeral).
//!
//! `delete_agent keep_memory:true` tells the subtree's hooks to leave the
//! agents' persisted memory-store rows ([`keep_memory`]); by default an AI
//! agent's rows are purged with it.
//!
//! THE CRATE-PRIVATE BOUNDARY. The `create_agent` and `delete_agent` bodies take
//! the creator as an argument, so only the kernel calls them, for its own verb.
//! A bundle that acts for its asker sends the verb with
//! [`Kernel::send_as_caller`], which reads the origin scope inside the kernel.
//! Neither body, nor the unchecked tree load, is reachable from outside:
//!
//! ```compile_fail
//! # async fn f(k: std::sync::Arc<fantastic_kernel::Kernel>, a: fantastic_kernel::declared::kernel::CreateAgent) {
//! let _ = fantastic_kernel::lifecycle::create_agent(&k, None, a).await;
//! # }
//! ```
//!
//! ```compile_fail
//! # async fn f(k: std::sync::Arc<fantastic_kernel::Kernel>, a: fantastic_kernel::declared::kernel::DeleteAgent) {
//! let _ = fantastic_kernel::lifecycle::delete_agent(&k, None, &a).await;
//! # }
//! ```
//!
//! ```compile_fail
//! # fn f(k: &fantastic_kernel::Kernel) {
//! let _ = k.load_records(Vec::new());
//! # }
//! ```
//!
//! The same shape through the public entry compiles. A whole-tree load with
//! unchecked `can` (`Kernel::load`, `Kernel::load_json`) exists for tests only
//! (`test-support`); the kernel's own bring-up is the only shipped path.
//!
//! ```
//! # async fn f(k: std::sync::Arc<fantastic_kernel::Kernel>) {
//! let _ = k
//!     .send_as_caller(vec![fantastic_kernel::Item::new(
//!         "kernel",
//!         serde_json::json!({"type": "delete_agent", "id": "x"}),
//!     )])
//!     .await;
//! # }
//! ```

use crate::agent::{Agent, AgentId};
use crate::kernel::Kernel;
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// `n` bytes from the OS CSPRNG — the source class python's `secrets` module
/// and `uuid.uuid4()` draw from (`os.urandom`). A failure is FATAL: there is
/// no second source, and a clock/pid mixer is not random at all.
pub(crate) fn os_random(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    fill_random(&mut buf);
    buf
}

/// Fill `buf` from the OS CSPRNG — the ONE randomness call of every crate,
/// the OS's own call ([`crate::sys`]). A failure is FATAL: there is no second
/// source.
pub fn fill_random(buf: &mut [u8]) {
    crate::sys::fill_random(buf);
}

/// Lowercase hex, python's `bytes.hex()` spelling.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// python `secrets.token_hex(n)` — `n` CSPRNG bytes as lowercase hex.
#[must_use]
pub fn token_hex(n: usize) -> String {
    hex(&os_random(n))
}

/// Mint a fresh id from `handler_module` (`<bundle>.tools` →
/// `<bundle>_<hex6>`) — python `_agent.py`'s
/// `f"{bundle}_{secrets.token_hex(3)}"`, where `bundle` is the SECOND-LAST
/// dotted segment (`handler_module.split(".")[-2]`), whatever the last one is:
/// `a.b.tools` → `b_…`, `pkg.ts` → `pkg_…`. A dotless module is used whole.
pub fn mint_id(handler_module: &str) -> String {
    let parts: Vec<&str> = handler_module.split('.').collect();
    let bundle = if parts.len() > 1 {
        parts[parts.len() - 2]
    } else {
        handler_module
    };
    format!("{bundle}_{}", hex(&os_random(3)))
}

/// Mint a stable 32-hex-char (lowercase) kernel instance guid — python
/// `kernel_state.tools`' `uuid.uuid4().hex`: 16 OS-random bytes carrying
/// uuid4's version nibble and the RFC-4122 variant bits. Called once at boot
/// when the root record carries no `guid`; an explicit/existing guid wins.
pub fn mint_guid() -> String {
    let mut b = os_random(16);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    hex(&b)
}

/// The `can` lines of a record, judged at WRITE time against the legal shape
/// ([`crate::authz::parse_can`]). An absent `can` holds no line.
pub fn bad_can(meta: &Map<String, Value>) -> Option<String> {
    meta.get("can").and_then(|v| crate::authz::parse_can(v).err())
}

/// Does `id` match one of the `who` patterns (each with at most one trailing `*`;
/// a bare `*` matches every id)?
pub(crate) fn who_matches(who: &[String], id: &str) -> bool {
    who.iter().any(|w| match w.strip_suffix('*') {
        Some(prefix) => id.starts_with(prefix),
        None => w == id,
    })
}

/// What one `create` line's `what` says of field `field` of `bundle`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum FieldRule {
    /// `"*"`: the creator sets it, or a line below narrows it.
    Free,
    /// A value: the field is set to it, whatever the creator or the bundle says.
    Forced(Value),
    /// The line does not name it.
    Unnamed,
}

/// Does `what` select `bundle` (by name, by a `"*"` key, or `what: "*"`)?
pub(crate) fn what_selects(what: &crate::authz::What, bundle: &str) -> bool {
    match what {
        crate::authz::What::All => true,
        crate::authz::What::Bundles(m) => m.contains_key(bundle) || m.contains_key("*"),
    }
}

/// The rule `what` gives `field` of `bundle`: the bundle's own entry first, then
/// the `"*"` bundle entry; within each, the field's own key, then the `"*"` key.
pub(crate) fn field_rule(what: &crate::authz::What, bundle: &str, field: &str) -> FieldRule {
    let crate::authz::What::Bundles(m) = what else {
        return FieldRule::Free;
    };
    for b in [bundle, "*"] {
        let Some(fields) = m.get(b) else { continue };
        for f in [field, "*"] {
            if let Some(v) = fields.get(f) {
                return if v.as_str() == Some("*") || f == "*" {
                    FieldRule::Free
                } else {
                    FieldRule::Forced(v.clone())
                };
            }
        }
    }
    FieldRule::Unnamed
}

/// The `create` lines of `holder` that cover creating `new_id` of `bundle` under
/// `parent`: a line on T covers every agent of T's tree, T included.
pub(crate) fn matching_create_lines(
    kernel: &Kernel,
    holder: &AgentId,
    parent: &AgentId,
    bundle: &str,
    new_id: &str,
) -> Vec<(Vec<String>, crate::authz::What)> {
    let Some(a) = kernel.agents.get(holder).map(|e| Arc::clone(&e)) else {
        return Vec::new();
    };
    let can = a
        .meta
        .read()
        .expect("meta lock poisoned")
        .get("can")
        .cloned()
        .unwrap_or(Value::Null);
    crate::authz::solver::lines_of(&can)
        .into_iter()
        .filter_map(|l| match (l.target, l.level) {
            (crate::authz::Target::Id(t), crate::authz::Level::Create { who, what })
                if kernel.in_tree(parent, &AgentId::from(t.as_str()))
                    && who_matches(&who, new_id)
                    && what_selects(&what, bundle) =>
            {
                Some((who, what))
            }
            _ => None,
        })
        .collect()
}

/// The id a create mints for `caller` under `parent`: when the first of the
/// caller's create lines covering the parent with `bundle` in `what` has a `who`
/// entry other than `*`, that entry's prefix plus `<hex6>` (`gbr_*` →
/// `gbr_<hex6>`; an entry with no `*` is the id itself); otherwise
/// [`mint_id`]. Either way the id is then checked against `who`.
fn mint_for(kernel: &Kernel, caller: &AgentId, parent: &AgentId, bundle: &str) -> String {
    let Some(a) = kernel.agents.get(caller).map(|e| Arc::clone(&e)) else {
        return mint_id(bundle);
    };
    let can = a
        .meta
        .read()
        .expect("meta lock poisoned")
        .get("can")
        .cloned()
        .unwrap_or(Value::Null);
    for l in crate::authz::solver::lines_of(&can) {
        let (crate::authz::Target::Id(t), crate::authz::Level::Create { who, what }) =
            (l.target, l.level)
        else {
            continue;
        };
        if !kernel.in_tree(parent, &AgentId::from(t.as_str())) || !what_selects(&what, bundle) {
            continue;
        }
        if let Some(w) = who.iter().find(|w| w.as_str() != "*") {
            return match w.strip_suffix('*') {
                Some(prefix) => format!("{prefix}{}", hex(&os_random(3))),
                None => w.clone(),
            };
        }
        return mint_id(bundle);
    }
    mint_id(bundle)
}

/// Rule 8 at a create by `caller`: every key of `meta` (bundle defaults included)
/// must be named by a matching line of the caller AND of every capping ancestor
/// (each ancestor whose tree does not hold `parent`), or be `can`. The values
/// those lines force are written over `meta`; two forced values that differ
/// refuse. Answers the forced values (a forced `can` was checked when its create
/// line was written, so the create does not check it again), or the refusal.
fn apply_create_lines(
    kernel: &Kernel,
    caller: &AgentId,
    parent: &AgentId,
    bundle: &str,
    new_id: &str,
    meta: &mut Map<String, Value>,
) -> Result<Map<String, Value>, Value> {
    let bad_field = |field: &str, why: String| {
        let mut r = crate::refusal::refusal(crate::refusal::BAD_ARG, why);
        crate::refusal::with(&mut r, "arg", json!(field));
        r
    };
    let mut holders = vec![caller.clone()];
    if let Some(c) = kernel.agents.get(caller).map(|e| Arc::clone(&e)) {
        holders.extend(
            kernel
                .ancestor_chain(&c)
                .into_iter()
                .filter(|a| !kernel.in_tree(parent, a)),
        );
    }
    let per: Vec<Vec<(Vec<String>, crate::authz::What)>> = holders
        .iter()
        .map(|h| matching_create_lines(kernel, h, parent, bundle, new_id))
        .collect();
    // forced values: from every matching line of the caller and the cap
    let mut forced: Map<String, Value> = Map::new();
    for lines in &per {
        for (_, what) in lines {
            let crate::authz::What::Bundles(m) = what else {
                continue;
            };
            for b in [bundle, "*"] {
                let Some(fields) = m.get(b) else { continue };
                for f in fields.keys().filter(|f| f.as_str() != "*") {
                    let FieldRule::Forced(v) = field_rule(what, bundle, f) else {
                        continue;
                    };
                    match forced.get(f) {
                        Some(have) if *have != v => {
                            return Err(bad_field(
                                f,
                                format!(
                                    "create_agent: two lines force different values on {}",
                                    fantastic_bundle::py_repr(f)
                                ),
                            ));
                        }
                        Some(_) => {}
                        None => {
                            forced.insert(f.clone(), v);
                        }
                    }
                }
            }
        }
    }
    for (k, v) in &forced {
        meta.insert(k.clone(), v.clone());
    }
    // no field, no set: every key named by the caller's lines and the cap's
    for k in meta.keys() {
        if k == "can" {
            continue;
        }
        let named = per.iter().all(|lines| {
            lines
                .iter()
                .any(|(_, what)| field_rule(what, bundle, k) != FieldRule::Unnamed)
        });
        if !named {
            return Err(bad_field(
                k,
                format!(
                    "create_agent: no create line names field {} of {}",
                    fantastic_bundle::py_repr(k),
                    fantastic_bundle::py_repr(bundle)
                ),
            ));
        }
    }
    Ok(forced)
}

/// What [`Kernel::check_asker_create`] answers: the id the create is to carry,
/// and the values the asker's own lines force on it.
#[derive(Debug, Clone, PartialEq)]
pub struct AskerCreate {
    /// The new agent's id: the one asked for, or one minted from the asker's
    /// matching `who`.
    pub id: String,
    /// The values the asker's matching create lines (and their cap) force.
    pub forced: Map<String, Value>,
}

impl Kernel {
    /// THE ASKER CHECK of a verb that creates for its asker (rule 7: such a verb
    /// is ALSO checked as the asker; Q39). The verb's own agent sends the create
    /// as itself, so its lines write the child (its forced values, its `can`
    /// hand-outs); this checks, first, that the asker may make the same child:
    ///
    /// - the asker is the origin of the running dispatch ([`crate::send::origin_scope`]):
    ///   the operator passes; no origin scope at all is refused;
    /// - `record` is the child as asked: `handler_module`, `id` (minted from the
    ///   asker's `who` when absent), its fields, and the `can` lines the ASKER
    ///   supplied (lines the verb's agent adds are its own hand-outs, checked when
    ///   it sends the create);
    /// - the asker's lines (capped) allow creating that bundle and id under
    ///   `parent`, or the answer is the unknown answer for `parent`;
    /// - every field is named by the asker's matching lines and its cap (rule 8),
    ///   except the fields the running agent's own matching lines force: those are
    ///   the line's, and its writer handed them out (Q23);
    /// - a value the asker's lines force and the running agent's lines force
    ///   differently refuses (no child satisfies both);
    /// - every supplied `can` line is held by the asker's written lines (rule 7),
    ///   unless a line of either forces `can`.
    ///
    /// Answers the id and the asker's forced values, which the verb writes into
    /// the create it sends; or the refusal.
    pub fn check_asker_create(
        &self,
        parent: &AgentId,
        record: &Map<String, Value>,
    ) -> Result<AskerCreate, Value> {
        let supplied = record.get("id").and_then(Value::as_str).map(str::to_string);
        let Some(asker) = crate::send::origin_scope() else {
            return Err(crate::refusal::unknown_agent(parent.as_str()));
        };
        let Some(p) = self.resolve_target(parent.as_str()) else {
            return Err(crate::refusal::unknown_agent(parent.as_str()));
        };
        let bundle = record
            .get("handler_module")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let Some(asker) = asker else {
            let id = supplied.unwrap_or_else(|| mint_id(&bundle));
            return Ok(AskerCreate { id, forced: Map::new() });
        };
        let id = supplied.unwrap_or_else(|| mint_for(self, &asker, &p.id, &bundle));
        let act = crate::authz::solver::Act::Create {
            bundle: bundle.clone(),
            new_id: id.clone(),
        };
        if bundle.is_empty() || !self.allowed(&asker, &act, &p) {
            return Err(crate::refusal::unknown_agent(parent.as_str()));
        }
        // the running agent's forced values: the line's, never the asker's to name
        let line_forced = match crate::send::current_sender() {
            Some(me) if me != asker => {
                let mut probe = Map::new();
                apply_create_lines(self, &me, &p.id, &bundle, &id, &mut probe).unwrap_or_default()
            }
            _ => Map::new(),
        };
        let mut meta: Map<String, Value> = record
            .iter()
            .filter(|(k, _)| !matches!(k.as_str(), "id" | "handler_module" | "parent_id" | "type"))
            .filter(|(k, _)| !line_forced.contains_key(*k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let forced = apply_create_lines(self, &asker, &p.id, &bundle, &id, &mut meta)?;
        for (k, v) in &forced {
            if line_forced.get(k).is_some_and(|l| l != v) {
                let mut r = crate::refusal::refusal(
                    crate::refusal::BAD_ARG,
                    format!(
                        "create_agent: the asker's line and the creator's line force different values on {}",
                        fantastic_bundle::py_repr(k)
                    ),
                );
                crate::refusal::with(&mut r, "arg", json!(k));
                return Err(r);
            }
        }
        if !forced.contains_key("can") && !line_forced.contains_key("can") {
            if let Some(can) = meta.get("can") {
                if let Some(why) = crate::authz::parse_can(can).err() {
                    return Err(crate::refusal::refusal(crate::refusal::INVALID_META, &why));
                }
                let recipient = crate::authz::glue::Recipient::Pending { parent: &p.id };
                for line in crate::authz::solver::lines_of(can) {
                    crate::authz::glue::can_write(self, &asker, recipient, &line)?;
                }
            }
        }
        Ok(AskerCreate { id, forced })
    }
}

/// The `create_agent` body, for the kernel verb alone (crate-private): a bundle
/// that makes a child for its asker (a file bridge's view, `fork`,
/// `import_subtree`) sends the verb with [`Kernel::send_as_caller`], which reads
/// the origin scope inside the kernel, so no bundle names the creator. `caller` is
/// the creator: `None` for the operator, which no line limits. `args` are the declared, typed arguments of `create_agent`; any field
/// beyond them is a field of the new agent's record, which its bundle's shared
/// entry checks.
///
/// With an agent caller, in order: the parent must exist; the id is minted (from
/// the caller's matching `who`, else the bundle's name) or taken as supplied;
/// `allowed(caller, create{bundle, id}, parent)` must hold, or the answer is the
/// same unknown answer as a missing parent; then the bundle must be installed;
/// then the lines' forced values are written over the bundle's and the
/// creator's, and every key that ends up set must be named by a matching line
/// of the caller and of every capping ancestor (rule 8); then every line of the
/// new `can` (the bundle's default lines included) must be held by the caller's
/// own written lines (rule 7), unless a line forced it.
pub(crate) async fn create_agent(
    kernel: &Arc<Kernel>,
    caller: Option<&AgentId>,
    args: crate::declared::kernel::CreateAgent,
) -> Value {
    let pid = args.parent_id.clone();
    let supplied_id = args.id.clone();
    // THE BUNDLE'S FIELDS go under the arguments' (`crate::inherit`).
    let (hm, mut meta) = crate::inherit::create_args(&kernel.bundles, args);
    let hm = hm.as_str();
    let pid = pid.as_str();

    // `parent_id` NAMES THE OWNER. It is a real, REQUIRED argument — resolved
    // like a send target (the reserved `kernel` address included), never defaulted.
    let Some(parent) = kernel.resolve_target(pid) else {
        return crate::refusal::unknown_agent(pid);
    };
    let parent = &parent;

    // A supplied id — even "" — is the caller's choice and is validated as one
    // by the declaration; only an ABSENT id is minted (python parity).
    let id = match supplied_id {
        Some(v) => v,
        None => match caller {
            Some(c) => mint_for(kernel, c, &parent.id, hm),
            None => mint_id(hm),
        },
    };

    // THE CREATE LINE. An agent creates only what a line in its `can` (capped by
    // every ancestor) allows; anything else answers exactly as a missing parent.
    // A bare agent (no handler) is the operator's alone: the declaration requires a
    // handler, and no line covers bundle "".
    if let Some(c) = caller {
        let act = crate::authz::solver::Act::Create {
            bundle: hm.to_string(),
            new_id: id.clone(),
        };
        if hm.is_empty() || !kernel.allowed(c, &act, parent) {
            return crate::refusal::unknown_agent(pid);
        }
    }

    // THE HANDLER MUST BE INSTALLED (issue #642): an agent of a bundle this kernel
    // does not run would be created, and fail only when called. Checked after the
    // create line, so a bundle the caller may not create and one that is not
    // installed answer the same. The LOAD path still skips a record whose bundle
    // is another runtime's (weak-load semantics).
    let Some(bundle) = kernel.bundles.get(hm) else {
        let mut r = crate::refusal::refusal_hint(
            crate::refusal::BAD_ARG,
            format!(
                "create_agent: handler_module {} is not installed in this kernel",
                fantastic_bundle::py_repr(hm)
            ),
            "`reflect` with bundles=ids lists the installed bundles",
        );
        crate::refusal::with(&mut r, "arg", json!("handler_module"));
        return r;
    };

    // NO FIELD, NO SET (rule 8): the lines' forced values over the bundle's and the
    // creator's, and every key named by the caller's lines and the cap's.
    let mut can_forced = false;
    if let Some(c) = caller {
        match apply_create_lines(kernel, c, &parent.id, hm, &id, &mut meta) {
            Ok(forced) => can_forced = forced.contains_key("can"),
            Err(refused) => return refused,
        }
    }

    // WRITE-TIME VALIDATION, same gate as `update_agent` (py-canonical), so a
    // field has one answer at create and at update. Record is empty
    // at create: the merged view IS the patch. THE RECORD'S FIELDS and the
    // bundle's own check first: the target's bundle judges them through its
    // shared entry (the declared shape of `can` included); the kernel only routes
    // the refusal. Then the `can` lines against the legal shape.
    if let Err(refused) = bundle.check_write("create_agent", &meta, &Map::new(), None) {
        return refused;
    }
    if let Some(why) = bad_can(&meta) {
        return crate::refusal::refusal(crate::refusal::INVALID_META, &why);
    }
    // RULE 7: the new agent's first `can` (the creator's and the bundle's default
    // lines alike) holds only what the creator's own written lines hold, cap
    // included. A `can` a create line forces was checked when that line was
    // written, and is not checked again.
    if let (Some(c), false) = (caller, can_forced) {
        if let Some(can) = meta.get("can") {
            let recipient = crate::authz::glue::Recipient::Pending { parent: &parent.id };
            for line in crate::authz::solver::lines_of(can) {
                if let Err(refused) = crate::authz::glue::can_write(kernel, c, recipient, &line) {
                    return refused;
                }
            }
        }
    }

    // An id BECOMES A DIRECTORY, so it must be one safe segment. Enforced at
    // the mint, so every consumer inherits it rather than each remembering.
    // The id's shape is the declaration's (`CreateAgent.id`); `kernel` is the operator's.
    if id == crate::refusal::RESERVED_ID {
        return crate::refusal::bad(
            "create_agent",
            "id",
            &format!(
                "a name other than the reserved {}",
                fantastic_bundle::py_repr(crate::refusal::RESERVED_ID)
            ),
        );
    }
    if kernel.agents.contains_key(&AgentId::from(id.as_str())) {
        let mut r = crate::refusal::refusal(
            crate::refusal::EXISTS,
            format!("agent {} exists", fantastic_bundle::py_repr(&id)),
        );
        crate::refusal::with(
            &mut r,
            "hint",
            json!("pick another id, or update the existing agent"),
        );
        crate::refusal::with(&mut r, "id", json!(id));
        return r;
    }

    // In Disk mode the agent's `root_path` is also where bundles can
    // write sidecar files (chat history, schedules, etc.). In
    // InMemory mode the path is composed but never read — bundles
    // that try to write a sidecar there will surface the fs error
    // themselves.
    let root_path = kernel.children_dir_of(parent).join(&id);
    let new_agent = Agent::new(
        AgentId::from(id.as_str()),
        Some(hm.to_string()),
        Some(parent.id.clone()),
        meta,
        root_path,
        false,
    );
    // Wire into kernel + parent FIRST, then persist. Order matters now that
    // persistence routes THROUGH a discovered `file_bridge` provider (a child of
    // root): a freshly-created store must already be a registered child so it can
    // persist its OWN record through itself (find_store sees it). No-ops in
    // InMemory mode or when no provider is wired (RAM — see `persistence::persist`).
    kernel.register(Arc::clone(&new_agent));
    parent.add_child(Arc::clone(&new_agent));

    // NO INLINE PERSIST. The record reaches disk through the ROOT LOADER's
    // `persist_writer`, which consumes the `added` event published below and
    // debounces it — python's `_FlushLoop`. A kernel with no loader booted (or no store
    // wired) keeps the record in RAM, which is the auto-flush law.

    // LIFECYCLE SHAPE, python's: `{agent_id, kind, …}` like every other state
    // event, so one subscriber reads one shape. `created` with a `handler_module`
    // was a rust-only kind nothing else answers; `name` is what a view renders.
    let event = json!({
        "agent_id": new_agent.id.0,
        "kind": "added",
        "name": new_agent.name_or_id(),
        "parent_id": parent.id.0,
    });
    kernel.publish_state(&event);

    let created = new_agent.record();
    let rec_value = crate::jsonsafe::record_value(&created);
    // BOOT — kernel bookkeeping: the kernel initializing the child it was told to
    // make, not the parent reaching out. The child's handler runs directly
    // ([`Kernel::announce`]), as the child, never through a gated send. Once
    // loaded, a bundle is an agent like any other: its origin is the child ITSELF,
    // as at every later boot (a restart, a reload), so its hook has the same
    // authority whenever it runs — its own lines, never its creator's. Failures
    // are logged but don't abort the create — matches Python.
    //
    // Wrap in Box::pin because boot may call create_agent recursively (a boot
    // hook may create a child agent); the resulting async future is recursive
    // and Rust requires explicit indirection.
    let boot = Box::pin(kernel.announce(&new_agent, json!({"type": "boot"}))).await;
    let new_id = new_agent.id.clone();
    // BRANCH ON THE OUTCOME STATUS, not on the reply shape. A handler that
    // RAISED settles `failed` and is the one case python prints — on stdout,
    // with the result's `str()`. A boot that merely answered `{error:…}`
    // (`errored`) or completed prints nothing — no WARN for every refusal.
    if let Some(boot) = boot.filter(|b| b.status == crate::outcome::Status::Failed) {
        println!(
            "  [create_agent] {} boot failed: {}",
            new_id.0,
            boot.result
        );
    }
    // Lifecycle event on the PARENT's inbox, not the root's: whoever watches an
    // agent for new children still sees them arrive. Kernel bookkeeping too —
    // put on the parent's inbox and mirrored to its watchers directly (the
    // fanout), never a gated emit. Python
    // `parent._fanout(..., kind="emit")`. EACH RECEIVER gets the new record cut
    // to what it holds on the new agent (rule 9, the reflect cut): the parent
    // holds it whole (its own tree), a watcher of the parent only its identity
    // and the fields it holds `update` on, a watcher that holds nothing on it no
    // event. A receiver the kernel cannot place (a watcher that is no agent, the
    // state stream) gets the identity keys only.
    let event = |rec: crate::agent::AgentRecord| {
        crate::inbox::InboxItem::text(crate::jsonsafe::obj([
            ("type", Value::from("agent_created")),
            ("id", Value::from(new_id.0.clone())),
            ("agent", crate::jsonsafe::record_value(&rec)),
        ]))
    };
    kernel
        .fanout_each(
            parent,
            "emit",
            |receiver| match receiver {
                Some(r) => kernel
                    .record_cut_for(Some(r), &new_agent, created.clone())
                    .map(&event),
                None => Some(event(Kernel::identity_record(created.clone()))),
            },
            crate::send::current_sender().as_ref(),
        )
        .await;
    rec_value
}

/// DFS through `target`'s subtree looking for the first agent (including
/// `target` itself) whose `delete_lock` meta is true. Returns the locked
/// agent's id, or `None` if the whole subtree is clear. Mirrors Python's
/// `Agent._find_locked_descendant`.
fn find_locked_descendant(kernel: &Kernel, target: &Arc<Agent>) -> Option<AgentId> {
    if target.is_delete_locked() {
        return Some(target.id.clone());
    }
    for cid in target.child_ids() {
        if let Some(child) = kernel.agents.get(&cid).map(|e| Arc::clone(&e)) {
            if let Some(blocker) = find_locked_descendant(kernel, &child) {
                return Some(blocker);
            }
        }
    }
    None
}

const KEEP_MEMORY_KEY: &str = "lifecycle.keep_memory";

/// The ids of the delete in flight that asked to keep their persisted memory.
#[derive(Default)]
pub struct KeepMemory(std::sync::Mutex<std::collections::HashSet<AgentId>>);

/// True while `id` is being deleted by a `delete_agent keep_memory:true`: the
/// `on_delete` hook of a memory-owning bundle then leaves its store rows.
#[must_use]
pub fn keep_memory(kernel: &Kernel, id: &AgentId) -> bool {
    kernel
        .extension::<KeepMemory>(KEEP_MEMORY_KEY)
        .0
        .lock()
        .expect("keep_memory poisoned")
        .contains(id)
}

/// Implementation of the `delete_agent` system verb. `caller` is the deleter:
/// `None` for the operator. An agent deletes an agent only when its lines
/// (capped) allow `delete` on that agent — a `create` line selecting it; the
/// agent's whole subtree goes with it. A bare agent is deleted by the operator
/// only (no line covers bundle ""). Refused, the answer is the unknown answer with
/// `deleted:false`, the same bytes as a missing id.
pub(crate) async fn delete_agent(
    kernel: &Arc<Kernel>,
    caller: Option<&AgentId>,
    args: &crate::declared::kernel::DeleteAgent,
) -> Value {
    let id_str = args.id.as_str();
    let id = AgentId::from(id_str);
    // THE ONE REFUSAL SHAPE, py-canonical: a code to branch on and `deleted:false`
    // so a caller reading that field is not told a typo succeeded.
    let unknown = || {
        let mut r = crate::refusal::unknown_agent(id_str);
        crate::refusal::with(&mut r, "deleted", json!(false));
        r
    };
    let Some(target) = kernel.agents.get(&id).map(|e| Arc::clone(&e)) else {
        return unknown();
    };
    if let Some(c) = caller {
        if !kernel.allowed(c, &crate::authz::solver::Act::Delete, &target) {
            return unknown();
        }
    }
    // THE ROOT IS NOT DELETABLE. It owns the persistence surface and answers
    // every kernel verb, so cascading it empties the kernel and leaves a process
    // with no control surface at all — `{"deleted": true}` and an empty tree.
    // PYTHON'S PROSE, verbatim (`<id>` raw, no repr
    // quoting) — key order `error, reason, arg, id, deleted` (`arg`/`id`/
    // `deleted` all ride python's `**extra`, in the order the call names them).
    if target.parent_id.is_none() {
        let mut r = crate::refusal::refusal(
            crate::refusal::BAD_ARG,
            format!(
                "delete_agent: {id_str} is the ROOT — it owns persistence and \
                 answers the kernel verbs; deleting it would leave no kernel",
            ),
        );
        crate::refusal::with(&mut r, "arg", json!("id"));
        crate::refusal::with(&mut r, "id", json!(id_str));
        crate::refusal::with(&mut r, "deleted", json!(false));
        return r;
    }
    // Refuse the cascade if ANY agent in the subtree (including the
    // target itself) carries `delete_lock=true`. Walking the subtree
    // matches Python's `_find_locked_descendant` — without this, the
    // caller learns "delete refused" but not which descendant blocks
    // them. Returns the first locked id so the caller can clear it
    // (`update_agent id=<blocker> delete_lock=null`) before retrying.
    if let Some(blocker) = find_locked_descendant(kernel, &target) {
        // THE ONE REFUSAL SHAPE, never a bare `locked: true` flag without a
        // `reason`. PYTHON'S PROSE, verbatim (both ids `py_repr`-quoted); key
        // order `error, reason, hint, blocked_by, id`.
        let mut r = crate::refusal::refusal_hint(
            crate::refusal::LOCKED,
            format!(
                "delete_agent: {} blocked by delete_lock on {}; clear it via \
                 update_agent before deleting",
                fantastic_bundle::py_repr(&id.0),
                fantastic_bundle::py_repr(&blocker.0),
            ),
            "clear the lock with update_agent, then delete again",
        );
        crate::refusal::with(&mut r, "blocked_by", json!(blocker.0));
        crate::refusal::with(&mut r, "id", json!(id.0));
        return r;
    }
    let mut gone = Vec::new();
    subtree_ids(kernel, &target, &mut gone);
    // `keep_memory:true` (only the literal) tells every `on_delete` hook of this
    // subtree to leave the agent's persisted memory alone. The hook has no
    // payload, so the flag rides kernel state, set for this one delete.
    let keep = args.keep_memory == Some(true);
    let kept = kernel.extension::<KeepMemory>(KEEP_MEMORY_KEY);
    if keep {
        kept.0
            .lock()
            .expect("keep_memory poisoned")
            .extend(gone.iter().cloned());
    }
    cascade_delete(kernel, &target).await;
    if keep {
        let mut set = kept.0.lock().expect("keep_memory poisoned");
        for g in &gone {
            set.remove(g);
        }
    }
    // The deleted ids' lines stay: other agents' lines on an id reach whoever is
    // created under it next (ids may be reused).
    // Emit `agent_deleted` on the root's inbox so watchers refresh
    // (canvas frame chrome, etc.). Mirrors Python's
    // `await self.emit(self.id, {type:"agent_deleted", id})`.
    if let Some(root) = kernel.root() {
        kernel
            .emit(&root.id, json!({"type": "agent_deleted", "id": id.0}))
            .await;
    }
    json!({ "deleted": true, "id": id.0 })
}

fn subtree_ids(kernel: &Arc<Kernel>, agent: &Arc<Agent>, out: &mut Vec<AgentId>) {
    out.push(agent.id.clone());
    for cid in agent.child_ids() {
        if let Some(c) = kernel.agents.get(&cid).map(|e| Arc::clone(&e)) {
            subtree_ids(kernel, &c, out);
        }
    }
}

/// Depth-first delete: each leaf's `on_delete` hook fires before its
/// parent's; the agent unregisters from `kernel.agents` and parent's
/// `children` map only AFTER its hook completes successfully.
pub(crate) async fn cascade_delete(kernel: &Arc<Kernel>, target: &Arc<Agent>) {
    // Snapshot children first (since we mutate during iteration).
    let child_ids = target.child_ids();
    for cid in child_ids {
        if let Some(child) = kernel.agents.get(&cid).map(|e| Arc::clone(&e)) {
            Box::pin(cascade_delete(kernel, &child)).await;
        }
    }
    // on_delete hook. Python's exact stdout line (`_agent.py:2835`,
    // `print(f"  [cascade] {self.id} bundle on_delete raised: {e}")`) — a
    // hook that raises is logged and the cascade still continues; delete is
    // not refusable. The hook is the agent's own code, so the kernel runs it AS
    // THE AGENT (its own id is the sender), whoever asked for the delete — like
    // `on_cancel`.
    if let Some(hm) = target.handler_module.as_deref() {
        if let Some(bundle) = kernel.bundles.get(hm) {
            let hook = bundle.on_delete(&target.id, kernel);
            if let Err(e) = crate::send::as_agent(&target.id, hook).await {
                println!("  [cascade] {} bundle on_delete raised: {e}", target.id.0);
            }
        }
    }
    // Detach from parent's children map.
    if let Some(parent_id) = target.parent_id.as_ref() {
        if let Some(parent) = kernel.agents.get(parent_id).map(|e| Arc::clone(&e)) {
            parent.remove_child(&target.id);
        }
    }
    // Unregister + drop inbox.
    kernel.unregister(&target.id);
    // Disk cleanup THROUGH the discovered provider (its recursive `delete`
    // verb) — the substrate owns no `fs` surface here. No-op without a wired
    // provider or in InMemory mode (the dir, if any, was never ours to remove).
    // NO INLINE FORGET: the `removed` event below is what the root loader's
    // `persist_writer` turns into the provider's recursive `delete`.
    let event = json!({
        "agent_id": target.id.0,
        "kind": "removed",
        "name": target.name_or_id(),
    });
    kernel.publish_state(&event);
}

#[cfg(test)]
mod tests;
