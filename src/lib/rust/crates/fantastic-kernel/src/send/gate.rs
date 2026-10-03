//! THE CALL GATE — two-sided, deny-all, explicit grants only.
//!
//! The trust boundary is WHO ORIGINATED the send. An operator send (no dispatch
//! running: the CLI, the cold bootstrap, a test driver, a transport handing an
//! inbound frame to its leg) and the kernel's own bookkeeping call are the origin
//! root of trust and never reach here. Identity comes only from dispatch: a send
//! made inside an agent's handler, or a task it spawned, is that agent's. Every AGENT send — ANY verb, `reflect` and the kernel verbs the
//! root answers included, nothing exempt — needs BOTH the sender's `can_call`
//! (egress) AND the target's `callable_by` (ingress). An edge exists only if both
//! ends grant it. The tree MUTATORS then act only on the sender's own direct
//! children.

use super::*;

/// Read a holder's authority grant (`can_call` or `callable_by`) + its tree facts
/// (parent id, direct-child ids) for the gate. An absent agent → empty grant →
/// deny-all (never a hole).
fn grant_facts(holder: Option<&Agent>, field: &str) -> (Value, Option<String>, HashSet<String>) {
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
            let kids: HashSet<String> = a.child_ids().into_iter().map(|c| c.0).collect();
            (grant, parent, kids)
        }
        None => (Value::Null, None, HashSet::new()),
    }
}

impl Kernel {
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
        let (s_can, s_parent, s_kids) = grant_facts(sender.as_deref(), "can_call");

        // Egress — may the SENDER call the target?
        let d = crate::authz::authorize_call(
            &s_can,
            verb,
            target.id.as_str(),
            sender_id.as_str(),
            s_parent.as_deref(),
            &s_kids,
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
        let (t_call, t_parent, t_kids) = grant_facts(Some(target.as_ref()), "callable_by");
        let di = crate::authz::authorize_call(
            &t_call,
            verb,
            sender_id.as_str(),
            target.id.as_str(),
            t_parent.as_deref(),
            &t_kids,
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

    /// THE MUTATORS ACT ON YOUR OWN CHILDREN ONLY. After the gate on the root,
    /// `create_agent`'s `parent_id` must be the sender, and `update_agent` /
    /// `delete_agent`'s `id` must name an agent whose parent is the sender — an
    /// unknown id is not the sender's child either. An operand that is missing or
    /// not a (non-empty) string is NOT judged here: the verb answers it with its
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
        let ok = if verb == "create_agent" {
            given == sender_id.as_str()
        } else {
            self.agents
                .get(&AgentId::from(given))
                .is_some_and(|op| op.parent_id.as_ref() == Some(sender_id))
        };
        if ok {
            return None;
        }
        Some(json!({
            "error": format!("{sender_id} may only {verb} its own children — {given} is not one"),
            "reason": "unauthorized", "hint": crate::authz::FROM_ABOVE, "side": "mutator",
        }))
    }
}
