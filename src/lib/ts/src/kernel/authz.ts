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
import { pyRepr } from "./pyjson.ts";

/** Verbs a WILDCARD never covers (python `_authz.PRIVILEGED_VERBS`) — only an
 *  explicit list entry names one, so a broad `$parent:"*"` can never silently
 *  become "mutate the graph through my parent". The mutators change the tree;
 *  `state_subscribe` reads the WHOLE tree's stream; `allow`/`disallow` change a
 *  file_bridge's exposure; the lifecycle, task and loader verbs are the root's;
 *  `stop`/`mount`/`unmount`/`boot` are any agent's own lifecycle and a host's
 *  routes. `watch` whose src is the ROOT loses a structural wildcard's cover the
 *  same way (`authorizeCall`).
 *  This kernel answers few of them — the set is gate POLICY, the same in every
 *  runtime, so a grant means one thing wherever it is read. */
export const PRIVILEGED_VERBS: ReadonlySet<string> = new Set([
  "create_agent",
  "delete_agent",
  "update_agent",
  "state_subscribe",
  "allow",
  "disallow",
  "kernel_start",
  "kernel_stop",
  "shutdown_agents",
  "ps",
  "cancel",
  "ledger",
  "ack_deadletter",
  "persist_record",
  "forget_record",
  "load_tree",
  // An agent's own LIFECYCLE and a host's ROUTES. By verb name, whatever the
  // bundle: a web_ws/web_rest leg's `$parent:*` let any client it admitted stop
  // its web host for every client, or unmount a sealed sibling's routes (audit
  // 2026-10-03, BV-1).
  "stop",
  "mount",
  "unmount",
  "boot",
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

/** `"$subtree:<id>"` — a `callable_by` key accepting `<id>` and every LIVE
 *  descendant of it, resolved at call time (python `SUBTREE_PREFIX`). */
export const SUBTREE_PREFIX = "$subtree:";

/** A grant value is `"*"` (any verb) or a list of verb names (which may itself
 *  contain `"*"`). A PRIVILEGED verb loses its wildcard cover. A HERITABLE map
 *  value (`{verbs, for}`) never grants its holder — it is read only by
 *  `authorizeHeritable`, for the holder's descendants. */
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
  /** `$subtree:<x>` — is `otherId` `x` or a live descendant of it? Handed in by
   *  the INGRESS check only: the key is `callable_by`-only, so on egress (no
   *  resolver) it matches nothing. */
  inSubtree?: (otherId: string, x: string) => boolean;
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
  // `watch` whose src is the ROOT (`loaderId`) is held to the same rule: the
  // root's inbox carries every kernel verb's payload, `persist_blob` contents
  // included, and `$parent:*` handed it to every direct child (audit 2026-10-03,
  // KV-2). Every other watch is an ordinary verb.
  const privileged =
    ctx.enforcePrivileged !== false &&
    (PRIVILEGED_VERBS.has(verb) ||
      (verb === "watch" && ctx.loaderId !== undefined && ctx.loaderId !== null && otherId === ctx.loaderId)) &&
    otherId !== ctx.holderId;

  for (const [key, value] of Object.entries(g)) {
    let matched: boolean;
    if (key === "$children") matched = ctx.holderChildren.has(otherId);
    else if (key === "$self") matched = otherId === ctx.holderId;
    else if (key === "$parent") matched = otherId === ctx.holderParent;
    else if (key === "$loader") matched = ctx.loaderId !== undefined && ctx.loaderId !== null && otherId === ctx.loaderId;
    else if (key.startsWith(SUBTREE_PREFIX)) {
      // A LOCAL descendant only: a foreign `<guid>|<agent>` is never in this
      // tree, and an empty or absent `<x>` matches nothing.
      const x = key.slice(SUBTREE_PREFIX.length);
      matched = x !== "" && !otherId.includes("|") && ctx.inSubtree !== undefined && ctx.inSubtree(otherId, x);
    } else if (key.startsWith("$")) matched = false; // unknown token matches nothing
    // every agent of ONE other kernel: `<guid>|*` (python `authorize_call`). A
    // local id never carries `|`, so this only ever matches a caller or target
    // on that kernel.
    else if (key.endsWith("|*")) matched = otherId.startsWith(key.slice(0, -1));
    else matched = key === otherId;
    // A privileged verb loses only a STRUCTURAL key's wildcard (python
    // `privileged and key in STRUCTURAL_TOKENS`); a NAMED id's `*` is the
    // operator's own statement and covers it (audit a12).
    const structural = key.startsWith("$");
    if (matched && verbAllowed(value, verb, privileged && structural)) return true;
  }
  return false;
}

/** A HERITABLE `can_call` entry — `{verbs: [<verb>, …], for: "descendants"}`,
 *  exactly those keys, a non-empty list of verb strings. */
export function isHeritable(value: Json | undefined): value is { verbs: string[]; for: "descendants" } {
  if (value === null || value === undefined || typeof value !== "object" || Array.isArray(value)) return false;
  const keys = Object.keys(value);
  const verbs = (value as Payload)["verbs"];
  return (
    keys.length === 2 &&
    (value as Payload)["for"] === "descendants" &&
    Array.isArray(verbs) &&
    verbs.length > 0 &&
    verbs.every((v) => typeof v === "string")
  );
}

/**
 * EGRESS through an ANCESTOR's heritable entry (python `authorize_heritable`).
 * `ancestorGrants` are the `can_call` maps of the sender's ancestors, parent
 * first. An entry keyed by the target's literal id lets the holder's
 * DESCENDANTS call it: a NAMED verb covers even a privileged one, `*` covers
 * only a non-privileged verb — and not `watch` on the ROOT (`loaderId`), held to
 * the structural-wildcard rule (KV-2). The holder itself is never covered — its
 * own map reaches `verbAllowed`, which refuses a map value.
 */
export function authorizeHeritable(
  ancestorGrants: Iterable<Payload | null | undefined>,
  verb: string,
  targetId: string,
  loaderId: string | null,
): boolean {
  const privileged = PRIVILEGED_VERBS.has(verb) ||
    (verb === "watch" && loaderId !== null && targetId === loaderId);
  for (const g of ancestorGrants) {
    if (g === null || g === undefined || typeof g !== "object" || Array.isArray(g)) continue;
    const entry = (g as Payload)[targetId];
    if (!isHeritable(entry)) continue;
    if (entry.verbs.includes(verb)) return true;
    if (entry.verbs.includes("*") && !privileged) return true;
  }
  return false;
}

/** The denial a refused edge answers with (python `_gate`'s texts, verbatim).
 *  `side` says WHICH end refused, so a caller can tell "I may not call" from
 *  "they do not accept me". A FOREIGN caller (`<guid>|<agent>`, `foreign`) is
 *  refused at ingress with the one remedy that applies to it — the baseline
 *  tokens never match a foreign id, so only a NAMED entry can accept it. */
export function denial(
  side: "egress" | "ingress",
  senderId: string,
  targetId: string,
  verb: string,
  foreign = false,
): Payload {
  if (side === "egress") {
    return refusal(UNAUTHORIZED, `${senderId} may not call ${pyRepr(verb)} on ${targetId}`, {
      hint: FROM_ABOVE,
      extra: { side },
    });
  }
  const hint = foreign
    ? `a foreign caller is accepted only by name: callable_by {${pyRepr(senderId)}: [...]} or ` +
      `{'${senderId.split("|")[0] as string}|*': [...]} on ${targetId}`
    : FROM_ABOVE;
  return refusal(UNAUTHORIZED, `${targetId} does not accept ${pyRepr(verb)} from ${senderId}`, {
    hint,
    extra: { side },
  });
}
