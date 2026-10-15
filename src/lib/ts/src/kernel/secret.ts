// SECRET MARKS on a send item (python `kernel/_secret.py`).
//
// An item may carry `secret: ["<dotted path>", ...]` — paths INTO its `payload`
// (`"fields.api_key"`). The handler gets the payload as sent; everything the
// kernel keeps or shows of the call — the call row in the store, the inbox
// mirrors to watchers — carries the payload AS SHOWN: each marked value replaced
// by `SHOWN`. A path naming nothing is skipped; `type` cannot be marked (the verb
// is routing, not data).
//
// The marks are readable by the handler as `kernel.currentSecret`, so an agent
// that relays the value onward marks its own send the same way; a host-bound
// call carries them on its `call` frame.

import type { Json, Payload } from "./json.ts";
import { BAD_ARG, refusal } from "./refusal.ts";
import { pyRepr } from "./pyjson.ts";

export const SHOWN = "[secret]";

/** The item's marks, or a `bad_arg` refusal for a malformed field. Absent ⇒ `[]`. */
export function check(marks: unknown): { marks: readonly string[]; bad: Payload | null } {
  if (marks === undefined || marks === null) return { marks: [], bad: null };
  if (
    !Array.isArray(marks) ||
    !marks.every((m) => typeof m === "string" && m !== "" && !m.split(".").includes(""))
  ) {
    return {
      marks: [],
      bad: refusal(
        BAD_ARG,
        `send: secret must be a list of dotted paths into the payload (e.g. ["fields.api_key"]), got ${pyRepr(marks as Json)}`,
        { extra: { arg: "secret" } },
      ),
    };
  }
  if (marks.includes("type")) {
    return {
      marks: [],
      bad: refusal(BAD_ARG, "send: secret may not mark `type` — the verb is not data", { extra: { arg: "secret" } }),
    };
  }
  return { marks: marks as string[], bad: null };
}

/** `payload` with every marked value replaced by `SHOWN`; the input is never
 *  mutated — each object on a marked path is copied, the rest is shared. No
 *  marks ⇒ the payload itself. */
export function shown(payload: Payload, marks: readonly string[]): Payload {
  if (marks.length === 0) return payload;
  const out: Payload = { ...payload };
  for (const mark of marks) {
    const keys = mark.split(".");
    const leaf = keys.pop() as string;
    let node: Payload | null = out;
    for (const key of keys) {
      const next: unknown = node[key];
      if (next === null || typeof next !== "object" || Array.isArray(next) || next instanceof Uint8Array) {
        node = null;
        break;
      }
      const copy: Payload = { ...(next as Payload) };
      node[key] = copy;
      node = copy;
    }
    if (node !== null && leaf in node) node[leaf] = SHOWN;
  }
  return out;
}
