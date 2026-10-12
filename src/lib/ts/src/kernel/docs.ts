// HOW TO CALL an agent — the `docs` kernel verb (python `kernel/_docs.py`).
//
// A reflect says what an agent IS; how to call it is `docs`, in two tiers:
// `oneliners` (default) — each doc's first paragraph cut at its first
// top-level `. ` or ` — ` (never inside brackets); `detailed` — the whole doc.
// Answered by the root and never gated (the call gate's one exemption by verb
// name, `Kernel.gate`).

import type { Json, Payload } from "./json.ts";
import { BAD_ARG, refusal } from "./refusal.ts";
import { pyRepr } from "./pyjson.ts";

export const TIERS = ["oneliners", "detailed"] as const;

/** A verb's whole doc, trimmed (python `inspect.cleandoc`). */
export function fullDoc(doc: string): string {
  return doc.trim();
}

/** The ONELINER of a verb doc (python `_docs.signature`). */
export function signature(doc: string): string {
  const para = fullDoc(doc).split("\n\n")[0] as string;
  const line = para
    .split("\n")
    .map((p) => p.trim())
    .join(" ");
  const chars = [...line];
  let depth = 0;
  for (let i = 0; i < chars.length; i++) {
    const ch = chars[i] as string;
    if ("([{".includes(ch)) depth += 1;
    else if (")]}".includes(ch)) depth = Math.max(0, depth - 1);
    else if (depth === 0) {
      if (ch === "." && chars[i + 1] === " ") return chars.slice(0, i).join("");
      if (ch === " " && chars[i + 1] === "—" && chars[i + 2] === " ") {
        return chars.slice(0, i).join("");
      }
    }
  }
  return line.replace(/\.+$/, "");
}

/** The `docs` reply `{of, bundle, tier, verbs}` (python `_docs.render`). This
 *  kernel's bundles publish no EMITS / CANCEL / NOTES, so a detailed reply
 *  carries verbs alone. */
export function renderDocs(
  of: string,
  bundle: string | null,
  tier: string,
  verb: string | null,
  docs: { [verb: string]: string },
): Json {
  if (verb !== null && !(verb in docs)) {
    return refusal(BAD_ARG, `docs: ${of} has no verb ${pyRepr(verb)}`, {
      hint: `its verbs: ${Object.keys(docs).join(", ")}`,
      extra: { arg: "verb" },
    });
  }
  const names = verb !== null ? [verb] : Object.keys(docs);
  const verbs: Payload = {};
  for (const n of names) {
    const d = docs[n] as string;
    verbs[n] = tier === "detailed" ? fullDoc(d) : signature(d);
  }
  return { of, bundle, tier, verbs };
}
