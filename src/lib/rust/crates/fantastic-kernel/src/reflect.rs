//! `reflect` verb — one uniform shape on every agent.
//!
//! A reflect reply is the ADDRESSED agent's identity:
//! `{id, sentence, display_name, parent_id, handler_module,
//! description?, ...flat meta}` (bundle agents answer it from their own
//! handler; bare agents via [`reflect_identity`]). The composable flags
//! are appended uniformly by [`apply_reflect_flags`] in `send`:
//!
//! - `detail=distilled|full` (default distilled) — distilled carries the
//!   gated ROSTER and the `more` hint; full carries the wiring tree.
//! - `tree=all|ids|none` (default `all`) — the nested subtree / the flat
//!   descendant-id index, paged with `next`; `none` omits it.
//!   `handler_modules=[names]` keeps only the agents whose `handler_module` is
//!   listed (an agent of a role, a meta bundle, is listed under its base as well
//!   as under its own name); `tree` is then a flat array (ids, or nodes without
//!   `children`).
//! - `bundles=all|ids` (default none) — the INSTALLED `{name, handler_module}`
//!   catalog / its names, paged with `bundles_next`.
//!
//! - `tier=oneliners|detailed`, `verb=<name>`, `part=docs|schema|all` — HOW TO
//!   CALL the agent: its verb docs and schemas (`docs::section`), in
//!   place of the one-line `verbs` block.
//!
//! The flags are the typed arguments of the declared `reflect`
//! ([`crate::declared::common::Reflect`]).
//!
//! VISIBLE = WHAT YOU CAN (grant model rule 9). One shape for every caller, cut
//! to what the caller holds on the target (`Sight`): the operator and an agent
//! reflecting its own tree see everything; `call` shows the identity keys and the
//! verbs it holds; `update` adds the values of the fields it holds; `create` adds
//! the bundles it may create. The tree, at every level, lists only the agents the
//! caller covers.

use crate::agent::Agent;
use crate::agent::AgentId;
use crate::authz::solver::Act;
use crate::declared::common::Reflect;
use crate::declared::{BundlesKind, Detail, TreeKind};
use crate::kernel::Kernel;
use crate::outcome::Status;
use crate::send::gate::{sample_id, Candidates, Held, PROBE};
use serde_json::{json, Map, Value};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// PURPOSE, not wiring — the reflect keys a roster node copies beyond its
/// `sentence` (python `_PURPOSE_KEYS`).
const PURPOSE_KEYS: [&str; 1] = ["mode"];

/// The IDENTITY keys: shown to every caller that holds anything on the agent.
/// Every other key a reflect carries is a field (a bundle's free keys too).
const IDENTITY_KEYS: [&str; 6] = [
    "id",
    "display_name",
    "parent_id",
    "handler_module",
    "description",
    "view",
];

/// The keys the tree walk itself writes into a node: never a field.
const WALK_KEYS: [&str; 2] = ["children", "pruned"];

/// WHO IS LOOKING, and so what a reflect shows. The operator (no viewer) sees
/// everything; an agent sees each agent at the level it holds there
/// ([`Kernel::held_from`]), its probe candidates computed once per reflect.
pub(crate) struct Sight {
    kernel: Arc<Kernel>,
    viewer: Option<(AgentId, Arc<Candidates>)>,
}

impl Sight {
    /// The sight of `caller` (`None`: the operator).
    pub(crate) fn of(kernel: &Arc<Kernel>, caller: Option<&AgentId>) -> Self {
        Self {
            kernel: Arc::clone(kernel),
            viewer: caller.map(|c| (c.clone(), kernel.candidates_for(c))),
        }
    }

    /// What the viewer holds on `id` (`Own` for the operator).
    pub(crate) fn held(&self, id: &AgentId) -> Held {
        match &self.viewer {
            None => Held::Own,
            Some((c, cands)) => self.kernel.held_from(c, id, cands),
        }
    }

    /// Does the viewer cover `a` (hold anything on it)?
    pub(crate) fn sees(&self, a: &Agent) -> bool {
        self.held(&a.id) != Held::Nothing
    }

    /// May the viewer, holding `held` on `a`, see its key `key`? An identity
    /// key always; a field only when the viewer holds `update` on it.
    fn shows_key(&self, a: &Agent, held: Held, key: &str) -> bool {
        if held == Held::Own || IDENTITY_KEYS.contains(&key) {
            return true;
        }
        let Some((c, _)) = &self.viewer else {
            return true;
        };
        held >= Held::Update && self.kernel.allowed(c, &Act::Update(key.to_string()), a)
    }

    /// Cut a RECORD of `a` to what the viewer holds there, as a tree node is cut:
    /// `None` when the viewer holds nothing on it, else its record with only the
    /// keys the viewer may see.
    fn cut_record(&self, a: &Agent) -> Option<crate::agent::AgentRecord> {
        self.cut(a, a.record())
    }

    /// Cut `rec`, a record of `a`, to what the viewer holds on `a`: `None` when
    /// it holds nothing there.
    fn cut(&self, a: &Agent, mut rec: crate::agent::AgentRecord) -> Option<crate::agent::AgentRecord> {
        let held = self.held(&a.id);
        if held == Held::Nothing {
            return None;
        }
        rec.meta.retain(|k, _| self.shows_key(a, held, k));
        Some(rec)
    }

    /// Cut a TREE NODE of `a` to what the viewer holds there: its identity, the
    /// fields it holds and the walk's own keys.
    fn cut_node(&self, a: &Agent, held: Held, node: &mut Map<String, Value>) {
        if held == Held::Own {
            return;
        }
        node.retain(|k, _| WALK_KEYS.contains(&k.as_str()) || self.shows_key(a, held, k));
    }

    /// May the viewer call `verb` on `a`?
    fn may_call(&self, a: &Agent, verb: &str) -> bool {
        match &self.viewer {
            None => true,
            Some((c, _)) => self.kernel.allowed(c, &Act::Call(verb.to_string()), a),
        }
    }

    /// May the viewer create an agent of `bundle` under `a`, with any id its
    /// lines select?
    fn may_create(&self, a: &Agent, bundle: &str) -> bool {
        let Some((c, cands)) = &self.viewer else {
            return true;
        };
        cands.whos.iter().any(|w| {
            self.kernel.allowed(
                c,
                &Act::Create {
                    bundle: bundle.to_string(),
                    new_id: sample_id(w),
                },
                a,
            )
        })
    }

    /// Is `bundle` a literal the viewer's lines (or its chain's) name?
    fn names_bundle(&self, bundle: &str) -> bool {
        match &self.viewer {
            None => true,
            Some((_, cands)) => cands.bundles.contains(bundle),
        }
    }

    /// The agents shown directly below `a`, in creation order, with what the
    /// viewer holds on each: each child it covers, and for a child it does not,
    /// that child's own shown agents, LIFTED (they keep their real parent).
    fn shown_below(&self, a: &Agent) -> Vec<(Arc<Agent>, Held)> {
        let mut out = Vec::new();
        for c in a.children_snapshot() {
            let held = self.held(&c.id);
            if held == Held::Nothing {
                out.extend(self.shown_below(&c));
            } else {
                out.push((c, held));
            }
        }
        out
    }
}

/// A LIFTED entry (shown under an agent that is not its parent) names its real
/// `parent_id`.
fn mark_lifted(a: &Agent, under: &Agent, entry: &mut Value) {
    if a.parent_id.as_ref() == Some(&under.id) {
        return;
    }
    if let Some(o) = entry.as_object_mut() {
        o.entry("parent_id".to_string()).or_insert_with(|| {
            a.parent_id
                .as_ref()
                .map(|p| json!(p.0))
                .unwrap_or(Value::Null)
        });
    }
}

/// Uniform reflect identity for a bare agent (the root, or any node with
/// no handler_module). Bundle agents build their own identity in their
/// handler; the substrate appends tree/bundles to BOTH via
/// [`apply_reflect_flags`], so root is not special-cased.
pub fn reflect_identity(target: &Arc<Agent>) -> Value {
    let mut obj = Map::new();
    obj.insert("id".to_string(), json!(target.id.0));
    obj.insert("sentence".to_string(), json!(sentence_for(target)));
    obj.insert(
        "parent_id".to_string(),
        target
            .parent_id
            .as_ref()
            .map(|p| json!(p.0))
            .unwrap_or(Value::Null),
    );
    obj.insert(
        "handler_module".to_string(),
        target
            .handler_module
            .as_ref()
            .map(|s| json!(s))
            .unwrap_or(Value::Null),
    );
    obj.insert("display_name".to_string(), json!(target.name_or_id()));
    if let Some(d) = target.description() {
        obj.insert("description".to_string(), json!(d));
    }
    // Flatten current meta into the reply for visibility.
    if let Ok(meta) = target.meta.read() {
        for (k, v) in meta.iter() {
            if obj.contains_key(k) {
                continue;
            }
            obj.insert(k.clone(), v.clone());
        }
    }
    Value::Object(obj)
}

fn sentence_for(target: &Arc<Agent>) -> &'static str {
    if target.parent_id.is_none() {
        "Fantastic kernel. Everything is reachable by sending messages to agents."
    } else {
        "Bare agent (no handler_module) — answers substrate verbs only."
    }
}

/// `{id, sentence?, description?, mode?, children:[...]}` — the tree, carrying
/// what each agent is FOR instead of how it is wired (python `_roster`).
///
/// The sentence is ASKED FROM THE AGENT ITSELF: a node with a handler is
/// reflected through [`Kernel::internal_dispatch`] (a real call — row, task,
/// watcher echo — but no reflect flags, so a roster never recurses into
/// rosters); a bare node answers its identity directly.
///
impl Kernel {
    /// The records of `ids` cut to what `caller` holds on each, the way `reflect`
    /// cuts the tree (rule 9): an agent the caller holds nothing on is left out
    /// (a missing id too), every other record keeps only the keys the caller may
    /// see. The operator (`None`) gets every record whole. For a verb that hands
    /// its asker records (`export_subtree`, Q39).
    pub fn records_for(
        self: &Arc<Self>,
        caller: Option<&AgentId>,
        ids: &[AgentId],
    ) -> Vec<crate::agent::AgentRecord> {
        let sight = Sight::of(self, caller);
        ids.iter()
            .filter_map(|id| self.agents.get(id).map(|e| Arc::clone(&e)))
            .filter_map(|a| sight.cut_record(&a))
            .collect()
    }

    /// `rec`, a record of `a`, cut to what `viewer` holds on `a` (rule 9, as
    /// [`Self::records_for`] cuts): `None` when the viewer holds nothing there.
    /// The operator (`None`) gets it whole. For a reply or an event that carries
    /// a record to someone other than the agent itself.
    pub(crate) fn record_cut_for(
        self: &Arc<Self>,
        viewer: Option<&AgentId>,
        a: &Agent,
        rec: crate::agent::AgentRecord,
    ) -> Option<crate::agent::AgentRecord> {
        Sight::of(self, viewer).cut(a, rec)
    }

    /// `rec` with its identity keys only: what a receiver the kernel cannot place
    /// (a watcher that is no agent, the state stream) may see of it.
    pub(crate) fn identity_record(mut rec: crate::agent::AgentRecord) -> crate::agent::AgentRecord {
        rec.meta.retain(|k, _| IDENTITY_KEYS.contains(&k.as_str()));
        rec
    }
}

/// The tree lists only the agents the viewer covers (`Sight`), each node cut
/// to what the viewer holds there; a node that is cut is never asked.
///
/// `Err` carries a node's RAISED reflect: python's exception propagates out of
/// `_roster` and fails the whole reflect.
fn roster(
    sight: Arc<Sight>,
    a: Arc<Agent>,
    held: Held,
    ignore: Arc<std::collections::HashSet<String>>,
) -> Pin<Box<dyn Future<Output = Result<Value, Value>> + Send>> {
    Box::pin(async move {
        let mut node = roster_node(&sight.kernel, &a).await?;
        sight.cut_node(&a, held, &mut node);
        let mut children = Vec::new();
        for (c, c_held) in sight.shown_below(&a) {
            let mut entry = if ignore.contains(&c.id.0) {
                crate::docs::pruned_stub(&c, &sight)
            } else {
                roster(
                    Arc::clone(&sight),
                    Arc::clone(&c),
                    c_held,
                    Arc::clone(&ignore),
                )
                .await?
            };
            mark_lifted(&c, &a, &mut entry);
            children.push(entry);
        }
        node.insert("children".to_string(), Value::Array(children));
        Ok(Value::Object(node))
    })
}

/// One distilled roster node WITHOUT its `children`: the keys [`roster`] and the
/// `handler_modules` walk share. `Err` carries the node's RAISED reflect.
async fn roster_node(kernel: &Arc<Kernel>, a: &Arc<Agent>) -> Result<Map<String, Value>, Value> {
    {
        let raw = if a.handler_module.is_some() {
            let (reply, status) = kernel
                .internal_dispatch(a, &json!({"type": "reflect"}))
                .await;
            if status == Status::Failed {
                return Err(reply);
            }
            reply
        } else {
            reflect_identity(a)
        };
        let mut node = Map::new();
        node.insert("id".to_string(), json!(a.id.0));
        if let Some(sentence) = raw
            .get("sentence")
            .filter(|v| v.as_str().is_some_and(|s| !s.is_empty()))
        {
            node.insert("sentence".to_string(), sentence.clone());
        }
        // Operator-set purpose — more specific than the bundle sentence.
        if let Some(d) = a.description() {
            node.insert("description".to_string(), json!(d));
        }
        for k in PURPOSE_KEYS {
            if let Some(v) = raw.get(k).filter(|v| !v.is_null()) {
                node.insert(k.to_string(), v.clone());
            }
        }
        // A TASK LINE is state the agent was given (a bundle that spawns it or takes
        // its report), not a purpose its bundle states: the tree reads it off the record,
        // so a lead sees its subtree's plan and progress in one `reflect tree=all`.
        if let Some(t) = a
            .meta
            .read()
            .ok()
            .and_then(|m| m.get("task").filter(|t| !t.is_null()).cloned())
        {
            node.insert("task".to_string(), t);
        }
        Ok(node)
    }
}

/// Append the composable reflect flags to any reflect reply — applied
/// uniformly to bare-agent and bundle reflects (python
/// `_apply_reflect_flags`). Non-object replies and refusals pass through.
///
/// Key order is python's: identity, `description`, gate fields, `view`, the root
/// frame, `tree`, `next`, `bundles`, `bundles_next`, `verbs`. With `tier`, `verb`
/// or `part` the verbs come as their docs (`tier`, `verbs`, `man`, `schemas`,
/// `docs::section`), cut to the same verbs; a `verb` it does not show is
/// refused naming only the shown ones.
///
/// `Err` = a roster node's reflect raised; the caller settles the whole
/// reflect `failed` with that reply.
///
/// Under `handler_modules` the `tree` is a flat array of the listed agents (key
/// order unchanged).
///
/// CUT BY LEVEL. `caller` is the sender as the gate judged it (`None`: the
/// operator). The operator, and an agent reflecting its own tree, get the whole
/// reply. Any other caller gets what it holds on `target`: at `call` the
/// identity keys and the verbs it may call; at `update` also the values of the
/// fields it holds (`can` only when held); at `create` also the bundles it may
/// create (the `bundles` flag). The root's runtime frame is the operator's only.
/// The tree, at every level, lists the agents the caller covers; a hidden node
/// is omitted and its shown agents are lifted with their real `parent_id`. A
/// caller holding nothing gets the unknown answer.
pub async fn apply_reflect_flags(
    kernel: &Arc<Kernel>,
    caller: Option<&AgentId>,
    target: &Arc<Agent>,
    args: &Reflect,
    reply: &mut Value,
) -> Result<(), Value> {
    let Some(obj) = reply.as_object_mut() else {
        return Ok(());
    };
    // A REFUSAL IS NOT A REFLECT. Decorating one with a tree
    // produces a reply that reads like a healthy reflect with a stray error
    // field, and a caller could not tell the two apart.
    if obj.contains_key("error") {
        return Ok(());
    }
    let sight = Arc::new(Sight::of(kernel, caller));
    let held = sight.held(&target.id);
    if held == Held::Nothing {
        *reply = crate::refusal::unknown_agent(target.id.as_str());
        return Ok(());
    }
    // The whole reply: the operator, or the caller's own tree.
    let whole = held == Held::Own;
    let full = args.detail == Some(Detail::Full);
    // THE IDENTITY KEYS on every reflect: a bundle handler builds its own reply
    // and may leave them out, so the substrate fills the ones it did not set.
    obj.entry("id".to_string())
        .or_insert_with(|| json!(target.id.0));
    obj.entry("parent_id".to_string()).or_insert_with(|| {
        target
            .parent_id
            .as_ref()
            .map(|p| json!(p.0))
            .unwrap_or(Value::Null)
    });
    obj.entry("handler_module".to_string()).or_insert_with(|| {
        target
            .handler_module
            .as_ref()
            .map(|s| json!(s))
            .unwrap_or(Value::Null)
    });
    obj.entry("display_name".to_string())
        .or_insert_with(|| json!(target.name_or_id()));
    // `description` is a substrate meta field — surface it on every
    // reflect (bundle handlers don't know about it) unless already set.
    if !obj.contains_key("description") {
        if let Some(d) = target.description() {
            obj.insert("description".to_string(), json!(d));
        }
    }
    // THE AGENT'S LINES on its OWN reflect. A bundle agent builds its own
    // identity and would otherwise never report its authority, so an agent
    // could not read what it may do — only discover it by trying. The same `can`
    // key the distilled tree nodes carry.
    if let Ok(meta) = target.meta.read() {
        if let Some(v) = meta.get("can") {
            obj.entry("can".to_string()).or_insert_with(|| v.clone());
        }
        // HOW THIS AGENT IS DRAWN. `reflect` is the one place any client learns
        // it, so a bundle agent's record `view` rides its reflect like the gate
        // fields do (a bare agent already flattens its meta). A reply that
        // built its own `view` keeps it.
        if let Some(v) = meta.get("view") {
            obj.entry("view".to_string()).or_insert_with(|| v.clone());
        }
    }
    // ONLY WHAT THE CALLER HOLDS: the identity keys, and the fields it may update.
    if !whole {
        obj.retain(|k, _| sight.shows_key(target, held, k));
    }
    // Kernel runtime identity + deployment context — surfaced on the ROOT
    // reflect so a client that hops to this kernel learns, in one round-trip:
    // which runtime (`runtime`), WHERE it runs (`env` — "container" when
    // launched from the image, else "host"), and which build (`version`).
    // env/version come from the optional FANTASTIC_ENV / FANTASTIC_VERSION
    // envs the container bakes in; RUN-scoped (never persisted to the portable
    // .fantastic workdir). Same field names + key order (runtime → env →
    // version) across all four runtimes. The OPERATOR'S only: an agent holding a
    // line on the root sees no peers, env, version or guid.
    if whole && target.parent_id.is_none() {
        obj.insert("runtime".to_string(), json!("rust"));
        // The kernels this one is CONNECTED to — each agent there is reachable
        // as `<guid>|<agent id>` (reflect `<guid>|kernel` for its tree).
        obj.insert("peers".to_string(), kernel.peer_rows());
        obj.insert(
            "env".to_string(),
            json!(std::env::var("FANTASTIC_ENV").unwrap_or_else(|_| "host".to_string())),
        );
        obj.insert(
            "version".to_string(),
            json!(std::env::var("FANTASTIC_VERSION").ok()),
        );
        // DUCK-TYPED ROOT EXTENSION (python `_agent.py:1697-1709`). The root's
        // handler_module — the kernel_state loader — contributes root-only
        // fields the substrate cannot compute without coupling to bundle logic:
        // `persistence: {provider}`, the instance `guid`, the doc-tree frame
        // and the `self_extension` shape. No bundle import here: the registry
        // already holds it, and a root whose bundle is not installed in this
        // runtime simply contributes nothing. A hook that RAISES fails the
        // reflect.
        if let Some(bundle) = target
            .handler_module
            .as_deref()
            .and_then(|hm| kernel.bundles.get(hm))
        {
            if let Some(extra) = bundle.reflect_root_extra(target, full, kernel)? {
                for (k, v) in extra {
                    obj.insert(k, v);
                }
            }
        }
    }
    let tree_mode = match args.tree.unwrap_or(TreeKind::All) {
        TreeKind::All => Some("all"),
        TreeKind::Ids => Some("ids"),
        TreeKind::None => None,
    };
    if let Some(mode) = tree_mode {
        // SUBTREE FILTERS: `show_subtrees` answers with only
        // those branches, `ignore_subtrees` prunes each named branch to a stub
        // `{id, pruned: n}`; together, show first and ignore inside it.
        let (show, ignore) = match crate::docs::subtree_filter(kernel, &sight, args, target) {
            Ok(f) => f,
            Err(refused) => {
                *reply = refused;
                return Ok(());
            }
        };
        // HANDLER FILTER: a list of handler names; `tree` then lists only the
        // agents whose record `handler_module`, or the base of its role (a meta
        // bundle), is one of them. Absent = no filter. Checked here, on the record,
        // before any node is asked.
        let handlers: Option<Vec<String>> = args.handler_modules.clone();
        let roots: Vec<Arc<Agent>> = if show.is_empty() {
            vec![Arc::clone(target)]
        } else {
            show.clone()
        };
        if let (Some(names), "all") = (&handlers, mode) {
            let mut nodes = Vec::new();
            for r in &roots {
                let mut stack = vec![Arc::clone(r)];
                while let Some(a) = stack.pop() {
                    // a hidden agent is not listed; the ones below it still are
                    let a_held = sight.held(&a.id);
                    if a_held != Held::Nothing {
                        if ignore.contains(&a.id.0) {
                            nodes.push(crate::docs::pruned_stub(&a, &sight));
                            continue;
                        }
                        if handler_listed(a.handler_module.as_deref(), names, &kernel.bundles) {
                            let mut node = if full {
                                let mut node = Map::new();
                                node.insert("id".to_string(), json!(a.id.0));
                                tree_fields(&a, &mut node);
                                node
                            } else {
                                roster_node(kernel, &a).await?
                            };
                            sight.cut_node(&a, a_held, &mut node);
                            nodes.push(Value::Object(node));
                        }
                    }
                    stack.extend(a.children_snapshot().into_iter().rev());
                }
            }
            obj.insert("tree".to_string(), Value::Array(nodes));
        } else if mode == "all" {
            let mut nodes = Vec::new();
            for r in &roots {
                let r_held = sight.held(&r.id);
                nodes.push(if ignore.contains(&r.id.0) {
                    crate::docs::pruned_stub(r, &sight)
                } else if full {
                    tree_node_pruned(&sight, r, r_held, &ignore)
                } else {
                    roster(
                        Arc::clone(&sight),
                        Arc::clone(r),
                        r_held,
                        Arc::new(ignore.clone()),
                    )
                    .await?
                });
            }
            let tree = if show.is_empty() {
                nodes.remove(0)
            } else {
                Value::Array(nodes)
            };
            obj.insert("tree".to_string(), tree);
        } else {
            // The FLAT id index is the one tree shape that is a plain list, so it
            // is the one that pages. The nested tree is never sliced — a page of
            // it would be a forest with no root. A pruned branch stands as its stub.
            let flat: Vec<Value> = roots
                .iter()
                .flat_map(|r| {
                    descendant_entries(r, &sight, &ignore, handlers.as_deref(), &kernel.bundles)
                })
                .collect();
            let (ids, next) =
                crate::paging::page(flat, args.limit, args.after.as_deref(), |v| match v {
                    Value::String(s) => s.clone(),
                    other => other
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                });
            obj.insert("tree".to_string(), Value::Array(ids));
            if let Some(next) = next {
                obj.insert("next".to_string(), json!(next));
            }
        }
    }
    // The catalog pages with the SAME `limit`/`after` as `tree=ids`, and names
    // its cursor `bundles_next` only when a further page exists. Below the whole
    // reply it shows only at `create`, cut to the bundles the caller may create.
    let catalog = |schemas: bool| -> Vec<Value> {
        let all = available_bundles(kernel, schemas);
        if whole {
            all
        } else {
            creatable(&sight, target, all)
        }
    };
    let bundles_flag = if whole || held == Held::Create {
        args.bundles
    } else {
        None
    };
    match bundles_flag {
        Some(BundlesKind::All) => {
            let all = catalog(true);
            let listed: Vec<Value> = match args.bundle_names.as_deref() {
                Some(names) => all
                    .into_iter()
                    .filter(|b| {
                        b.get("name")
                            .and_then(Value::as_str)
                            .is_some_and(|n| names.iter().any(|w| w == n))
                    })
                    .collect(),
                None => all,
            };
            let (cat, next) = crate::paging::page(listed, args.limit, args.after.as_deref(), |b| {
                b.get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            });
            obj.insert("bundles".to_string(), Value::Array(cat));
            if let Some(next) = next {
                obj.insert("bundles_next".to_string(), json!(next));
            }
        }
        Some(BundlesKind::Ids) => {
            let names: Vec<String> = catalog(false)
                .iter()
                .filter_map(|b| b.get("name").and_then(Value::as_str).map(str::to_string))
                .collect();
            let (names, next) =
                crate::paging::page(names, args.limit, args.after.as_deref(), Clone::clone);
            obj.insert("bundles".to_string(), json!(names));
            if let Some(next) = next {
                obj.insert("bundles_next".to_string(), json!(next));
            }
        }
        None => {}
    }
    let bundle = target
        .handler_module
        .as_deref()
        .and_then(|hm| kernel.bundles.get(hm));
    // THE VERBS, rendered from the declarations: the agent's own (its bundle's) and the
    // ones every agent answers; the root answers the kernel's too. A bare agent below
    // the root has none. With `tier`, `verb` or `part` they come as the verb docs
    // (`docs::section`).
    if bundle.is_some() || target.parent_id.is_none() {
        let dynamic = bundle
            .as_ref()
            .map(|b| b.dynamic_verbs(target))
            .unwrap_or_default();
        let lists = verb_lists(target, bundle.as_deref());
        // below the whole reply, only the verbs the caller may call
        let shown: Vec<crate::verbs::Row> = crate::verbs::rows(&lists, &dynamic)
            .into_iter()
            .filter(|r| whole || sight.may_call(target, r.name))
            .collect();
        let man = bundle.as_ref().map(|b| b.man());
        match crate::docs::section(&target.id.0, args, &shown, man) {
            Ok(section) => {
                for (k, v) in section {
                    obj.insert(k, v);
                }
            }
            Err(refused) => {
                *reply = refused;
                return Ok(());
            }
        }
    }
    Ok(())
}

/// The declared verb lists `target` answers, in precedence order: the kernel's on
/// the root, its bundle's, then the ones every agent answers (none for a bare agent
/// below the root).
pub(crate) fn verb_lists(
    target: &Agent,
    bundle: Option<&dyn crate::bundle::Bundle>,
) -> Vec<&'static [crate::verbs::Verb]> {
    let mut lists: Vec<&'static [crate::verbs::Verb]> = Vec::new();
    if target.parent_id.is_none() {
        lists.push(crate::declared::kernel::VERBS);
    }
    if let Some(b) = bundle {
        lists.push(b.verbs());
    }
    if bundle.is_some() || target.parent_id.is_none() {
        lists.push(crate::declared::common::VERBS);
    }
    lists
}

/// Walk an agent and its descendants, producing a nested
/// `{id, parent_id, handler_module, display_name, description?, ...posture,
/// children:[...]}` dict — the `detail:full` wiring tree (python `_tree`),
/// children in creation order.
///
/// A reflect that passed the gate returns it WHOLE — no per-node hiding.
pub fn tree_node(target: &Arc<Agent>) -> Value {
    let mut obj = Map::new();
    obj.insert("id".to_string(), json!(target.id.0));
    tree_fields(target, &mut obj);
    // CREATION ORDER, not sorted: python walks `_children`, a plain dict.
    let children: Vec<Value> = target.children_snapshot().iter().map(tree_node).collect();
    obj.insert("children".to_string(), Value::Array(children));
    Value::Object(obj)
}

/// One reflectable node's fields after its `id` (python `_node_summary`).
fn tree_fields(target: &Agent, obj: &mut Map<String, Value>) {
    if let Some(p) = target.parent_id.as_ref() {
        obj.insert("parent_id".to_string(), json!(p.0));
    } else {
        obj.insert("parent_id".to_string(), Value::Null);
    }
    obj.insert(
        "handler_module".to_string(),
        match &target.handler_module {
            Some(s) => json!(s),
            None => Value::Null,
        },
    );
    obj.insert("display_name".to_string(), json!(target.name_or_id()));
    if let Some(d) = target.description() {
        obj.insert("description".to_string(), json!(d));
    }
    // Wiring/posture meta surfaced inline (mirrors py _POSTURE_KEYS in kernel/_agent.py):
    // a client reads the IO landscape — which legs are open vs sealed-by-default, what a
    // bridge exposes — from ONE root reflect. Same keys + order across runtimes.
    if let Ok(meta) = target.meta.read() {
        for k in [
            "ingress_rule",
            "egress_rule",
            "auth",
            "root",
            // what a file_bridge exposes under its root
            "allow",
            // THE LINES, made readable. Without them an agent cannot read its
            // own authority — only discover it by trying, which cost whole
            // passes of empirical probing every run. They also make the
            // replace-not-merge warning on `update_agent` actionable: you must
            // send `can` WHOLE, so you must be able to read it.
            "can",
        ] {
            if let Some(v) = meta.get(k) {
                obj.insert(k.to_string(), v.clone());
            }
        }
    }
}

/// `tree_node` with `ignore`d branches pruned to their stub, as `sight` sees it:
/// only the agents it covers, each cut to what it holds there (`held` on
/// `target`), a hidden node's shown agents lifted.
fn tree_node_pruned(
    sight: &Sight,
    target: &Arc<Agent>,
    held: Held,
    ignore: &std::collections::HashSet<String>,
) -> Value {
    let mut obj = Map::new();
    obj.insert("id".to_string(), json!(target.id.0));
    tree_fields(target, &mut obj);
    sight.cut_node(target, held, &mut obj);
    let children: Vec<Value> = sight
        .shown_below(target)
        .into_iter()
        .map(|(c, c_held)| {
            if ignore.contains(&c.id.0) {
                let mut stub = crate::docs::pruned_stub(&c, sight);
                mark_lifted(&c, target, &mut stub);
                stub
            } else {
                tree_node_pruned(sight, &c, c_held, ignore)
            }
        })
        .collect();
    obj.insert("children".to_string(), Value::Array(children));
    Value::Object(obj)
}

/// `descendant_ids` as reply entries: an id in `ignore` stands as its stub
/// `{id, pruned: n}`, its branch unwalked. Python `_descendant_ids(ignore)`.
/// With `handlers`, an agent's own id is listed only when its handler, or the base
/// of its role, is named. An agent `sight` does not cover is not listed; the
/// ones below it are.
fn descendant_entries(
    target: &Agent,
    sight: &Sight,
    ignore: &std::collections::HashSet<String>,
    handlers: Option<&[String]>,
    bundles: &crate::bundle::BundleRegistry,
) -> Vec<Value> {
    let seen = sight.sees(target);
    if seen && ignore.contains(&target.id.0) {
        return vec![crate::docs::pruned_stub(target, sight)];
    }
    let listed = seen
        && handlers.is_none_or(|names| {
            handler_listed(target.handler_module.as_deref(), names, bundles)
        });
    let mut out = if listed {
        vec![json!(target.id.0)]
    } else {
        Vec::new()
    };
    for c in target.children_snapshot() {
        out.extend(descendant_entries(&c, sight, ignore, handlers, bundles));
    }
    out
}

/// Whether `names` holds the stored `handler`, or the base that handler (a role,
/// a meta bundle) resolves to. No intermediate link of a chain is matched.
fn handler_listed(
    handler: Option<&str>,
    names: &[String],
    bundles: &crate::bundle::BundleRegistry,
) -> bool {
    handler.is_some_and(|h| {
        names.iter().any(|n| n == h) || {
            let base = bundles.base_of(h);
            names.contains(&base)
        }
    })
}

/// Flat id index of an agent + all descendants (DFS, self first, children in
/// CREATION ORDER — python's dict walk), only the agents `sight` covers. The
/// cheap `tree=ids` tier.
pub(crate) fn descendant_ids(target: &Agent, sight: &Sight) -> Vec<String> {
    let mut out = if sight.sees(target) {
        vec![target.id.0.clone()]
    } else {
        Vec::new()
    };
    for c in target.children_snapshot() {
        out.extend(descendant_ids(&c, sight));
    }
    out
}

/// The INSTALLABLE-bundle catalog as `{name, handler_module}`, sorted by
/// name (python's entry-point scan) — unlisted fixtures are not in it. The
/// `bundles=all` tier (`bundles=ids` maps to just the names).
///
/// With `schemas`, an entry also carries `schema` when its bundle publishes a
/// [`crate::bundle::Bundle::record`] — what that KIND's record takes,
/// readable before any agent of it exists. Python
/// `_available_bundles(schemas=True)`; the cheap `bundles=ids` listing never asks.
fn available_bundles(kernel: &Arc<Kernel>, schemas: bool) -> Vec<Value> {
    let mut out: Vec<Value> = kernel
        .bundles
        .catalog()
        .map(|(handler_module, b)| {
            let mut entry = json!({
                "name": b.name(),
                "handler_module": handler_module,
            });
            if schemas {
                entry["schema"] = crate::verbs::record_fields(b.record());
            }
            entry
        })
        .collect();
    // FOLDER BUNDLES of this kernel's root, beside the installed ones — a refused
    // folder too, with its `error`.
    out.extend(kernel.bundles.folder_rows());
    out.sort_by(|a, b| {
        a.get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .cmp(b.get("name").and_then(Value::as_str).unwrap_or(""))
    });
    out
}

/// The catalog `rows` cut to the bundles `sight` may create under `target`: all
/// of them when its lines give `what: "*"` there, else the ones its lines name
/// (an installed bundle by its `handler_module`, a folder bundle by its name)
/// that it may create. A folder bundle counts once: a row whose name is already
/// listed is merged into that entry.
fn creatable(sight: &Sight, target: &Agent, rows: Vec<Value>) -> Vec<Value> {
    let star = sight.may_create(target, PROBE);
    let mut out: Vec<Value> = Vec::new();
    for row in rows {
        let Some(name) = row.get("name").and_then(Value::as_str).map(str::to_string) else {
            continue;
        };
        let bundle = row
            .get("handler_module")
            .and_then(Value::as_str)
            .map_or_else(|| name.clone(), str::to_string);
        if !star && !(sight.names_bundle(&bundle) && sight.may_create(target, &bundle)) {
            continue;
        }
        if let Some(prev) = out
            .iter_mut()
            .find(|p| p.get("name").and_then(Value::as_str) == Some(name.as_str()))
        {
            if let (Some(p), Value::Object(r)) = (prev.as_object_mut(), row) {
                for (k, v) in r {
                    p.entry(k).or_insert(v);
                }
            }
            continue;
        }
        out.push(row);
    }
    out
}

#[cfg(test)]
mod tests;
