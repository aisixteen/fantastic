import { asDict, asList, dim, err, head, line, num, refusalOf, str } from "./rows.ts";
import type { Dict, Row } from "./rows.ts";

// agentic_resources: roles (name, purpose, tier), the fleet it spawned, and the
// counts against their limits. `fleet` carries {id} per agent; a parent or a
// paused flag shows only when an entry carries one. No answer is active.

export interface Count {
  running: number;
  max: number | null;
}
export interface ResourcesState {
  roles: Array<{ name: string; purpose: string; tier: string | null }>;
  fleet: Record<string, Array<{ id: string; parent: string | null; paused: boolean }>>;
  total: Count | null;
  byRole: Record<string, Count>;
  refusal: string | null;
}

export const countOf = (r: unknown): Count | null => {
  const running = num(r, "running");
  if (running === null) return null;
  return { running, max: num(r, "max") };
};

export const countText = (c: Count): string => (c.max === null ? `${c.running} · no limit` : `${c.running}/${c.max}`);

/** The raw replies a view read: `roles`, `fleet`, `get_count {}`, and per role its card and count. */
export interface ResourcesReplies {
  roles?: unknown;
  fleet?: unknown;
  total?: unknown;
  cards?: Record<string, unknown>;
  counts?: Record<string, unknown>;
}

export function resourcesState(r: ResourcesReplies): ResourcesState {
  const s: ResourcesState = { roles: [], fleet: {}, total: countOf(r.total), byRole: {}, refusal: null };
  s.refusal = refusalOf(r.roles) ?? refusalOf(r.fleet) ?? refusalOf(r.total);
  for (const x of asList(asDict(r.roles)?.["roles"])) {
    const name = str(x, "name");
    const card = r.cards?.[name];
    s.roles.push({ name, purpose: str(x, "purpose"), tier: str(card, "tier") || null });
  }
  const fleet = asDict(asDict(r.fleet)?.["roles"]);
  for (const [role, list] of Object.entries(fleet ?? {})) {
    s.fleet[role] = asList(list).map((m) => {
      const d = asDict(m) ?? {};
      return {
        id: str(d, "id"),
        parent: str(d, "parent") || str(d, "spawned_by") || null,
        paused: d["paused"] === true || str(d, "status") === "paused",
      };
    });
  }
  for (const [role, c] of Object.entries(r.counts ?? {})) {
    const k = countOf(c);
    if (k !== null) s.byRole[role] = k;
  }
  return s;
}

export function resourcesRows(s: ResourcesState): Row[] {
  const out: Row[] = [];
  if (s.refusal !== null) out.push(err(`refused: ${s.refusal}`));
  out.push(head("TOTAL"), line(s.total === null ? "reading…" : countText(s.total), 1));
  out.push(head("ROLES"));
  if (s.roles.length === 0) out.push(dim("no roles", 1));
  for (const r of s.roles) {
    const c = s.byRole[r.name];
    out.push(line(`${r.name}  tier ${r.tier ?? "?"}  ${c === undefined ? "?" : countText(c)}`, 1));
    if (r.purpose !== "") out.push(dim(r.purpose, 2));
  }
  out.push(head("FLEET"));
  const all = Object.entries(s.fleet).flatMap(([role, ms]) => ms.map((m) => ({ role, ...m })));
  if (all.length === 0) out.push(dim("nothing spawned", 1));
  for (const m of all) out.push(line(`${m.id}  ${m.role}  parent ${m.parent ?? "-"}  ${m.paused ? "paused" : "active"}`, 1));
  return out;
}

export type { Dict };
