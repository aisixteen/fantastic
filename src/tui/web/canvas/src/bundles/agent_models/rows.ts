// The shared shape of the read-only agent views (resources, tasks, store,
// scheduler, cabinets, credentials): a model turns a reply into ROWS, a view
// draws them as text. No DOM here. A model keeps only whitelisted keys of a
// reply, so nothing it does not name can reach a row.

export type Dict = Record<string, unknown>;

export interface Row {
  kind: "head" | "line" | "dim" | "err";
  text: string;
  /** tree depth, for indentation */
  depth?: number;
  /** a row that can be clicked (expand) carries the id it acts on */
  id?: string;
}

export const head = (text: string): Row => ({ kind: "head", text });
export const line = (text: string, depth = 0, id?: string): Row => ({ kind: "line", text, depth, ...(id !== undefined ? { id } : {}) });
export const dim = (text: string, depth = 0): Row => ({ kind: "dim", text, depth });
export const err = (text: string): Row => ({ kind: "err", text });

export const asDict = (v: unknown): Dict | null => (v !== null && typeof v === "object" && !Array.isArray(v) ? (v as Dict) : null);
export const asList = (v: unknown): unknown[] => (Array.isArray(v) ? v : []);
export const str = (v: unknown, k: string): string => {
  const d = asDict(v);
  const x = d === null ? undefined : d[k];
  return typeof x === "string" ? x : "";
};
export const num = (v: unknown, k: string): number | null => {
  const d = asDict(v);
  const x = d === null ? undefined : d[k];
  return typeof x === "number" && Number.isFinite(x) ? x : null;
};

/** The text of a refusal reply (`{error}`), or null. */
export function refusalOf(reply: unknown): string | null {
  const d = asDict(reply);
  if (d === null || d["error"] === undefined || d["error"] === null) return null;
  const e = typeof d["error"] === "string" ? d["error"] : JSON.stringify(d["error"]);
  return typeof d["message"] === "string" ? `${e}: ${d["message"]}` : e;
}

/** One line of any value, cut to `n` characters. */
export function brief(v: unknown, n: number): string {
  const s = (typeof v === "string" ? v : JSON.stringify(v) ?? "").replace(/\n/g, " ");
  return s.length > n ? `${s.slice(0, n)}…` : s;
}
