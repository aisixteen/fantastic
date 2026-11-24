//! THE CALL GATE — the caller's lines and the ancestor cap (grant model v9).
//!
//! The trust boundary is WHO ORIGINATED the send. The operator (origin `None`:
//! the in-process shell, the cold boot, kernel bookkeeping) is never checked.
//! Identity comes only from dispatch: a send made inside an agent's handler, or a
//! task it spawned, is that agent's. An AGENT send passes only when the CALLER's
//! own `can` lines allow it (its own tree it reaches by position) and every
//! ancestor of the caller whose tree does not hold the target allows it too. The
//! target keeps no list of callers. Every denial is the unknown answer,
//! byte-identical to a missing id.

use super::*;
use crate::authz::solver::{lines_of, Act, Basis, Slot};
use crate::authz::{Level, What};
use std::collections::BTreeSet;

/// What a caller holds on a target, lowest first (for reflect, ps, watch and
/// [`Kernel::holds`]; never for handing out lines). `Own`: the target is in the
/// caller's tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Held {
    /// Nothing: the target answers as an unknown agent.
    Nothing,
    /// Some verbs.
    Call,
    /// Some fields (and every verb).
    Update,
    /// Creating under it (and the agents it selects).
    Create,
    /// The caller's own tree.
    Own,
}

/// A value no line names: a create probe with it is allowed only where every
/// bundle is.
pub(crate) const PROBE: &str = "\u{1}probe";

/// An id a `who` pattern selects: a trailing `*` becomes a sentinel character,
/// which only a pattern with that prefix (or `*`) matches.
pub(crate) fn sample_id(who: &str) -> String {
    match who.strip_suffix('*') {
        Some(prefix) => format!("{prefix}\u{1}"),
        None => who.to_string(),
    }
}

/// The literals a level probe tries: the verbs, fields, bundles and `who`
/// patterns named in a set of agents' lines, plus those agents' own bundles
/// (proto `reflect_in`). `can` is always a field candidate and `*` always a
/// `who` candidate.
#[derive(Debug, Default)]
pub(crate) struct Candidates {
    pub(crate) verbs: BTreeSet<String>,
    pub(crate) fields: BTreeSet<String>,
    pub(crate) bundles: BTreeSet<String>,
    pub(crate) whos: BTreeSet<String>,
}

impl Candidates {
    /// Add one agent's bundle and the literals of its lines.
    fn add(&mut self, a: &Agent) {
        if let Some(b) = a.handler_module.as_deref() {
            self.bundles.insert(b.to_string());
        }
        let can = a
            .meta
            .read()
            .expect("meta lock poisoned")
            .get("can")
            .cloned()
            .unwrap_or(Value::Null);
        let named =
            |v: &[String]| -> Vec<String> { v.iter().filter(|x| *x != "*").cloned().collect() };
        for l in lines_of(&can) {
            match &l.level {
                Level::Call(v) => self.verbs.extend(named(v)),
                Level::Update(f) => self.fields.extend(named(f)),
                Level::Create { who, what } => {
                    self.whos.extend(who.iter().cloned());
                    if let What::Bundles(m) = what {
                        for (b, fs) in m {
                            if b != "*" {
                                self.bundles.insert(b.clone());
                            }
                            self.fields.extend(fs.keys().filter(|f| *f != "*").cloned());
                        }
                    }
                }
            }
        }
    }
}

impl Kernel {
    /// `a`'s ancestors, root-ward: its parent first, the root last. A loop in
    /// the parent links ends the walk.
    pub(crate) fn ancestor_chain(&self, a: &Agent) -> Vec<AgentId> {
        let mut out: Vec<AgentId> = Vec::new();
        let mut seen: HashSet<AgentId> = HashSet::new();
        let mut next = a.parent_id.clone();
        while let Some(pid) = next {
            if !seen.insert(pid.clone()) {
                break;
            }
            next = self.agents.get(&pid).and_then(|p| p.parent_id.clone());
            out.push(pid);
        }
        out
    }

    /// May `caller` do `act` on `target`? Cedar for the caller, then the cap:
    /// Cedar for every ancestor of the caller whose tree does not hold the
    /// target. The root holds every agent in its tree, so it is always skipped
    /// and needs no lines. A remote target is refused for every agent caller: no
    /// line can name one.
    pub(crate) fn allowed(&self, caller: &AgentId, act: &Act, target: &Agent) -> bool {
        self.capped_on(caller, act, target, Basis::Whole)
    }

    /// [`Self::allowed`] with the caller's written lines only (no position);
    /// every capping ancestor is still checked by its whole set. The writer check
    /// uses it: a writer hands out only what its written lines give.
    pub(crate) fn allowed_written(&self, caller: &AgentId, act: &Act, target: &Agent) -> bool {
        self.capped_on(caller, act, target, Basis::Written)
    }

    /// The basis a capping ancestor is checked on when the caller is checked on
    /// `basis`: its whole set, or its tree-wide lines for [`Basis::Wide`].
    fn cap_basis(basis: Basis) -> Basis {
        match basis {
            Basis::Wide => Basis::Wide,
            Basis::Whole | Basis::Written => Basis::Whole,
        }
    }

    /// The one cap loop: Cedar for `caller` on `basis`, then for every ancestor
    /// of it whose tree does not hold `target` ([`Self::cap_basis`]).
    /// [`Basis::Wide`] is the writer check's for a `call`/`update` line on a
    /// literal id: that line reaches the target's whole tree, so a hold on the
    /// target node alone (`$parent`, a create line's selection) never covers it,
    /// for the writer or for a capping ancestor.
    pub(crate) fn capped_on(
        &self,
        caller: &AgentId,
        act: &Act,
        target: &Agent,
        basis: Basis,
    ) -> bool {
        if target.is_remote() || !self.cedar_on(caller, act, &target.id, basis) {
            return false;
        }
        let Some(c) = self.agents.get(caller).map(|e| Arc::clone(&e)) else {
            return false;
        };
        let cap = Self::cap_basis(basis);
        self.ancestor_chain(&c)
            .iter()
            .all(|anc| self.in_tree(&target.id, anc) || self.cedar_on(anc, act, &target.id, cap))
    }

    /// Is every value of `slot` allowed to `caller` on `target`, capped like
    /// [`Self::allowed`]? A partial evaluation per agent in the chain, the
    /// caller on `basis` and each capping ancestor as in [`Self::capped_on`].
    pub(crate) fn allowed_all(
        &self,
        caller: &AgentId,
        slot: Slot,
        target: &Agent,
        basis: Basis,
    ) -> bool {
        if target.is_remote() || !self.cedar_all(caller, slot, &target.id, basis) {
            return false;
        }
        let Some(c) = self.agents.get(caller).map(|e| Arc::clone(&e)) else {
            return false;
        };
        let cap = Self::cap_basis(basis);
        self.ancestor_chain(&c)
            .iter()
            .all(|anc| self.in_tree(&target.id, anc) || self.cedar_all(anc, slot, &target.id, cap))
    }

    /// May `caller` call `verb` on `target`? The public form of
    /// [`Self::allowed`] with a call. A missing target answers `false`.
    pub fn may_call(&self, caller: &AgentId, verb: &str, target: &AgentId) -> bool {
        let Some(t) = self.agents.get(target).map(|e| Arc::clone(&e)) else {
            return false;
        };
        self.allowed(caller, &Act::Call(verb.to_string()), &t)
    }

    /// Does `caller` hold at least `level` on `target`? The solver's decision
    /// ([`Self::held`]) as a plain call for bundles: it knows agents and levels
    /// only, never what a bundle keeps for them. The operator (`None`) holds
    /// everything; a missing or remote target holds `Nothing`.
    pub fn holds(&self, caller: Option<&AgentId>, level: Held, target: &AgentId) -> bool {
        match caller {
            None => true,
            Some(c) => self.held(c, target) >= level,
        }
    }

    /// The probe candidates of `ids`' lines and bundles (missing ids add
    /// nothing).
    pub(crate) fn candidates_of(&self, ids: &[AgentId]) -> Candidates {
        let mut c = Candidates::default();
        c.fields.insert("can".to_string());
        c.whos.insert("*".to_string());
        for id in ids {
            if let Some(a) = self.agents.get(id).map(|e| Arc::clone(&e)) {
                c.add(&a);
            }
        }
        c
    }

    /// The highest level `caller` holds on `target` (for reflect, ps and watch;
    /// never for handing out lines). `Own` when the target is in the caller's
    /// tree; otherwise the highest level a probe finds, the candidates being the
    /// literals in the caller's and its ancestors' lines. A target with no entity
    /// (a missing id, a remote id) is `Nothing`.
    pub(crate) fn held(&self, caller: &AgentId, target: &AgentId) -> Held {
        let cands = self.candidates_for(caller);
        self.held_from(caller, target, &cands)
    }

    /// The probe candidates for `caller`: the literals of its own and its
    /// ancestors' lines (proto `reflect_local`). CACHED PER CALLER in the solver
    /// index: built once, and reused while the caller's chain is the same and
    /// every `can` on it is the one they were built from (compared, never parsed);
    /// a changed `can` rebuilds them, and every register or unregister clears
    /// them all.
    pub(crate) fn candidates_for(&self, caller: &AgentId) -> Arc<Candidates> {
        let mut ids = vec![caller.clone()];
        if let Some(c) = self.agents.get(caller).map(|e| Arc::clone(&e)) {
            ids.extend(self.ancestor_chain(&c));
        }
        let live_can = |id: &AgentId, built: &Value| -> bool {
            self.agents.get(id).is_some_and(|a| {
                a.meta.read().expect("meta lock poisoned").get("can").unwrap_or(&Value::Null) == built
            })
        };
        {
            let ix = self.authz.read().expect("authz index poisoned");
            if let Some(e) = ix.cands.get(caller) {
                let same = e.chain.len() == ids.len()
                    && e.chain.iter().zip(&ids).all(|((id, built), live)| id == live && live_can(id, built));
                if same {
                    return Arc::clone(&e.cands);
                }
            }
        }
        let chain: Vec<(AgentId, Value)> = ids
            .iter()
            .filter_map(|id| {
                let a = self.agents.get(id).map(|e| Arc::clone(&e))?;
                let can = a.meta.read().expect("meta lock poisoned").get("can").cloned().unwrap_or(Value::Null);
                Some((id.clone(), can))
            })
            .collect();
        let cands = Arc::new(self.candidates_of(&ids));
        if chain.len() == ids.len() {
            self.authz.write().expect("authz index poisoned").cands.insert(
                caller.clone(),
                crate::authz::solver::CandEntry { chain, cands: Arc::clone(&cands) },
            );
        }
        cands
    }

    /// [`Self::held`] over a given candidate set.
    pub(crate) fn held_from(&self, caller: &AgentId, target: &AgentId, cands: &Candidates) -> Held {
        let Some(t) = self.agents.get(target).map(|e| Arc::clone(&e)) else {
            return Held::Nothing;
        };
        if t.is_remote() {
            return Held::Nothing;
        }
        if self.in_tree(target, caller) {
            return Held::Own;
        }
        let creates = cands
            .bundles
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(PROBE))
            .any(|b| {
                cands.whos.iter().any(|w| {
                    self.allowed(
                        caller,
                        &Act::Create {
                            bundle: b.to_string(),
                            new_id: sample_id(w),
                        },
                        &t,
                    )
                })
            });
        if creates {
            return Held::Create;
        }
        if self.allowed_all(caller, Slot::Field, &t, Basis::Whole)
            || cands
                .fields
                .iter()
                .any(|f| self.allowed(caller, &Act::Update(f.clone()), &t))
        {
            return Held::Update;
        }
        if self.allowed_all(caller, Slot::Verb, &t, Basis::Whole)
            || cands
                .verbs
                .iter()
                .any(|v| self.allowed(caller, &Act::Call(v.clone()), &t))
        {
            return Held::Call;
        }
        Held::Nothing
    }

    /// May `origin` receive the outcome of execution `execution_id`, joining it
    /// on `target`? Returns a denial, or None.
    ///
    /// Joining hands back ANOTHER caller's result verbatim, so only the agent that
    /// started the execution (its ledger row's originator) joins it, and the
    /// operator (no origin) joins any. Every other joiner gets the unknown answer
    /// for the target.
    pub(super) fn join_denial(
        self: &Arc<Self>,
        origin: &Option<AgentId>,
        execution_id: i64,
        target: &Arc<Agent>,
    ) -> Option<Value> {
        let joiner = origin.as_ref()?;
        match self.ledger.originator(execution_id) {
            Some(Some(o)) if &o == joiner => None,
            _ => Some(crate::refusal::unknown_agent(target.id.as_str())),
        }
    }

    /// The call gate for an agent-originated send: `sender` may call `verb` on
    /// `target` when its lines and the ancestor cap allow it
    /// ([`Self::allowed`]). Returns the unknown answer for the target when it is
    /// refused, else `None`. A remote target is always refused to an agent: no
    /// line can name one.
    pub(crate) fn gate(
        self: &Arc<Self>,
        sender_id: &AgentId,
        verb: &str,
        target: &Arc<Agent>,
    ) -> Option<Value> {
        if self.allowed(sender_id, &Act::Call(verb.to_string()), target) {
            None
        } else {
            Some(crate::refusal::unknown_agent(target.id.as_str()))
        }
    }
}
