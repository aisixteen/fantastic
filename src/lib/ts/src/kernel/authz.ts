// THE CALL GATE — fully gated, two-sided, from above.
//
// Port of python `kernel/_authz.py`. Every AGENT-originated send needs BOTH
// ends to grant the edge: the sender's `can_call` (egress) AND the target's
// `callable_by` (ingress). Nothing is exempt — `reflect` included.
//
// An OPERATOR send (no origin: the page's own bootstrap, a bundle wiring itself
// up, a test driver) is the ORIGIN ROOT OF TRUST and passes ungated. An agent
// cannot forge that: `currentOrigin` comes from the kernel view a handler is
// handed, never from anything the handler says.
//
// #39 — the READ side is gated too. A watcher receives its target's raw inbound
// payloads VERBATIM, so an ungated `watch` is a read side-channel around
// `callable_by`: an agent that may not CALL another could still read everything
// sent TO it. `state_subscribe` is the same hole at kernel scale.

import type { Json, Payload } from "./json.ts";
import { UNAUTHORIZED, refusal } from "./refusal.ts";

/** Verbs a WILDCARD never covers (python `_authz.PRIVILEGED_VERBS`) — only an
 *  explicit list entry names one, so a broad `$parent:"*"` can never silently
 *  become "mutate the graph through my parent". The mutators change the tree;
 *  `state_subscribe` reads the WHOLE tree's stream; `rows_read`/`rows_write` are
 *  hydration's pair (a conversation read whole / written into an agent). This
 *  kernel answers none of the last two — the set is gate POLICY, the same in
 *  every runtime, so a grant means one thing wherever it is read. */
export const PRIVILEGED_VERBS: ReadonlySet<string> = new Set([
  "create_agent",
  "delete_agent",
  "update_agent",
  "state_subscribe",
  "rows_read",
  "rows_write",
]);

/** Structural baseline, merged UNDER any explicit grant on every agent: reach
 *  and accept your own subtree and your creator. Deny-all governs every edge
 *  not in the merged map. */
export const DEFAULT_GRANT: Readonly<Payload> = {
  $self: "*",
  $parent: "*",
  $children: "*",
};

/** The remedy shown on any denial: authority descends, never self-raised. */
export const FROM_ABOVE =
  "authority is granted from ABOVE — set at create_agent by your creator, or by " +
  "the operator; a parent may delegate a subset of its own reach. You cannot " +
  "grant it to yourself. Agents are deny-all by default (both to call and to be " +
  "called).";

/** A grant value is `"*"` (any verb) or a list of verb names (which may itself
 *  contain `"*"`). A PRIVILEGED verb loses its wildcard cover. */
function verbAllowed(grant: Json | undefined, verb: string, privileged: boolean): boolean {
  if (privileged) return Array.isArray(grant) && grant.includes(verb);
  if (grant === "*") return true;
  if (Array.isArray(grant)) return grant.includes(verb) || grant.includes("*");
  return false;
}

export interface GrantContext {
  holderId: string;
  holderParent: string | null;
  holderChildren: ReadonlySet<string>;
  /** What `$loader` resolves to — the persistence root, i.e. this kernel's root. */
  loaderId?: string | null;
  /** EGRESS only. On INGRESS the escalation is already blocked at egress, so a
   *  callee's `$children:"*"` ("I trust my own subtree") legitimately accepts a
   *  privileged verb. */
  enforcePrivileged?: boolean;
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
  const g = grant ?? {};
  // Acting on your OWN subtree is baseline, not escalation — egress only.
  const privileged =
    ctx.enforcePrivileged !== false &&
    PRIVILEGED_VERBS.has(verb) &&
    otherId !== ctx.holderId;

  for (const [key, value] of Object.entries(g)) {
    let matched: boolean;
    if (key === "$children") matched = ctx.holderChildren.has(otherId);
    else if (key === "$self") matched = otherId === ctx.holderId;
    else if (key === "$parent") matched = otherId === ctx.holderParent;
    else if (key === "$loader") matched = ctx.loaderId !== undefined && ctx.loaderId !== null && otherId === ctx.loaderId;
    else if (key.startsWith("$")) matched = false; // unknown token matches nothing
    else matched = key === otherId;
    if (matched && verbAllowed(value, verb, privileged)) return true;
  }
  return false;
}

/** The denial a refused edge answers with. `side` says WHICH end refused, so a
 *  caller can tell "I may not call" from "they do not accept me". */
export function denial(side: "egress" | "ingress", senderId: string, targetId: string, verb: string): Payload {
  const what =
    side === "egress"
      ? `${senderId} may not call ${targetId}`
      : `${targetId} does not accept calls from ${senderId}`;
  return refusal(UNAUTHORIZED, `${what} (verb '${verb}')`, {
    hint: FROM_ABOVE,
    extra: { side, verb },
  });
}
