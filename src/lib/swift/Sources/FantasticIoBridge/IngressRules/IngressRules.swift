// IngressRules — the inbound-FILTER registry (the "upper importer").
//
// Each ingress rule is its own file in this folder (an `extension IngressRules`);
// `resolve` registers them BY NAME. Add a rule = drop a file + one switch arm; the
// relay_connector dispatcher never changes (it is rule-agnostic).

import FantasticJSON
import OrderedCollections

/// Namespace + registry for inbound-filter rules (Swift has a flat module namespace,
/// so the rules are nested here rather than colliding with the egress `Password`).
public enum IngressRules {
    /// Resolve an ingress rule spec (string | `{type, env}` | null) BY NAME.
    /// **SEALED BY DEFAULT** — absent ⇒ `DenyInbound` (an io leg with no rule
    /// refuses every inbound call until opened with `ingress_rule=allow_all`).
    /// Mirrors py/rust.
    ///
    /// UNKNOWN TYPE ⇒ `DenyInbound(unknown:)` — seal, and name the bad rule in every
    /// denial. Never a throw: resolution runs PER CALL, not at boot, and a raise there
    /// surfaces as an unhandled exception out of the handler, killing the caller's
    /// whole chain with no reply. An unresolvable rule is not an
    /// authorization, so deny. A MALFORMED spec is named by its repr (`parseSpec`)
    /// and seals the same way, so resolution cannot fail.
    public static func resolve(_ spec: JSON?) -> IngressRule {
        let (name, env) = parseSpec(spec)
        switch name {
        case nil, .some("deny_ingress"): return DenyInbound()
        case .some("allow_all_ingress"): return AllowAll()
        case .some("password_ingress"): return Password(tokenEnv: env ?? DefaultGroupTokenEnv)
        case .some(let other): return DenyInbound(unknown: other)
        }
    }

    /// name → ingress rule, in python's `REGISTRY` order. New ingress rule types
    /// register HERE and in `resolve`.
    public static let registry = ["allow_all_ingress", "deny_ingress", "password_ingress"]

    /// The rule name in `spec` this registry does not implement, if any — python's
    /// `getattr(resolve_ingress(...), "unknown", None)`, truthiness included (an
    /// empty name is a plain seal, not an unknown one).
    public static func unknown(_ spec: JSON?) -> String? {
        guard let name = parseSpec(spec).name, !name.isEmpty, !registry.contains(name) else {
            return nil
        }
        return name
    }

    /// THE write-time check every leg's `validateMeta` makes — python
    /// `ingress_rules.unknown_ingress_refusal`: an `ingress_rule` this registry
    /// does not implement (an unknown name, or a malformed spec) is refused at the
    /// WRITE, next to the typo. The gate would seal on it anyway, but
    /// `{"updated": true}` for a control that does not exist is its own failure.
    /// `legal` is the leg's own list — an in-process edge (fs/process) has no wire
    /// envelope to carry a token, so it names no `password_ingress`; a wire leg
    /// passes `registry`. Only a PRESENT `ingress_rule` is judged: the `auth`
    /// stem's ingress half may legitimately name no rule here.
    public static func unknownRefusal(
        _ meta: OrderedDictionary<String, JSON>, legal: [String] = registry
    ) -> String? {
        guard let spec = meta["ingress_rule"], unknown(spec) != nil else { return nil }
        return "ingress_rule=\(pyRepr(spec)) is not a rule this bundle implements — "
            + "legal values: \(legal.joined(separator: ", "))"
    }
}
