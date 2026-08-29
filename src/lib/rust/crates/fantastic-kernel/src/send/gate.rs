//! THE CALL GATE — two-sided, deny-all, from-above only.
//!
//! The trust boundary is WHO ORIGINATED the send. An operator send
//! (`current_sender()` is `None`: the CLI, the cold bootstrap, a loader at boot,
//! a verb completing its own side effect) is the origin root of trust and never
//! reaches here. Every AGENT send — ANY verb, `reflect` included, nothing
//! exempt — needs BOTH the sender's `can_call` (egress) AND the target's
//! `callable_by` (ingress). An edge exists only if both ends grant it.
//!
//! Authority flows from ABOVE: an agent can never raise its own, and a parent
//! may delegate only a subset of what it already holds.

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
    /// The two-sided call gate for an agent-originated send. Returns the error
    /// `Value` if the send is refused, else `None`. Checks egress (the sender's
    /// `can_call`) then ingress (the target's `callable_by`), both via the one
    /// [`crate::authz::authorize_call`] engine, then the authority-conferral rules
    /// (no self-raise; delegate only a subset). A holder with no record → empty
    /// grant → deny-all (never a hole).
    /// May `origin` receive the outcome of an execution that ran `verb` on
    /// `target`? Returns a denial, or None.
    ///
    /// Joining hands back ANOTHER caller's result verbatim, so it is equivalent
    /// to having made that call. Asking the gate about the joiner's own payload
    /// instead — a different verb, whose reply it never gets — authorizes the
    /// wrong operation. An OPERATOR joiner (no origin) is ungated, as everywhere.
    pub(super) fn join_denial(
        self: &Arc<Self>,
        origin: &Option<AgentId>,
        verb: &str,
        target: &Arc<Agent>,
    ) -> Option<Value> {
        let joiner = origin.as_ref()?;
        self.gate(joiner, verb, target, &serde_json::json!({"type": verb}))
    }

    pub(super) fn gate(
        self: &Arc<Self>,
        sender_id: &AgentId,
        verb: &str,
        target: &Arc<Agent>,
        payload: &Value,
    ) -> Option<Value> {
        let loader_id = self.root().map(|r| r.id.0.clone());
        let sender = self.agents.get(sender_id).map(|e| Arc::clone(&e));
        let (s_can, s_parent, s_kids) = grant_facts(sender.as_deref(), "can_call");

        // FORKING YOUR OWN SUBTREE IS BASELINE — and the exemption keys on
        // PLACEMENT, not on who answers. `create_agent` is privileged, so a
        // wildcard never covers it; the one documented exception is that
        // growing your OWN subtree needs no grant, which is what makes
        // self-extension work with zero wiring.
        //
        // While the verb was answered by every agent that fell out for free
        // (`other_id == holder_id`, because you addressed yourself). Now the
        // verb is kernel-owned and the target is always the root, so that
        // exemption could never fire again and EVERY agent would need an
        // explicit `create_agent` grant on the root just to author its own
        // tool. The property being preserved was never "you addressed
        // yourself" — it is "the new agent lands in MY subtree", which
        // `parent_id` now states outright.
        let placement_is_own = verb == "create_agent"
            && payload.get("parent_id").and_then(Value::as_str) == Some(sender_id.as_str());

        // Egress — may the SENDER call the target?
        let d = crate::authz::authorize_call(
            &s_can,
            verb,
            target.id.as_str(),
            sender_id.as_str(),
            s_parent.as_deref(),
            &s_kids,
            loader_id.as_deref(),
            !placement_is_own,
        );
        if !d.allowed {
            return Some(json!({
                "error": format!("{sender_id} may not call {verb:?} on {}", target.id),
                "reason": "unauthorized", "side": "egress", "hint": d.hint,
            }));
        }

        // INGRESS IS NOT CHECKED FOR A KERNEL VERB AT THE ROOT. `callable_by`
        // answers "do I accept being acted upon", and the root is not what is
        // acted upon — the operand is in the payload. Requiring it would make
        // the operator edit the ROOT's record for every agent that wants to
        // read the task table. Consequence accepted: an agent granted
        // `<root>:[ps,cancel]` can kill ANY task; the per-victim edge a
        // manifest could grant before is GONE.
        if is_kernel_verb(verb) && target.parent_id.is_none() {
            // ...but CONFERRAL still applies. `update_agent` is a kernel verb
            // whose target is ALWAYS the root, so returning flat here made
            // `check_conferral` unreachable for the exact case it guards: any
            // agent holding `<root>:update_agent` could rewrite its OWN
            // can_call and grant itself egress to the whole tree, which is the
            // deny-all gate defeated from below. Python runs the check on this
            // path (`_agent.py`, `verb in _SYSTEM_VERBS and target.parent is
            // None`); rust did not. Skipping INGRESS here is deliberate and
            // unchanged — skipping conferral was not.
            if verb == "create_agent" || verb == "update_agent" {
                return self.check_conferral(
                    sender_id,
                    &s_can,
                    s_parent.as_deref(),
                    &s_kids,
                    loader_id.as_deref(),
                    payload,
                );
            }
            return None;
        }

        // Ingress — does the TARGET accept the sender? (tokens relative to target;
        // privileged-no-wildcard is egress-only, so `enforce_privileged=false`.)
        let (t_call, t_parent, t_kids) = grant_facts(Some(target.as_ref()), "callable_by");
        let di = crate::authz::authorize_call(
            &t_call,
            verb,
            sender_id.as_str(),
            target.id.as_str(),
            t_parent.as_deref(),
            &t_kids,
            loader_id.as_deref(),
            false,
        );
        if !di.allowed {
            return Some(json!({
                "error": format!("{} does not accept {verb:?} from {sender_id}", target.id),
                "reason": "unauthorized", "side": "ingress", "hint": di.hint,
            }));
        }

        // Authority conferral — no self-raise, delegate only a subset you hold.
        if verb == "create_agent" || verb == "update_agent" {
            return self.check_conferral(
                sender_id,
                &s_can,
                s_parent.as_deref(),
                &s_kids,
                loader_id.as_deref(),
                payload,
            );
        }
        None
    }

    /// Authority flows from ABOVE only: an agent may not raise its OWN authority
    /// fields, and may confer on another only a SUBSET of its own `can_call`.
    fn check_conferral(
        self: &Arc<Self>,
        sender_id: &AgentId,
        s_can: &Value,
        s_parent: Option<&str>,
        s_kids: &HashSet<String>,
        loader_id: Option<&str>,
        payload: &Value,
    ) -> Option<Value> {
        let obj = payload.as_object()?;
        let confers = obj.contains_key("can_call") || obj.contains_key("callable_by");
        if !confers {
            return None;
        }
        let verb = verb_of(payload);
        let edited = obj.get("id").and_then(Value::as_str);
        // Self-raise: never edit your own authority fields.
        if verb == "update_agent" && edited == Some(sender_id.as_str()) {
            return Some(json!({
                "error": format!("{sender_id} may not edit its own can_call/callable_by"),
                "reason": "unauthorized", "side": "self-grant", "hint": crate::authz::FROM_ABOVE,
            }));
        }
        // Subset: every LATERAL edge in the conferred `can_call` must be one the
        // granter itself holds (structural tokens are free; `callable_by` grants no
        // reach, so it is unbounded).
        if let Some(Value::Object(conferred)) = obj.get("can_call") {
            for (key, verbs) in conferred {
                if key.starts_with('$') {
                    continue; // structural tokens confer no lateral reach
                }
                // Conferring `"*"` (or a list with it) requires the granter to hold
                // `"*"` on that target; a concrete list requires each verb.
                let probe: Vec<&str> = match verbs {
                    Value::Array(vs) if !vs.iter().any(|v| v.as_str() == Some("*")) => {
                        vs.iter().filter_map(Value::as_str).collect()
                    }
                    _ => vec!["*"],
                };
                for v in probe {
                    let ok = crate::authz::authorize_call(
                        s_can,
                        v,
                        key,
                        sender_id.as_str(),
                        s_parent,
                        s_kids,
                        loader_id,
                        true,
                    )
                    .allowed;
                    if !ok {
                        return Some(json!({
                            "error": format!(
                                "{sender_id} cannot delegate {v:?} on {key} — you may only \
                                 confer can_call you already hold"
                            ),
                            "reason": "unauthorized", "side": "delegation",
                            "hint": crate::authz::FROM_ABOVE,
                        }));
                    }
                }
            }
        }
        None
    }
}
