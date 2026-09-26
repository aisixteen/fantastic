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
    /// Resolve an id the way `send` does — the `kernel` alias, then a
    /// declared `alias` meta through `well_known` (C35), then a literal id.
    /// `None` when it names no live agent.
    fn resolve_agent(self: &Arc<Self>, id: Option<&Value>) -> Option<Arc<Agent>> {
        let id = id.and_then(Value::as_str).filter(|s| !s.is_empty())?;
        self.resolve_target(id)
    }

    /// Two-sided gate on the PLACEMENT TARGET of `create_agent`. Returns a
    /// refusal, or `None` when the sender may create under `parent`.
    ///
    /// Placing inside your OWN subtree is baseline (no grant). Anywhere else
    /// needs egress on the parent (explicitly named — `create_agent` is
    /// privileged, so a wildcard never covers it) AND the parent's own
    /// `callable_by` accepting the sender.
    fn gate_placement(
        self: &Arc<Self>,
        sender_id: &AgentId,
        s_can: &Value,
        s_parent: Option<&str>,
        s_kids: &HashSet<String>,
        loader_id: Option<&str>,
        parent: &Arc<Agent>,
    ) -> Option<Value> {
        let mut node = Some(Arc::clone(parent));
        while let Some(n) = node {
            if n.id == *sender_id {
                return None; // your own subtree — baseline
            }
            node = n
                .parent_id
                .as_ref()
                .and_then(|p| self.agents.get(p).map(|e| Arc::clone(&e)));
        }
        let d = crate::authz::authorize_call(
            s_can,
            "create_agent",
            parent.id.as_str(),
            sender_id.as_str(),
            s_parent,
            s_kids,
            loader_id,
            true,
        );
        if !d.allowed {
            return Some(json!({
                "error": format!("{sender_id} may not create under {}", parent.id),
                "reason": "unauthorized", "hint": d.hint, "side": "egress",
            }));
        }
        let (t_call, t_parent, t_kids) = grant_facts(Some(parent.as_ref()), "callable_by");
        let di = crate::authz::authorize_call(
            &t_call,
            "create_agent",
            sender_id.as_str(),
            parent.id.as_str(),
            t_parent.as_deref(),
            &t_kids,
            loader_id,
            false,
        );
        if !di.allowed {
            return Some(json!({
                "error": format!("{} does not accept a child from {sender_id}", parent.id),
                "reason": "unauthorized", "hint": di.hint, "side": "ingress",
            }));
        }
        None
    }

    /// The two-sided call gate for an agent-originated send. Returns the error
    /// `Value` if the send is refused, else `None`. Checks egress (the sender's
    /// `can_call`) then ingress (the target's `callable_by`), both via the one
    /// [`crate::authz::authorize_call`] engine, then the authority-conferral rules
    /// (no self-raise; delegate only a subset). A holder with no record → empty
    /// grant → deny-all (never a hole).
    /// May `origin` receive the outcome of the execution the ledger opened with
    /// `payload` on `target`? Returns a denial, or None.
    ///
    /// Joining hands back ANOTHER caller's result verbatim, so it is equivalent
    /// to having made that call. Asking the gate about the joiner's own payload
    /// instead — a different verb, whose reply it never gets — authorizes the
    /// wrong operation.
    ///
    /// THE WHOLE PAYLOAD, not just the verb: `create_agent`'s placement check
    /// reads `parent_id` and the conferral check reads the grant maps, so a
    /// join gated on a synthesised `{"type": verb}` asked a different question
    /// than the call did. A REHYDRATED row carries no payload (nothing is left
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

    pub(crate) fn gate(
        self: &Arc<Self>,
        sender_id: &AgentId,
        verb: &str,
        target: &Arc<Agent>,
        payload: &Value,
    ) -> Option<Value> {
        let loader_id = self.root().map(|r| r.id.0.clone());
        let sender = self.agents.get(sender_id).map(|e| Arc::clone(&e));
        let (s_can, s_parent, s_kids) = grant_facts(sender.as_deref(), "can_call");

        // PLACEMENT IS THE OPERAND. `create_agent` is answered by the root, but
        // the agent it ACTS UPON is the parent named in `parent_id` — so that is
        // what the gate asks about, on both sides. Gating the root instead made
        // one grant (`<root>:[create_agent]`, the ordinary "may author tools"
        // wiring) a key to the whole tree: the holder could plant a child under
        // ANY agent and inherit `$parent:*` reach into it (measured 2026-09-02).
        //
        // FORKING YOUR OWN SUBTREE IS BASELINE — placing under yourself or a
        // descendant needs no grant at all, which is what makes self-extension
        // work with zero wiring at any depth.
        if verb == "create_agent" {
            if let Some(parent) = self.resolve_agent(payload.get("parent_id")) {
                if let Some(denial) = self.gate_placement(
                    sender_id,
                    &s_can,
                    s_parent.as_deref(),
                    &s_kids,
                    loader_id.as_deref(),
                    &parent,
                ) {
                    return Some(denial);
                }
                return self.check_conferral(
                    sender_id,
                    &s_can,
                    s_parent.as_deref(),
                    &s_kids,
                    loader_id.as_deref(),
                    payload,
                );
            }
            // An unknown parent falls through to the ordinary path; the verb
            // itself answers `unknown_agent`.
        }

        // Egress — may the SENDER call the target?
        let d = crate::authz::authorize_call(
            &s_can,
            verb,
            target.id.as_str(),
            sender_id.as_str(),
            s_parent.as_deref(),
            &s_kids,
            loader_id.as_deref(),
            true,
        );
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
                "error": format!(
                    "{} does not accept {} from {sender_id}",
                    target.id,
                    fantastic_bundle::py_repr(verb)
                ),
                "reason": "unauthorized", "hint": di.hint, "side": "ingress",
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
                let probe_key: &str = match key.as_str() {
                    // Relative to the HOLDER — no lateral reach.
                    "$self" | "$parent" | "$children" => continue,
                    // NOT relative: `$loader` resolves to the ROOT for whoever
                    // holds it, so conferring it hands over real reach on the
                    // kernel's control surface (ps, cancel, ledger,
                    // kernel_stop). Subset-probe it like any lateral edge —
                    // skipping it let a grandchild grant its own child root
                    // reach it did not hold (measured 2026-09-02).
                    "$loader" => match loader_id {
                        Some(r) => r,
                        None => continue,
                    },
                    k if k.starts_with('$') => {
                        return Some(json!({
                            "error": format!(
                                "{sender_id} cannot confer {}: not a structural token \
                                 — $self, $parent, $children and $loader are the only ones",
                                fantastic_bundle::py_repr(key)
                            ),
                            "reason": "bad_arg", "arg": "can_call", "side": "delegation",
                            "hint": crate::authz::FROM_ABOVE,
                        }));
                    }
                    k => k,
                };
                // Conferring `"*"` (or a list with it) requires the granter to hold
                // `"*"` on that target; a concrete list requires each verb.
                //
                // python: `probe = need if (need and "*" not in need) else ["*"]` —
                // an EMPTY list is falsy, so it probes `*` too. A bare `Value::Array`
                // emptiness check used to be missing here: `can_call:{"x":[]}` built
                // an empty `probe` vec, the `for v in probe` loop ran zero times, and
                // the conferral silently PASSED (C21).
                let probe: Vec<&str> = match verbs {
                    Value::Array(vs)
                        if !vs.is_empty() && !vs.iter().any(|v| v.as_str() == Some("*")) =>
                    {
                        vs.iter().filter_map(Value::as_str).collect()
                    }
                    _ => vec!["*"],
                };
                for v in probe {
                    let ok = crate::authz::authorize_call(
                        s_can,
                        v,
                        probe_key,
                        sender_id.as_str(),
                        s_parent,
                        s_kids,
                        loader_id,
                        true,
                    )
                    .allowed;
                    if !ok {
                        if key == "$loader" {
                            return Some(json!({
                                "error": format!(
                                    "{sender_id} cannot delegate {} on $loader: it \
                                     resolves to {probe_key}, which you do not hold — \
                                     $loader is real reach on the kernel's control \
                                     surface, not a structural token",
                                    fantastic_bundle::py_repr(v)
                                ),
                                "reason": "unauthorized", "side": "delegation",
                                "hint": crate::authz::FROM_ABOVE,
                            }));
                        }
                        // Name the TRUE reason. A target that is not an agent at all
                        // is almost always one created in the SAME batch — items
                        // launch concurrently, so it is not there yet when this gate
                        // runs. "you don't hold it" sent a builder into a 382-second
                        // deliberation (measured 2026-09-02).
                        if !self.agents.contains_key(&AgentId::from(key.as_str())) {
                            return Some(json!({
                                "error": format!(
                                    "{sender_id} cannot delegate {} on {key}: no such \
                                     agent — calls of one batch run concurrently, so an \
                                     agent created in this batch does not exist yet; \
                                     create in one batch, grant in the next",
                                    fantastic_bundle::py_repr(v)
                                ),
                                "reason": "unauthorized", "side": "delegation",
                                "missing": key,
                                "hint": crate::authz::FROM_ABOVE,
                            }));
                        }
                        // The target exists but the granter does not hold it. Say WHY
                        // in one line — the usual cause is a specialist created under
                        // the root instead of under the granter, and "you don't hold
                        // it" hid that for twenty messages (2026-09-02).
                        let pid = self
                            .agents
                            .get(&AgentId::from(key.as_str()))
                            .and_then(|a| a.parent_id.clone());
                        let pid_s = pid.as_ref().map(|p| p.to_string());
                        return Some(json!({
                            "error": format!(
                                "{sender_id} cannot delegate {} on {key}: {key} is not \
                                 your child (its parent is {}) and not in your can_call — \
                                 you may only confer can_call you already hold; create \
                                 the agents you will wire as YOUR children",
                                fantastic_bundle::py_repr(v),
                                pid_s.as_deref().unwrap_or("None")
                            ),
                            "reason": "unauthorized", "side": "delegation",
                            "parent": pid_s,
                            "hint": crate::authz::FROM_ABOVE,
                        }));
                    }
                }
            }
        }
        None
    }
}
