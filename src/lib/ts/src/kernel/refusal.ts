// The ONE shape every refused call answers with — port of python
// `kernel/_refusal.py` (#27), byte-identical in field names and codes.
//
// Before this there were ~18 shapes: `{error}`, `{error, reason, side, hint}`,
// `{error, created:false}`, `{error, locked:true}`. A caller — human, LLM, or
// another runtime — had to know which verb it called to know what came back,
// and a machine could not branch on the failure without matching prose.
//
// So: one object, one machine field (`reason`), one human field (`error`).
//
// The `hint` is not decoration. Measured 2026-08-04: a model copied the literal
// placeholder `<id>` out of a prompt example, received `{"error": "no agent
// '<id>'"}` thirty-four times in one run, and never once corrected. The error
// stated a fact and withheld every fact that would have repaired it.

import type { Json, Payload } from "./json.ts";
import { pyRepr } from "./pyjson.ts";

// The closed set. Each names a KIND of refusal, not a call site — two verbs
// failing for the same reason share a code.
export const UNAUTHORIZED = "unauthorized"; // the call gate said no (carries `side`)
export const UNKNOWN_AGENT = "unknown_agent"; // no agent by that id (carries `agents`)
export const MISSING_ARG = "missing_arg"; // a required argument was absent (carries `arg`)
export const BAD_ARG = "bad_arg"; // present but wrong type/value (carries `arg`)
export const INVALID_META = "invalid_meta"; // the bundle's own validate_meta refused
export const LOCKED = "locked"; // `delete_lock` (carries `id`)
export const NOT_HERE = "not_here"; // right verb, wrong agent — kernel verbs are root-only
export const EXISTS = "exists"; // an id already taken (carries `id`)
export const UNHANDLED = "unhandled"; // the agent has no such verb (carries `verb`)
export const NOT_FOUND = "not_found"; // the named THING is absent — a row, a grant, an execution
export const DRAINING = "draining"; // the kernel is stopping; no new roots are accepted
export const NO_CONSUMER = "no_consumer"; // watch: the watcher has no registered inbox

// Transport codes — a leg that could not carry the call at all. Listed here
// because they share the field: a caller branching on `reason` must be able to
// see the whole vocabulary in one place.
export const NOT_CONNECTED = "not_connected";
export const TRANSPORT_ERROR = "transport_error";
export const TRANSPORT_CLOSED = "transport_closed";
export const TIMEOUT = "timeout";

export const CODES: ReadonlySet<string> = new Set([
  UNAUTHORIZED, UNKNOWN_AGENT, MISSING_ARG, BAD_ARG, INVALID_META,
  LOCKED, NOT_HERE, EXISTS, UNHANDLED, NOT_FOUND, DRAINING, NO_CONSUMER,
  NOT_CONNECTED, TRANSPORT_ERROR, TRANSPORT_CLOSED, TIMEOUT,
]);

export interface RefusalOpts {
  hint?: string;
  see?: string;
  extra?: Payload;
}

/**
 * Build the one refusal shape. `code` must be one of `CODES`.
 *
 * An unknown code THROWS: the point of a closed set is that it cannot quietly
 * grow at a call site.
 */
export function refusal(code: string, error: string, opts: RefusalOpts = {}): Payload {
  if (!CODES.has(code)) {
    throw new Error(
      `unknown refusal code '${code}'; one of ${[...CODES].sort().join(", ")}`,
    );
  }
  const out: Payload = { error, reason: code };
  if (opts.hint) out["hint"] = opts.hint;
  if (opts.see) out["see"] = opts.see;
  if (opts.extra !== undefined) Object.assign(out, opts.extra);
  return out;
}

/** `<verb>: <arg> required` — the commonest refusal, spelled once. */
export function missing(verb: string, arg: string, hint = ""): Payload {
  return refusal(MISSING_ARG, `${verb}: ${arg} required`, { hint, extra: { arg } });
}

/** Present but unusable — say what was WANTED, not just that it was wrong. */
export function bad(verb: string, arg: string, want: string, hint = ""): Payload {
  return refusal(BAD_ARG, `${verb}: ${arg} must be ${want}`, { hint, extra: { arg } });
}

/** How many live ids an `unknown_agent` refusal lists (python
 *  `UNKNOWN_AGENT_IDS_MAX`); `agent_count` says how many there are. */
export const UNKNOWN_AGENT_IDS_MAX = 40;

/** No agent by that id — NAMES the live ids, because a bare "no agent 'x'"
 *  teaches the caller nothing it can act on (python `Kernel.unknown_agent`).
 *  An id wrapped in angle brackets is a SLOT copied out of an example, never an
 *  id this kernel holds — so that, and only that, gets a hint. `extra` is merged
 *  last. */
export function unknown(id: string, live: string[], extra: Payload = {}): Payload {
  const ids = live.slice().sort();
  const out = refusal(UNKNOWN_AGENT, `no agent ${pyRepr(id)}`, {
    extra: { agent_count: ids.length, agents: ids.slice(0, UNKNOWN_AGENT_IDS_MAX) as Json },
  });
  if (id.startsWith("<") && id.endsWith(">")) {
    out["hint"] = `${pyRepr(id)} is a PLACEHOLDER from an example, not an id — substitute one of \`agents\`.`;
  }
  Object.assign(out, extra);
  return out;
}

/** The one reserved agent id — the operator's name (send target, store identity). */
export const RESERVED_ID = "kernel";

/**
 * Refuse an agent id that is not ONE SAFE NAME (python `guard_agent_id`).
 * Returns a refusal, or null when the id is fine.
 *
 * An id becomes a directory on the host (the loader persists each record under
 * it), a `|` marks a peer address `<guid>|<agent>` (a local id never carries
 * one), an all-digit id reads as a message id, and `kernel` is the operator.
 */
export function guardAgentId(verb: string, agentId: unknown): Payload | null {
  if (typeof agentId !== "string" || agentId === "") return bad(verb, "id", "a non-empty string");
  const r = pyRepr(agentId);
  if (agentId.includes("/") || agentId.includes("\\")) {
    return bad(verb, "id", `a single path segment (no separators); got ${r}`);
  }
  if (agentId === "." || agentId === ".." || agentId.includes("..")) {
    return bad(verb, "id", `a name that is not a path traversal; got ${r}`);
  }
  if (agentId.startsWith(".")) return bad(verb, "id", `a name that is not hidden; got ${r}`);
  if (/^\p{Nd}+$/u.test(agentId)) {
    return bad(verb, "id", `a name, not a number (message ids are ints); got ${r}`);
  }
  if (agentId.includes("|")) {
    return bad(verb, "id", `a local name without \`|\` (it marks a foreign \`guid|agent\`); got ${r}`);
  }
  if (agentId === RESERVED_ID) return bad(verb, "id", `a name other than the reserved ${pyRepr(RESERVED_ID)}`);
  return null;
}
