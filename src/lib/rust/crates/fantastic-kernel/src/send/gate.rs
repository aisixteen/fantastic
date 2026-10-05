//! THE CALL GATE — two-sided, deny-all, explicit grants only.
//!
//! The trust boundary is WHO ORIGINATED the send. An operator send (no dispatch
//! running: the CLI, the cold bootstrap, a test driver, a transport handing an
//! inbound frame to its leg) and the kernel's own bookkeeping call are the origin
//! root of trust and never reach here. Identity comes only from dispatch: a send
//! made inside an agent's handler, or a task it spawned, is that agent's. Every AGENT send — ANY verb, `reflect` and the kernel verbs the
//! root answers included, nothing exempt — needs BOTH the sender's `can_call`
//! (egress) AND the target's `callable_by` (ingress). An edge exists only if both
//! ends grant it. The tree MUTATORS then act only within the sender's own
//! subtree.

use super::*;

/// Read a holder's authority grant (`can_call` or `callable_by`) + its parent id
/// for the gate. An absent agent → empty grant → deny-all (never a hole).
fn grant_facts(holder: Option<&Agent>, field: &str) -> (Value, Option<String>) {
    match holder {
        Some(a) => {
            let grant = a
                .meta
                .read()
                .expect("meta lock poisoned")
                .get(field)
                .cloned()
                .unwrap_or(Value::Null);
            let parent = a.parent_id.as_ref().map(|p| p.0.clone());
            (grant, parent)
        }
        None => (Value::Null, None),
    }
}

impl Kernel {
    /// Every id ABOVE `agent` — what `$subtree` matches against: a grant holder
    /// reaches the other end when it is one of these. A peer target (no local
    /// parent) has none.
    pub fn ancestor_ids(&self, agent: Option<&Agent>) -> HashSet<String> {
        let mut out = HashSet::new();
        let mut next = agent.and_then(|a| a.parent_id.clone());
        while let Some(pid) = next {
            if !out.insert(pid.0.clone()) {
                break;
            }
            next = self.agents.get(&pid).and_then(|p| p.parent_id.clone());
        }
        out
    }

    /// May `origin` receive the outcome of the execution the ledger opened with
    /// `payload` on `target`? Returns a denial, or None.
    ///
    /// Joining hands back ANOTHER caller's result verbatim, so it is equivalent
    /// to having made that call. Asking the gate about the joiner's own payload
    /// instead — a different verb, whose reply it never gets — authorizes the
    /// wrong operation.
    ///
    /// THE WHOLE PAYLOAD, not just the verb: the mutator check reads
    /// `parent_id` / `id`, so a join gated on a synthesised `{"type": verb}`
    /// asked a different question than the call did. A REHYDRATED row carries no payload (nothing is left
    /// to re-run), so it gates as python's `entry.get("payload") or {}` — an
    /// empty object with no verb. An OPERATOR joiner (no origin) is ungated.
    pub(super) fn join_denial(
        self: &Arc<Self>,
        origin: &Option<AgentId>,
        payload: &Value,
        target: &Arc<Agent>,
    ) -> Option<Value> {
        let joiner = origin.as_ref()?;
        let payload = match payload {
            Value::Object(_) => payload.clone(),
            _ => serde_json::json!({}),
        };
        let verb = payload
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        self.gate(joiner, &verb, target, &payload)
    }

    /// The two-sided call gate for an agent-originated send. Returns the error
    /// `Value` if the send is refused, else `None`. Egress (the sender's
    /// `can_call`) then ingress (the target's `callable_by`), both via the one
    /// [`crate::authz::authorize_call`] engine; then, for a tree mutator answered
    /// by the root, the children-only rule ([`Self::gate_mutator`]). A holder with
    /// no record → empty grant → deny-all (never a hole).
    pub(crate) fn gate(
        self: &Arc<Self>,
        sender_id: &AgentId,
        verb: &str,
        target: &Arc<Agent>,
        payload: &Value,
    ) -> Option<Value> {
        let sender = self.agents.get(sender_id).map(|e| Arc::clone(&e));
        let (s_can, s_parent) = grant_facts(sender.as_deref(), "can_call");

        // Egress — may the SENDER call the target?
        let d = crate::authz::authorize_call(
            &s_can,
            verb,
            target.id.as_str(),
            sender_id.as_str(),
            s_parent.as_deref(),
            &self.ancestor_ids(Some(target.as_ref())),
        );

        // A target on ANOTHER KERNEL: egress is the whole check here — the
        // sender's `can_call` must name `<guid>|<agent>` or `<guid>|*`. Whether
        // that agent accepts the call is its OWN kernel's decision (its
        // `callable_by`, against the qualified `<this guid>|<sender>`), made where
        // the agent lives — a mutator's children-only rule included.
        if let Some(remote) = target.remote.as_ref() {
            if d.allowed {
                return None;
            }
            return Some(json!({
                "error": format!(
                    "{sender_id} may not call {} on {}",
                    fantastic_bundle::py_repr(verb),
                    target.id
                ),
                "reason": "unauthorized",
                "hint": format!(
                    "{} is an agent on the connected kernel {}: name it in can_call — \
                     {{{}: [...]}} or {{'{}|*': [...]}}",
                    target.id,
                    remote.guid,
                    fantastic_bundle::py_repr(target.id.as_str()),
                    remote.guid
                ),
                "side": "egress",
            }));
        }
        if !d.allowed {
            return Some(json!({
                "error": format!(
                    "{sender_id} may not call {} on {}",
                    fantastic_bundle::py_repr(verb),
                    target.id
                ),
                "reason": "unauthorized", "hint": d.hint, "side": "egress",
            }));
        }

        // Ingress — does the TARGET accept the sender? Tokens resolve relative to
        // the target. The root answering a kernel verb is no exception.
        let (t_call, t_parent) = grant_facts(Some(target.as_ref()), "callable_by");
        let di = crate::authz::authorize_call(
            &t_call,
            verb,
            sender_id.as_str(),
            target.id.as_str(),
            t_parent.as_deref(),
            &self.ancestor_ids(sender.as_deref()),
        );
        if !di.allowed {
            return Some(json!({
                "error": format!(
                    "{} does not accept {} from {sender_id}",
                    target.id,
                    fantastic_bundle::py_repr(verb)
                ),
                "reason": "unauthorized", "hint": di.hint, "side": "ingress",
            }));
        }

        if crate::authz::MUTATORS.contains(&verb) && target.parent_id.is_none() {
            return self.gate_mutator(sender_id, verb, payload);
        }
        None
    }

    /// THE MUTATORS ACT WITHIN YOUR OWN SUBTREE. After the gate on the root,
    /// `create_agent`'s `parent_id` must be the sender or an agent below it, and
    /// `update_agent` / `delete_agent`'s `id` must name an agent below the sender
    /// (any depth) — an unknown id is in no subtree. An operand that is missing
    /// or not a (non-empty) string is NOT judged here: the verb answers it with its
    /// own missing/shape refusal. The operator never reaches here. Python
    /// `_gate_mutator`.
    fn gate_mutator(
        self: &Arc<Self>,
        sender_id: &AgentId,
        verb: &str,
        payload: &Value,
    ) -> Option<Value> {
        let field = if verb == "create_agent" {
            "parent_id"
        } else {
            "id"
        };
        let given = payload
            .get(field)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())?;
        let operand = self
            .agents
            .get(&AgentId::from(given))
            .map(|e| Arc::clone(&e));
        let below = operand
            .as_deref()
            .is_some_and(|op| self.ancestor_ids(Some(op)).contains(sender_id.as_str()));
        let ok = below || (verb == "create_agent" && given == sender_id.as_str());
        if ok {
            return None;
        }
        Some(json!({
            "error": format!("{sender_id} may only {verb} within its own subtree — {given} is not in it"),
            "reason": "unauthorized", "side": "mutator",
        }))
    }
}
