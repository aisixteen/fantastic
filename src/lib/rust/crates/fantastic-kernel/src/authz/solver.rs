//! The solver index (grant model v9, #754): every local agent as a Cedar entity,
//! and each principal's `can` lines as its own Cedar policy set.
//!
//! - The entity is `Agent in [Agent] {id, bundle, parent?}`: `id` is an attribute
//!   because `like` needs a string, `bundle` is `handler_module` as stored (`""`
//!   for a bare agent), and the parents set is `{parent_id}`.
//! - A principal's set is the GENERIC policies (position, never upward, structure)
//!   followed by the policies its lines generate. The `written` set is the same
//!   without the position permit: what its record holds, for the writer check.
//!   The `wide` set keeps only the written lines that reach a target's whole
//!   tree: a handed-out `call`/`update` line on a literal id reaches that id's
//!   whole tree, so only such a hold may justify it.
//! - The index follows the tree by itself: `Kernel::register` and
//!   `Kernel::unregister` mark the entities dirty, and a principal's sets are
//!   rebuilt when its live `meta["can"]` differs from the value they were built
//!   from. One global set is never built.

use super::line::{parse_can, Level, Line, Target, What};
use crate::agent::{Agent, AgentId};
use crate::kernel::Kernel;
use cedar_policy::{
    Authorizer, Context, Decision, Entities, Entity, EntityId, EntityTypeName, EntityUid,
    PolicySet, Request, RestrictedExpression, Schema,
};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;

/// The Cedar schema of the grant model.
pub(crate) const SCHEMA: &str = r#"
entity Agent in [Agent] { id: String, bundle: String, parent?: Agent };
action call appliesTo { principal: Agent, resource: Agent, context: { verb: String } };
action update appliesTo { principal: Agent, resource: Agent, context: { field: String } };
action create appliesTo { principal: Agent, resource: Agent, context: { bundle: String, new_id: String } };
action delete appliesTo { principal: Agent, resource: Agent };
"#;

/// Position: every agent calls every verb on its own tree.
const POSITION: &str =
    "permit(principal, action == Action::\"call\", resource) when { resource in principal };\n";

/// Never upward (nothing changes or deletes its own record or an ancestor's), and
/// structure (`id`, `parent` and `handler_module` are not fields).
const FORBIDS: &str = "forbid(principal, action in [Action::\"update\", Action::\"delete\"], resource) when { principal in resource };\n\
forbid(principal, action == Action::\"update\", resource) when { [\"id\",\"parent\",\"handler_module\"].contains(context.field) };\n";

/// The GENERIC policies every principal's set starts with: position, never
/// upward, structure.
pub(crate) fn generic() -> String {
    format!("{POSITION}{FORBIDS}")
}

/// One act a principal asks for on a resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Act {
    /// Call one verb.
    Call(String),
    /// Change one field.
    Update(String),
    /// Create an agent of `bundle` with id `new_id` under the resource.
    Create {
        /// The new agent's bundle (`""` for a bare agent).
        bundle: String,
        /// The new agent's id.
        new_id: String,
    },
    /// Delete the resource.
    Delete,
}

/// Which of a principal's sets a check reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Basis {
    /// GENERIC + its lines: what a call is checked against.
    Whole,
    /// Its lines without position: what a writer holds.
    Written,
    /// Its written lines that reach a target's whole tree: no `$parent` line,
    /// and a `create` line only when its `who` is `*` and its bundles are
    /// unconstrained (on bundle `*` only, for `update`). What a writer holds
    /// for handing out a `call` or `update` line on a literal id.
    Wide,
}

/// The slot a partial evaluation leaves unknown: `cedar_all` answers whether every
/// value of it is allowed (a `*` slot).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Slot {
    /// The verb of a `call`.
    Verb,
    /// The field of an `update`.
    Field,
}

/// The solver index the kernel keeps beside its agents.
pub(crate) struct Index {
    /// One entity per local agent.
    pub(crate) entities: Entities,
    /// The agents changed since [`Self::entities`] was built.
    pub(crate) entities_dirty: bool,
    /// Per principal: the `can` its set was built from, and GENERIC + its lines.
    pub(crate) per: HashMap<AgentId, (Value, PolicySet)>,
    /// Per principal: the same lines without the position permit (the writer
    /// check holds only what is written).
    pub(crate) written: HashMap<AgentId, (Value, PolicySet)>,
    /// Per principal: the written lines that reach a whole tree ([`Basis::Wide`]).
    pub(crate) wide: HashMap<AgentId, (Value, PolicySet)>,
    /// Per caller: the probe candidates of [`Kernel::held`] (the literals of its
    /// own and its ancestors' lines), with the chain and the `can` of each agent
    /// on it they were built from. Reused while every `can` on the chain is the
    /// one it was built from; cleared at every register or unregister (a new
    /// chain, a new bundle).
    pub(crate) cands: HashMap<AgentId, CandEntry>,
    /// GENERIC only: the set of a principal with no lines.
    pub(crate) base: PolicySet,
    /// The Cedar authorizer.
    authorizer: Authorizer,
    /// The schema the entities are built against.
    schema: Schema,
    /// `Agent`.
    agent_type: EntityTypeName,
    /// `Action::"call"`, `"update"`, `"create"`, `"delete"`.
    actions: [EntityUid; 4],
}

/// One caller's cached probe candidates ([`Index::cands`]).
pub(crate) struct CandEntry {
    /// The caller and its ancestors, root-ward, each with the `can` the
    /// candidates were built from.
    pub(crate) chain: Vec<(AgentId, Value)>,
    /// The candidates.
    pub(crate) cands: Arc<crate::send::gate::Candidates>,
}

/// A Cedar string literal: `\`, `"` and control characters escaped.
fn q(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `[..].contains(attr)`, or no condition when the list holds `*`.
fn in_list(attr: &str, v: &[String]) -> Option<String> {
    if v.iter().any(|x| x == "*") {
        return None;
    }
    let items: Vec<String> = v.iter().map(|x| q(x)).collect();
    Some(format!("[{}].contains({attr})", items.join(",")))
}

/// `attr like who` for any `who`, or no condition when `who` holds `*`.
fn who_cond(attr: &str, who: &[String]) -> Option<String> {
    if who.iter().any(|x| x == "*") {
        return None;
    }
    if who.is_empty() {
        return Some("false".to_string());
    }
    let items: Vec<String> = who
        .iter()
        .map(|w| format!("{attr} like {}", q(w)))
        .collect();
    Some(format!("({})", items.join(" || ")))
}

/// One permit of `principal` for `action` on `scope`, when every condition holds.
fn permit(principal: &str, action: &str, scope: &str, conds: Vec<Option<String>>) -> String {
    let c: Vec<String> = conds.into_iter().flatten().collect();
    let when = if c.is_empty() {
        String::new()
    } else {
        format!(" when {{ {} }}", c.join(" && "))
    };
    format!(
        "permit(principal == Agent::{}, action == Action::\"{action}\", {scope}){when};\n",
        q(principal)
    )
}

/// The lines of a record's `can`: none when it is absent or outside the legal
/// shape (a record is refused at the write, so a bad `can` here holds nothing).
pub(crate) fn lines_of(can: &Value) -> Vec<Line> {
    if can.is_null() {
        return Vec::new();
    }
    parse_can(can).unwrap_or_default()
}

/// The policies `principal`'s lines generate (GENERIC not included).
pub(crate) fn line_policies(principal: &str, lines: &[Line]) -> String {
    let mut s = String::new();
    for l in lines {
        match (&l.target, &l.level) {
            (Target::Parent, Level::Call(verbs)) => {
                s += &permit(
                    principal,
                    "call",
                    "resource",
                    vec![
                        Some("principal has parent && resource == principal.parent".to_string()),
                        in_list("context.verb", verbs),
                    ],
                );
            }
            // `$parent` is call level only: the shape check refuses the rest
            (Target::Parent, _) => {}
            (Target::Id(t), Level::Call(verbs)) => {
                let scope = format!("resource in Agent::{}", q(t));
                s += &permit(
                    principal,
                    "call",
                    &scope,
                    vec![in_list("context.verb", verbs)],
                );
            }
            (Target::Id(t), Level::Update(fields)) => {
                let scope = format!("resource in Agent::{}", q(t));
                s += &permit(
                    principal,
                    "update",
                    &scope,
                    vec![in_list("context.field", fields)],
                );
                // `update` includes `call` on every verb
                s += &permit(principal, "call", &scope, vec![]);
            }
            (Target::Id(t), Level::Create { who, what }) => {
                s += &create_policies(principal, t, who, what);
            }
        }
    }
    s
}

/// The policies of `principal`'s lines that reach a target's whole tree
/// ([`Basis::Wide`]; GENERIC not included).
///
/// - `call`/`update` on a literal id: as [`line_policies`];
/// - `$parent`: none (it names one node);
/// - `create`: only with `who` `*`, and then for every bundle (`what: "*"` or a
///   `*` bundle key; only the `*` bundle's freed fields for `update`). A line
///   selecting some ids or some bundles holds single agents, never a tree.
pub(crate) fn wide_policies(principal: &str, lines: &[Line]) -> String {
    let mut s = String::new();
    for l in lines {
        match (&l.target, &l.level) {
            (Target::Parent, _) => {}
            (Target::Id(_), Level::Call(_) | Level::Update(_)) => {
                s += &line_policies(principal, std::slice::from_ref(l));
            }
            (Target::Id(t), Level::Create { who, what }) => {
                if !who.iter().any(|w| w == "*") {
                    continue;
                }
                let every = match what {
                    What::All => what.clone(),
                    What::Bundles(m) => match m.get("*") {
                        Some(fs) => What::Bundles([("*".to_string(), fs.clone())].into()),
                        None => continue,
                    },
                };
                s += &create_policies(principal, t, &["*".to_string()], &every);
            }
        }
    }
    s
}

/// The policies one `create` line on `t` generates.
///
/// - `create` anywhere in `t`'s tree, for the bundles of `what` and the ids of `who`;
/// - `call` (every verb) and `delete` on the agents strictly under `t` it selects:
///   a create line gives nothing on `t` itself;
/// - `update` of the fields each bundle of `what` frees (`"*"`) on those agents.
///   A forced field is never updatable, and `can` only when `what` frees it;
/// - no generated permit ever covers a bare agent (`bundle != ""`).
fn create_policies(principal: &str, t: &str, who: &[String], what: &What) -> String {
    let scope = format!("resource in Agent::{}", q(t));
    let below = Some(format!("resource != Agent::{}", q(t)));
    let names: Option<Vec<String>> = match what {
        What::All => None,
        What::Bundles(m) if m.contains_key("*") => None,
        What::Bundles(m) => Some(m.keys().cloned().collect()),
    };
    let bundle_in = |attr: &str| names.as_ref().and_then(|n| in_list(attr, n));
    let not_bare = |attr: &str| Some(format!("{attr} != \"\""));
    let mut s = permit(
        principal,
        "create",
        &scope,
        vec![
            bundle_in("context.bundle"),
            who_cond("context.new_id", who),
            not_bare("context.bundle"),
        ],
    );
    for action in ["call", "delete"] {
        s += &permit(
            principal,
            action,
            &scope,
            vec![
                below.clone(),
                bundle_in("resource.bundle"),
                who_cond("resource.id", who),
                not_bare("resource.bundle"),
            ],
        );
    }
    match what {
        What::All => {
            s += &permit(
                principal,
                "update",
                &scope,
                vec![
                    below.clone(),
                    who_cond("resource.id", who),
                    not_bare("resource.bundle"),
                ],
            );
        }
        What::Bundles(m) => {
            for (bundle, fields) in m {
                let freed: Vec<String> = fields
                    .iter()
                    .filter(|(_, v)| v.as_str() == Some("*"))
                    .map(|(f, _)| f.clone())
                    .collect();
                if freed.is_empty() {
                    continue;
                }
                let bundle_is = if bundle == "*" {
                    None
                } else {
                    Some(format!("resource.bundle == {}", q(bundle)))
                };
                s += &permit(
                    principal,
                    "update",
                    &scope,
                    vec![
                        below.clone(),
                        bundle_is,
                        in_list("context.field", &freed),
                        who_cond("resource.id", who),
                        not_bare("resource.bundle"),
                    ],
                );
            }
        }
    }
    s
}

/// A policy set from generated text; a text that does not parse is a programming
/// error, logged, and holds nothing.
fn parse_set(text: &str) -> PolicySet {
    text.parse().unwrap_or_else(|e| {
        eprintln!("  [kernel] grant policies do not parse: {e}");
        PolicySet::new()
    })
}

/// The string `v` as a Cedar value.
fn s(v: &str) -> RestrictedExpression {
    RestrictedExpression::new_string(v.to_string())
}

impl Index {
    /// An empty index: no entities yet (dirty, so the first check builds them).
    pub(crate) fn new() -> Self {
        let (schema, _) = Schema::from_cedarschema_str(SCHEMA).expect("grant schema parses");
        let action = |n: &str| -> EntityUid {
            EntityUid::from_type_name_and_id(
                EntityTypeName::from_str("Action").expect("Action is a type name"),
                EntityId::new(n),
            )
        };
        Index {
            entities: Entities::empty(),
            entities_dirty: true,
            per: HashMap::new(),
            written: HashMap::new(),
            wide: HashMap::new(),
            cands: HashMap::new(),
            base: parse_set(&generic()),
            authorizer: Authorizer::new(),
            schema,
            agent_type: EntityTypeName::from_str("Agent").expect("Agent is a type name"),
            actions: [
                action("call"),
                action("update"),
                action("create"),
                action("delete"),
            ],
        }
    }

    /// The entity uid of agent `id`.
    fn uid(&self, id: &str) -> EntityUid {
        EntityUid::from_type_name_and_id(self.agent_type.clone(), EntityId::new(id))
    }

    /// Rebuild the entities from `agents` (remote agents are never entities).
    fn build_entities(&mut self, agents: &[Arc<Agent>]) {
        let mut es = Vec::with_capacity(agents.len());
        for a in agents.iter().filter(|a| !a.is_remote()) {
            let mut attrs: HashMap<String, RestrictedExpression> = HashMap::new();
            attrs.insert("id".to_string(), s(a.id.as_str()));
            attrs.insert(
                "bundle".to_string(),
                s(a.handler_module.as_deref().unwrap_or("")),
            );
            let mut parents = HashSet::new();
            if let Some(p) = &a.parent_id {
                let pu = self.uid(p.as_str());
                attrs.insert(
                    "parent".to_string(),
                    RestrictedExpression::new_entity_uid(pu.clone()),
                );
                parents.insert(pu);
            }
            match Entity::new(self.uid(a.id.as_str()), attrs, parents) {
                Ok(e) => es.push(e),
                Err(e) => eprintln!("  [kernel] agent {} is no grant entity: {e}", a.id.as_str()),
            }
        }
        self.entities = Entities::from_entities(es, Some(&self.schema)).unwrap_or_else(|e| {
            eprintln!("  [kernel] grant entities do not build: {e}");
            Entities::empty()
        });
    }

    /// Rebuild `principal`'s two sets from `can`.
    fn build_sets(&mut self, principal: &AgentId, can: Value) {
        let lines = lines_of(&can);
        let text = line_policies(principal.as_str(), &lines);
        let per = if lines.is_empty() {
            self.base.clone()
        } else {
            parse_set(&format!("{}{text}", generic()))
        };
        let written = parse_set(&format!("{FORBIDS}{text}"));
        let wide = parse_set(&format!(
            "{FORBIDS}{}",
            wide_policies(principal.as_str(), &lines)
        ));
        self.per.insert(principal.clone(), (can.clone(), per));
        self.written
            .insert(principal.clone(), (can.clone(), written));
        self.wide.insert(principal.clone(), (can, wide));
    }

    /// The set `principal` is checked against on `basis`: for [`Basis::Whole`]
    /// its own, or the base when it has none.
    pub(crate) fn set_of(&self, principal: &AgentId, basis: Basis) -> Option<&PolicySet> {
        match basis {
            Basis::Whole => Some(self.per.get(principal).map_or(&self.base, |(_, ps)| ps)),
            Basis::Written => self.written.get(principal).map(|(_, ps)| ps),
            Basis::Wide => self.wide.get(principal).map(|(_, ps)| ps),
        }
    }

    /// Both ends are entities.
    fn known(&self, principal: &AgentId, resource: &AgentId) -> bool {
        self.entities.get(&self.uid(principal.as_str())).is_some()
            && self.entities.get(&self.uid(resource.as_str())).is_some()
    }

    /// Does `ps` allow `principal` to do `act` on `resource`? A principal or
    /// resource with no entity answers `false`.
    pub(crate) fn decide(
        &self,
        ps: &PolicySet,
        principal: &AgentId,
        act: &Act,
        resource: &AgentId,
    ) -> bool {
        if !self.known(principal, resource) {
            return false;
        }
        let (i, ctx) = match act {
            Act::Call(v) => (0, Context::from_pairs([("verb".to_string(), s(v))])),
            Act::Update(f) => (1, Context::from_pairs([("field".to_string(), s(f))])),
            Act::Create { bundle, new_id } => (
                2,
                Context::from_pairs([
                    ("bundle".to_string(), s(bundle)),
                    ("new_id".to_string(), s(new_id)),
                ]),
            ),
            Act::Delete => (3, Ok(Context::empty())),
        };
        let Ok(ctx) = ctx else { return false };
        let Ok(req) = Request::new(
            self.uid(principal.as_str()),
            self.actions[i].clone(),
            self.uid(resource.as_str()),
            ctx,
            None,
        ) else {
            return false;
        };
        self.authorizer
            .is_authorized(&req, ps, &self.entities)
            .decision()
            == Decision::Allow
    }

    /// Does `ps` allow `principal` every value of `slot` on `resource`? A partial
    /// evaluation with the verb (`call`) or the field (`update`) unknown.
    pub(crate) fn decide_all(
        &self,
        ps: &PolicySet,
        principal: &AgentId,
        slot: Slot,
        resource: &AgentId,
    ) -> bool {
        if !self.known(principal, resource) {
            return false;
        }
        let (i, key) = match slot {
            Slot::Verb => (0, "verb"),
            Slot::Field => (1, "field"),
        };
        let Ok(ctx) =
            Context::from_pairs([(key.to_string(), RestrictedExpression::new_unknown(key))])
        else {
            return false;
        };
        let Ok(req) = Request::new(
            self.uid(principal.as_str()),
            self.actions[i].clone(),
            self.uid(resource.as_str()),
            ctx,
            None,
        ) else {
            return false;
        };
        self.authorizer
            .is_authorized_partial(&req, ps, &self.entities)
            .decision()
            == Some(Decision::Allow)
    }
}

impl Kernel {
    /// Bring the index up to date for `principal`: the entities when an agent was
    /// registered or unregistered since they were built, and its sets when its
    /// live `can` differs from the one they were built from.
    fn refresh_authz(&self, principal: Option<&AgentId>) {
        let live = principal.and_then(|p| self.agents.get(p).map(|e| Arc::clone(&e)));
        {
            let ix = self.authz.read().expect("authz index poisoned");
            let fresh_sets = match (principal, &live) {
                (Some(p), Some(a)) => {
                    let meta = a.meta.read().expect("meta lock poisoned");
                    let can = meta.get("can").unwrap_or(&Value::Null);
                    ix.per.get(p).is_some_and(|(built, _)| built == can)
                }
                _ => true,
            };
            if fresh_sets && !ix.entities_dirty {
                return;
            }
        }
        let mut ix = self.authz.write().expect("authz index poisoned");
        if ix.entities_dirty {
            // cleared before the walk: a register racing it marks it dirty again
            ix.entities_dirty = false;
            let agents: Vec<Arc<Agent>> =
                self.agents.iter().map(|e| Arc::clone(e.value())).collect();
            ix.build_entities(&agents);
        }
        if let (Some(p), Some(a)) = (principal, live) {
            let can = a
                .meta
                .read()
                .expect("meta lock poisoned")
                .get("can")
                .cloned()
                .unwrap_or(Value::Null);
            if ix.per.get(p).map(|(built, _)| built) != Some(&can) {
                ix.build_sets(p, can);
            }
        }
    }

    /// Does `principal`'s own set (GENERIC + its lines) allow `act` on `resource`?
    /// No ancestor cap: that is the gate's. A principal or resource with no entity
    /// (a missing id, a remote id) answers `false`.
    #[cfg(test)]
    pub(crate) fn cedar(&self, principal: &AgentId, act: &Act, resource: &AgentId) -> bool {
        self.cedar_on(principal, act, resource, Basis::Whole)
    }

    /// [`Self::cedar`] against `principal`'s written set: its lines only, no
    /// position.
    #[cfg(test)]
    pub(crate) fn cedar_written(&self, principal: &AgentId, act: &Act, resource: &AgentId) -> bool {
        self.cedar_on(principal, act, resource, Basis::Written)
    }

    /// Does `principal`'s set on `basis` allow `act` on `resource`? No ancestor
    /// cap: that is the gate's. A principal or resource with no entity (a missing
    /// id, a remote id) answers `false`.
    pub(crate) fn cedar_on(
        &self,
        principal: &AgentId,
        act: &Act,
        resource: &AgentId,
        basis: Basis,
    ) -> bool {
        self.refresh_authz(Some(principal));
        let ix = self.authz.read().expect("authz index poisoned");
        ix.set_of(principal, basis)
            .is_some_and(|ps| ix.decide(ps, principal, act, resource))
    }

    /// Is every value of `slot` allowed to `principal` on `resource` (a `*` slot)?
    /// A partial evaluation with the verb or the field unknown, against the
    /// principal's set on `basis`.
    pub(crate) fn cedar_all(
        &self,
        principal: &AgentId,
        slot: Slot,
        resource: &AgentId,
        basis: Basis,
    ) -> bool {
        self.refresh_authz(Some(principal));
        let ix = self.authz.read().expect("authz index poisoned");
        ix.set_of(principal, basis)
            .is_some_and(|ps| ix.decide_all(ps, principal, slot, resource))
    }

    /// Is `res` in `anc`'s tree (`anc` itself included)?
    pub(crate) fn in_tree(&self, res: &AgentId, anc: &AgentId) -> bool {
        if res == anc {
            return true;
        }
        self.refresh_authz(None);
        let ix = self.authz.read().expect("authz index poisoned");
        ix.entities
            .is_ancestor_of(&ix.uid(anc.as_str()), &ix.uid(res.as_str()))
    }
}
