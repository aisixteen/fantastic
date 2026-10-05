// THE CALL GATE — two-sided, explicit grants only.
//
// Port of python `kernel/_authz.py`. Every AGENT-originated send needs BOTH
// ends to grant the edge: the sender's `can_call` (egress) grants
// (target, verb) AND the target's `callable_by` (ingress) grants (sender,
// verb). Nothing is exempt — `reflect` and the kernel verbs the root answers
// included. The gate never branches on a verb name: `"*"` covers EVERY verb.
//
// An OPERATOR send (no origin: a send made outside every dispatch — the page's
// own bootstrap, a transport handing an inbound frame in, a test driver) is the
// ORIGIN ROOT OF TRUST and passes ungated. Identity comes ONLY from dispatch: the
// sends a handler makes through the kernel view it is handed are that agent's,
// and nothing a handler says can name another sender.
//
// A grant map is `{key: "*" | [verb, ...]}`. Keys: a literal local agent id,
// `$self`, `$parent`, `$subtree` (every agent BELOW the holder, any depth) — and,
// in `can_call` only,
// `<guid>|<agent>` / `<guid>|*`: an OUTBOUND peer address. A caller is always a
// LOCAL agent, so a `<guid>|…` key in `callable_by` never matches. Tokens resolve
// relative to the HOLDER (the sender for `can_call`, the target for
// `callable_by`). The baseline stamped on every agent is `{$self: "*"}` on both
// fields — nothing else; every other edge is an explicit grant.
//
// #39 — the READ side is gated too. A watcher receives its target's raw inbound
// payloads VERBATIM, so an ungated `watch` is a read side-channel around
// `callable_by`: it is gated as the verb `watch` like any call.

import type { Json, Payload } from "./json.ts";
import { UNAUTHORIZED, refusal } from "./refusal.ts";
import { pyRepr } from "./pyjson.ts";

/** The baseline stamped + MERGED under any explicit grant on BOTH fields of
 *  every agent: reach and accept yourself. Nothing else. */
export const DEFAULT_GRANT: Readonly<Payload> = { $self: "*" };

/** The remedy shown on any denial: authority descends, never self-raised
 *  (python `_FROM_ABOVE`, verbatim). */
export const FROM_ABOVE =
  "grants come from above — an ancestor (update_agent on an agent in its " +
  "subtree) or the operator writes can_call and callable_by; an agent never " +
  "raises its own";

/** A grant value is `"*"` (every verb) or a list of verb names. */
function verbAllowed(grant: Json | undefined, verb: string): boolean {
  if (grant === "*") return true;
  if (Array.isArray(grant)) return grant.includes(verb) || grant.includes("*");
  return false;
}

export interface GrantContext {
  holderId: string;
  holderParent: string | null;
  /** Every id ABOVE the other end — `$subtree` matches when the holder is one. */
  otherAncestors: ReadonlySet<string>;
}

/**
 * ONE matching engine, used BOTH ways. `grant` is the holder's map (`can_call`
 * for egress, `callable_by` for ingress); `otherId` is the id at the other end
 * (the target for egress, the sender for ingress). Tokens resolve relative to
 * the HOLDER. Absent or empty grant ⇒ deny, which is the default posture.
 */
export function authorizeCall(
  grant: Payload | null | undefined,
  verb: string,
  otherId: string,
  ctx: GrantContext,
): boolean {
  // A malformed grant (a bare "*" string, a list) is DENY, never a crash and
  // never a character-by-character walk: the gate judges what is there; refusing
  // the shape is the writer's job.
  if (grant !== null && grant !== undefined && (typeof grant !== "object" || Array.isArray(grant))) return false;
  for (const [key, value] of Object.entries(grant ?? {})) {
    let matched: boolean;
    if (key === "$subtree") matched = ctx.otherAncestors.has(ctx.holderId);
    else if (key === "$self") matched = otherId === ctx.holderId;
    else if (key === "$parent") matched = otherId === ctx.holderParent;
    else if (key.startsWith("$")) matched = false; // no other token exists
    // every agent of ONE peer kernel: `<guid>|*`. A local id never carries `|`,
    // so this only ever matches an outbound peer target.
    else if (key.endsWith("|*")) matched = otherId.startsWith(key.slice(0, -1));
    else matched = key === otherId;
    if (matched && verbAllowed(value, verb)) return true;
  }
  return false;
}

/** The denial a refused edge answers with (python `_gate`'s texts, verbatim).
 *  `side` says WHICH end refused, so a caller can tell "I may not call" from
 *  "they do not accept me". */
export function denial(side: "egress" | "ingress", senderId: string, targetId: string, verb: string): Payload {
  const error = side === "egress"
    ? `${senderId} may not call ${pyRepr(verb)} on ${targetId}`
    : `${targetId} does not accept ${pyRepr(verb)} from ${senderId}`;
  return refusal(UNAUTHORIZED, error, { hint: FROM_ABOVE, extra: { side } });
}
