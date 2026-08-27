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

// Transport codes — a leg that could not carry the call at all. Listed here
// because they share the field: a caller branching on `reason` must be able to
// see the whole vocabulary in one place.
export const NOT_CONNECTED = "not_connected";
export const TRANSPORT_ERROR = "transport_error";
export const TRANSPORT_CLOSED = "transport_closed";
export const TIMEOUT = "timeout";

export const CODES: ReadonlySet<string> = new Set([
  UNAUTHORIZED, UNKNOWN_AGENT, MISSING_ARG, BAD_ARG, INVALID_META,
  LOCKED, NOT_HERE, EXISTS, UNHANDLED,
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

/** No agent by that id — NAMES the live ids, because a bare "no agent 'x'"
 *  teaches the caller nothing it can act on. */
export function unknown(id: string, live: string[]): Payload {
  return refusal(UNKNOWN_AGENT, `no agent '${id}'`, {
    hint: "name one of the live ids, or create it first",
    extra: { agents: live.slice().sort() as Json },
  });
}
