import { asDict, asList, dim, err, head, line, num, refusalOf, str } from "./rows.ts";
import type { Row } from "./rows.ts";

// ram_store / sqlite_store: a COUNTS view. `dump` answers every row of every
// owner; the model keeps the owner name and two numbers, so no row body can be
// shown. `search` answers the caller's own lines, which is all it shows.

export interface StoreState {
  owners: Record<string, { rows: number; folds: number }>;
  total: number;
  /** why `dump` was refused, when it was */
  dumpError: string | null;
  loaded: boolean;
}
export interface Hits {
  query: string;
  matches: Array<{ id: number; line: number; text: string }>;
  total: number;
  truncated: boolean;
  error: string | null;
}

export function storeState(reply: unknown): StoreState {
  const s: StoreState = { owners: {}, total: 0, dumpError: refusalOf(reply), loaded: false };
  const d = asDict(reply);
  if (s.dumpError !== null || d === null) return s;
  const rows = asList(d["rows"]);
  for (const r of rows) (s.owners[str(r, "owner")] ??= { rows: 0, folds: 0 }).rows++;
  for (const f of asList(d["fold_refs"])) (s.owners[str(f, "owner")] ??= { rows: 0, folds: 0 }).folds++;
  s.total = rows.length;
  s.loaded = true;
  return s;
}

export function hitsState(query: string, reply: unknown): Hits {
  const matches = asList(asDict(reply)?.["matches"]).map((m) => ({ id: num(m, "id") ?? 0, line: num(m, "line") ?? 0, text: str(m, "text") }));
  return {
    query,
    matches,
    total: num(reply, "total") ?? matches.length,
    truncated: asDict(reply)?.["truncated"] === true,
    error: refusalOf(reply),
  };
}

export function storeRows(s: StoreState, hits: Hits | null): Row[] {
  const out: Row[] = [];
  if (s.dumpError !== null) out.push(err(`owners not readable: ${s.dumpError}`));
  else if (!s.loaded) out.push(dim("reading…", 1));
  else {
    const names = Object.keys(s.owners).sort();
    out.push(head(`OWNERS · ${names.length} · ${s.total} rows`));
    if (names.length === 0) out.push(dim("no rows yet", 1));
    for (const o of names) out.push(line(`${o}  ${s.owners[o]!.rows} rows  ${s.owners[o]!.folds} folds`, 1));
  }
  out.push(head("SEARCH"));
  if (hits === null) out.push(dim("search your own rows", 1));
  else if (hits.error !== null) out.push(err(`refused: ${hits.error}`));
  else {
    out.push(dim(`"${hits.query}" · ${hits.total} hits${hits.truncated ? " (truncated)" : ""}`, 1));
    for (const m of hits.matches) out.push(line(`${m.id}:${m.line}  ${m.text}`, 1));
  }
  return out;
}
