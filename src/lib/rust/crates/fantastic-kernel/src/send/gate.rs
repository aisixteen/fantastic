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
    /// Resolve an id the way `send` does — the reserved `kernel` address, then a literal
    /// id. `None` when it names no live agent.
    fn resolve_agent(self: &Arc<Self>, id: Option<&Value>) -> Option<Arc<Agent>> {
        let id = id.and_then(Value::as_str).filter(|s| !s.is_empty())?;
        self.resolve_target(id)
    }

    /// Is `other` X or a live descendant of X? The `$subtree:<X>` resolver, asked
    /// at call time so an agent spawned after the grant is covered.
    pub fn in_subtree(&self, other: &str, x: &str) -> bool {
        let mut node = self
            .agents
            .get(&AgentId::from(other))
            .map(|e| Arc::clone(&e));
        while let Some(n) = node {
            if n.id.as_str() == x {
                return true;
            }
            node = n
                .parent_id
                .as_ref()
                .and_then(|p| self.agents.get(p).map(|e| Arc::clone(&e)));
        }
        false
    }

    /// May `sender_id` call `target_id` through a HERITABLE `can_call` entry one
    /// of its ANCESTORS holds? Nearest ancestor first; the sender's own entries
    /// never count (a heritable entry does not reach for its holder).
    pub fn heritable_egress(&self, sender_id: &str, verb: &str, target_id: &str) -> bool {
        let parent_of = |id: &AgentId| self.agents.get(id).and_then(|a| a.parent_id.clone());
        let mut grants = Vec::new();
        let mut node = parent_of(&AgentId::from(sender_id));
        while let Some(id) = node {
            let Some(a) = self.agents.get(&id).map(|e| Arc::clone(&e)) else {
                break;
            };
            grants.push(
                a.meta
                    .read()
                    .expect("meta lock poisoned")
                    .get("can_call")
                    .cloned()
                    .unwrap_or(Value::Null),
            );
            node = a.parent_id.clone();
        }
        let loader_id = self.root().map(|r| r.id.0.clone());
        crate::authz::authorize_heritable(grants.iter(), verb, target_id, loader_id.as_deref())
            .allowed
    }

    /// Two-sided gate on the PLACEMENT TARGET of `create_agent`. Returns a
    /// refusal, or `None` when the sender may create under `parent`.
    ///
    /// Placing inside your OWN subtree is baseline (no grant). Anywhere else
    /// needs egress on the parent (explicitly named — `create_agent` is
    /// privileged, so a wildcard never covers it) AND the parent's own
    /// `callable_by` accepting the sender — judged as the qualified
    /// `<guid>|<agent>` when a bridge leg relays a foreign caller.
    ///
    /// A RELAYED caller owns no subtree here: the leg's subtree is the leg's,
    /// not its peer's, so the baseline never applies on the leg's behalf (audit
    /// 2026-10-03, G-1) — it takes the path any other placement takes.
    fn gate_placement(
        self: &Arc<Self>,
        sender_id: &AgentId,
        s_can: &Value,
        s_parent: Option<&str>,
        s_kids: &HashSet<String>,
        loader_id: Option<&str>,
        parent: &Arc<Agent>,
    ) -> Option<Value> {
        let foreign = crate::send::relayed_caller(sender_id);
        // a relayed caller owns no subtree here — no baseline walk
        let mut node = foreign.is_none().then(|| Arc::clone(parent));
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
            None,
        );
        if !d.allowed
            && !self.heritable_egress(sender_id.as_str(), "create_agent", parent.id.as_str())
        {
            return Some(json!({
                "error": format!("{sender_id} may not create under {}", parent.id),
                "reason": "unauthorized", "hint": d.hint, "side": "egress",
            }));
        }
        let in_sender = foreign.unwrap_or_else(|| sender_id.0.clone());
        let (t_call, t_parent, t_kids) = grant_facts(Some(parent.as_ref()), "callable_by");
        let di = crate::authz::authorize_call(
            &t_call,
            "create_agent",
            &in_sender,
            parent.id.as_str(),
            t_parent.as_deref(),
            &t_kids,
            loader_id,
            false,
            Some(&|o: &str, x: &str| self.in_subtree(o, x)),
        );
        if !di.allowed {
            return Some(json!({
                "error": format!("{} does not accept a child from {in_sender}", parent.id),
                "reason": "unauthorized", "hint": di.hint, "side": "ingress",
            }));
        }
        None
    }

    /// Two-sided gate on the OPERAND of `update_agent` / `delete_agent` (the
    /// record `payload.id` names). Returns a refusal, or `None` when the sender
    /// may act on `operand`. Python `_gate_operand`.
    ///
    /// Inside your OWN subtree (yourself included) it is baseline (no grant).
    /// Anywhere else needs egress NAMING the operand with the verb
    /// ([`crate::authz::names_operand`]: a literal `<id>: ["<verb>"]`, never a
    /// wildcard or a token; or a heritable entry for that id naming the verb) AND
    /// the operand's own `callable_by` accepting the sender — judged as the
    /// qualified `<guid>|<agent>` when a bridge leg relays a foreign caller.
    /// That caller owns no subtree here, the leg's own record included (G-1, as
    /// [`Self::gate_placement`]).
    fn gate_operand(
        self: &Arc<Self>,
        sender_id: &AgentId,
        verb: &str,
        s_can: &Value,
        loader_id: Option<&str>,
        operand: &Arc<Agent>,
    ) -> Option<Value> {
        let foreign = crate::send::relayed_caller(sender_id);
        // a relayed caller owns no subtree here — no baseline walk
        let mut node = foreign.is_none().then(|| Arc::clone(operand));
        while let Some(n) = node {
            if n.id == *sender_id {
                return None; // your own subtree — baseline
            }
            node = n
                .parent_id
                .as_ref()
                .and_then(|p| self.agents.get(p).map(|e| Arc::clone(&e)));
        }
        if !crate::authz::names_operand(s_can, verb, operand.id.as_str())
            && !self.heritable_egress(sender_id.as_str(), verb, operand.id.as_str())
        {
            return Some(json!({
                "error": format!("{sender_id} may not {verb} {}", operand.id),
                "reason": "unauthorized",
                "hint": format!(
                    "{0} is outside your subtree: your can_call must name it with the \
                     verb — {{'{0}': ['{verb}']}}, never a wildcard — and its \
                     callable_by must accept you",
                    operand.id
                ),
                "side": "egress",
            }));
        }
        let in_sender = foreign.unwrap_or_else(|| sender_id.0.clone());
        let (t_call, t_parent, t_kids) = grant_facts(Some(operand.as_ref()), "callable_by");
        let di = crate::authz::authorize_call(
            &t_call,
            verb,
            &in_sender,
            operand.id.as_str(),
            t_parent.as_deref(),
            &t_kids,
            loader_id,
            false,
            Some(&|o: &str, x: &str| self.in_subtree(o, x)),
        );
        if !di.allowed {
            return Some(json!({
                "error": format!(
                    "{} does not accept {} from {in_sender}",
                    operand.id,
                    fantastic_bundle::py_repr(verb)
                ),
                "reason": "unauthorized", "hint": crate::authz::FROM_ABOVE, "side": "ingress",
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

        // A target on ANOTHER KERNEL: egress is the whole check here — the
        // sender's `can_call` must name `<guid>|<agent>` or `<guid>|*`. Whether
        // that agent accepts the call is its OWN kernel's decision (its
        // `callable_by`, against the qualified `<this guid>|<sender>`), made where
        // the agent lives.
        //
        // FIRST, before the placement branch below: a `create_agent` sent to
        // `<guid>|kernel` places its child on the PEER, so a `parent_id` in the
        // payload names nothing here — resolving it locally would judge the wrong
        // operand, and a sender placing under its own subtree would pass without
        // any grant on the peer at all.
        if let Some(remote) = target.remote.as_ref() {
            let d = crate::authz::authorize_call(
                &s_can,
                verb,
                target.id.as_str(),
                sender_id.as_str(),
                s_parent.as_deref(),
                &s_kids,
                loader_id.as_deref(),
                true,
                None,
            );
            if d.allowed || self.heritable_egress(sender_id.as_str(), verb, target.id.as_str()) {
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

        // THE RECORD IS THE OPERAND — the same rule for `update_agent` /
        // `delete_agent`. Gating the root made one grant
        // (`<root>:[update_agent, delete_agent]`, the builder wiring) a key to
        // every record — rewrite a credential store's refs, open a sealed leg,
        // clear a delete_lock and delete — while an agent could not edit its OWN
        // child without that tree-wide grant (audit 2026-10-03, AV-1). The id is
        // a LITERAL agent id (`kernel` is no alias here), as python's
        // `ctx.agents.get(oid)`.
        if crate::authz::RECORD_OPERAND_VERBS.contains(&verb) && target.parent_id.is_none() {
            let operand = payload
                .get("id")
                .and_then(Value::as_str)
                .and_then(|oid| self.agents.get(&AgentId::from(oid)).map(|e| Arc::clone(&e)));
            if let Some(operand) = operand {
                if let Some(denial) =
                    self.gate_operand(sender_id, verb, &s_can, loader_id.as_deref(), &operand)
                {
                    return Some(denial);
                }
                if verb == "update_agent" {
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
            // No such record: the ordinary path; the verb answers `unknown_agent`.
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
            None,
        );
        if !d.allowed && !self.heritable_egress(sender_id.as_str(), verb, target.id.as_str()) {
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
        // manifest could grant before is GONE. The tree mutators are the
        // exception: whenever their operand names an agent they were judged ON
        // THAT AGENT above (`gate_placement` / `gate_operand`); only an unknown
        // operand gets here.
        //
        // A FOREIGN caller a bridge leg relays is the exception to the exception:
        // the leg's `can_call` is the leg's reach, not its peer's, so the root's
        // `callable_by` must ALSO accept the caller as `<guid>|<agent>` or
        // `<guid>|*` — it takes the ingress check below (audit 2026-10-03, AV-3).
        // Local callers are unchanged.
        let foreign = crate::send::relayed_caller(sender_id);
        if is_kernel_verb(verb) && target.parent_id.is_none() && foreign.is_none() {
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
        // A bridge leg relaying a FOREIGN caller is judged by that caller's
        // qualified `<guid>|<agent>`, never by the leg: the target names who on the
        // other kernel it accepts, and the baseline tokens never match a foreign id.
        let in_sender = foreign.clone().unwrap_or_else(|| sender_id.0.clone());
        let (t_call, t_parent, t_kids) = grant_facts(Some(target.as_ref()), "callable_by");
        let di = crate::authz::authorize_call(
            &t_call,
            verb,
            &in_sender,
            target.id.as_str(),
            t_parent.as_deref(),
            &t_kids,
            loader_id.as_deref(),
            false,
            Some(&|o: &str, x: &str| self.in_subtree(o, x)),
        );
        if !di.allowed {
            let hint = match &foreign {
                Some(f) => format!(
                    "a foreign caller is accepted only by name: callable_by {{{}: [...]}} or \
                     {{{}: [...]}} on {}",
                    fantastic_bundle::py_repr(f),
                    fantastic_bundle::py_repr(&format!(
                        "{}|*",
                        f.split('|').next().unwrap_or_default()
                    )),
                    target.id
                ),
                None => di.hint,
            };
            return Some(json!({
                "error": format!(
                    "{} does not accept {} from {in_sender}",
                    target.id,
                    fantastic_bundle::py_repr(verb)
                ),
                "reason": "unauthorized", "hint": hint, "side": "ingress",
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

    /// Only the OPERATOR lowers a secret holder's mark — python
    /// `_check_holder_mark`. An agent's `update_agent` naming `holds_secrets` with
    /// any value but `true` on an agent whose record says `holds_secrets: true` is
    /// refused — else a model granted `update_agent` could unmark a
    /// credential_store and then call it. Setting `true` (on a holder or not)
    /// passes: protection may be added from anywhere. Judged on the record the
    /// verb would patch (the literal `id`), before the write-time shape check, so
    /// a non-operator's malformed value on a holder gets THIS refusal. The
    /// operator (no sender) never reaches here.
    fn check_holder_mark(&self, sender_id: &AgentId, obj: &Map<String, Value>) -> Option<Value> {
        use crate::lifecycle::{HOLDER_MARK_HINT, HOLDER_MARK_SIDE, HOLDS_SECRETS};
        if obj.get("type").and_then(Value::as_str) != Some("update_agent") {
            return None;
        }
        if obj.get(HOLDS_SECRETS)? == &Value::Bool(true) {
            return None;
        }
        let edited = obj.get("id").and_then(Value::as_str)?;
        let target = self
            .agents
            .get(&AgentId::from(edited))
            .map(|e| Arc::clone(&e))?;
        let held = target
            .meta
            .read()
            .expect("meta lock poisoned")
            .get(HOLDS_SECRETS)
            == Some(&Value::Bool(true));
        if !held {
            return None;
        }
        Some(json!({
            "error": format!(
                "{sender_id} may not clear holds_secrets on {edited}: it holds secrets, \
                 and only the operator lowers that mark"
            ),
            "reason": "unauthorized", "side": HOLDER_MARK_SIDE, "hint": HOLDER_MARK_HINT,
        }))
    }

    /// Authority flows from ABOVE only: an agent may not raise its OWN authority
    /// fields, and may confer on another only a SUBSET of its own `can_call`.
    /// Nor may it lower a secret holder's mark ([`Self::check_holder_mark`],
    /// judged first).
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
        if let Some(held) = self.check_holder_mark(sender_id, obj) {
            return Some(held);
        }
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
                if matches!(key.as_str(), "$self" | "$parent" | "$children") {
                    continue; // relative to the HOLDER — no lateral reach
                }
                if verbs.is_object() || key.starts_with(crate::authz::SUBTREE_PREFIX) {
                    // A heritable entry is conferred only by a holder of the SAME
                    // entry (subset). A malformed one, and a `$subtree:` key (which
                    // is callable_by-only), are refused by the write-time shape
                    // check with the shape's own words.
                    if crate::authz::is_heritable(verbs) && !key.starts_with('$') {
                        if let Some(v) = crate::authz::heritable_uncovered(
                            s_can.get(key),
                            verbs["verbs"].as_array().map(Vec::as_slice).unwrap_or(&[]),
                        ) {
                            return Some(json!({
                                "error": format!(
                                    "{sender_id} cannot confer a heritable {} on {key}: you \
                                     do not hold that heritable entry — only the operator, \
                                     or a holder of the same entry, confers one (a subset \
                                     of it)",
                                    fantastic_bundle::py_repr(&v)
                                ),
                                "reason": "unauthorized", "side": "delegation",
                                "hint": crate::authz::FROM_ABOVE,
                            }));
                        }
                    }
                    continue;
                }
                let probe_key: &str = match key.as_str() {
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
                        None,
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
