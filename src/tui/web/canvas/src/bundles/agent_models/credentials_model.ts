import { asDict, asList, dim, err, head, line, refusalOf, str } from "./rows.ts";
import type { Row } from "./rows.ts";

// credential_store: key NAMES, the host each is bound to, the field NAMES, and
// who may use the store (the reflect's `callable_by`). NEVER A VALUE: the parser
// keeps three whitelisted things per ref; any other key of a reply (a value, a
// length, a mask) is dropped here, so it cannot reach a row.

export interface CredRef {
  name: string;
  host: string | null;
  fields: string[];
}
export interface CredentialsState {
  refs: CredRef[];
  loaded: boolean;
  refusal: string | null;
}

/** Names only: a list keeps its strings; an object keeps its KEYS. */
function fieldNames(v: unknown): string[] {
  if (Array.isArray(v)) return v.filter((x): x is string => typeof x === "string");
  const d = asDict(v);
  return d === null ? [] : Object.keys(d);
}

export function credentialsState(reply: unknown): CredentialsState {
  const d = asDict(reply);
  const refs = asList(d?.["refs"]).map((r): CredRef => ({
    name: str(r, "ref"),
    host: str(r, "host") || null,
    fields: fieldNames(asDict(r)?.["fields"]),
  }));
  return { refs, loaded: d !== null && Array.isArray(d["refs"]), refusal: refusalOf(reply) };
}

/** caller -> verbs, from the store's own reflect (the baseline `$…` keys are left out). */
export function grantsOf(reflect: unknown): Array<[string, string]> {
  const m = asDict(asDict(reflect)?.["callable_by"]);
  if (m === null) return [];
  return Object.entries(m)
    .filter(([k]) => !k.startsWith("$"))
    .map(([k, v]): [string, string] => [k, typeof v === "string" ? v : asList(v).filter((x): x is string => typeof x === "string").join(", ")]);
}

export function credentialRows(s: CredentialsState, grants: Array<[string, string]>): Row[] {
  const out: Row[] = [];
  if (s.refusal !== null) out.push(err(`refused: ${s.refusal}`));
  out.push(head("REFS"));
  if (s.loaded && s.refs.length === 0) out.push(dim("no refs", 1));
  for (const r of s.refs) {
    out.push(line(`${r.name}  host ${r.host ?? "-"}`, 1));
    out.push(dim(`fields: ${r.fields.join(", ")}`, 2));
  }
  out.push(head("GRANTS · who may use the store"));
  if (grants.length === 0) out.push(dim("nobody named", 1));
  for (const [who, verbs] of grants) out.push(line(`${who}: ${verbs}`, 1));
  return out;
}
