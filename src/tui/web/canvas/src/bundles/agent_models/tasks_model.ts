import { asDict, asList, brief, dim, err, head, line, refusalOf, str } from "./rows.ts";
import type { Row } from "./rows.ts";

// task_registry: the task tree. Roots first, children under their parent, ids
// sorted; an orphan or a cycle member falls to the root level. `versions` shows
// when an entry carries it (#437) and is absent without it.

export interface TaskEntry {
  id: string;
  parent: string | null;
  source: string;
  updateTo: string;
  status: string;
  holder: string;
  history: Array<{ from: string; to: string; by: string }>;
  report: string | null;
  versions: string | null;
}
export interface TasksState {
  tasks: TaskEntry[];
  refusal: string | null;
}

export function holderText(v: unknown): string {
  if (typeof v === "string" && v !== "") return v;
  const d = asDict(v);
  if (d !== null && typeof d["subtree"] === "string") return `subtree:${d["subtree"]}`;
  return "unheld";
}

function versionsText(v: unknown): string | null {
  if (typeof v === "string") return v === "" ? null : `v${v}`;
  if (typeof v === "number") return `v${v}`;
  if (Array.isArray(v)) return v.length === 0 ? null : `versions [${v.map((x) => brief(x, 24)).join(", ")}]`;
  if (v === null || v === undefined) return null;
  return `versions ${brief(v, 40)}`;
}

export function tasksState(reply: unknown): TasksState {
  const refusal = refusalOf(reply);
  const tasks = asList(asDict(reply)?.["tasks"]).map((t): TaskEntry => {
    const d = asDict(t) ?? {};
    const r = asDict(d["report"]);
    const res = r?.["result"];
    return {
      id: str(d, "id"),
      parent: str(d, "parent") || null,
      source: str(d, "source"),
      updateTo: str(d, "update_to"),
      status: str(d, "status"),
      holder: holderText(d["holder"]),
      history: asList(d["history"]).map((h) => ({ from: holderText(asDict(h)?.["from"]), to: holderText(asDict(h)?.["to"]), by: str(h, "by") })),
      report: r === null ? null : `${str(r, "from")} ${str(r, "state")}${res === undefined || res === null ? "" : ` ${brief(res, 60)}`}`.trim(),
      versions: versionsText(d["versions"]),
    };
  });
  return { tasks, refusal };
}

/** [depth, task] in display order. */
export function taskOrder(tasks: TaskEntry[]): Array<[number, TaskEntry]> {
  const sorted = [...tasks].sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
  const ids = new Set(sorted.map((t) => t.id));
  const seen = new Set<TaskEntry>();
  const out: Array<[number, TaskEntry]> = [];
  const walk = (t: TaskEntry, d: number): void => {
    if (seen.has(t)) return;
    seen.add(t);
    out.push([d, t]);
    for (const c of sorted) if (c !== t && c.parent === t.id) walk(c, d + 1);
  };
  for (const t of sorted) if (t.parent === null || !ids.has(t.parent)) walk(t, 0);
  for (const t of sorted) walk(t, 0); // what is left is a cycle
  return out;
}

export function tasksRows(s: TasksState, expanded: ReadonlySet<string>): Row[] {
  const out: Row[] = [];
  if (s.refusal !== null) out.push(err(`refused: ${s.refusal}`));
  out.push(head(`TASKS · ${s.tasks.length}`));
  if (s.tasks.length === 0) out.push(dim("no tasks", 1));
  for (const [d, t] of taskOrder(s.tasks)) {
    out.push(line(`${t.id}  ${t.status}  ${t.holder}  ${t.source}${t.versions === null ? "" : `  ${t.versions}`}`, 1 + d, t.id));
    if (!expanded.has(t.id)) continue;
    if (t.updateTo !== "" && t.updateTo !== t.source) out.push(dim(`update_to ${t.updateTo}`, 2 + d));
    if (t.history.length === 0) out.push(dim("no reassign history", 2 + d));
    for (const h of t.history) out.push(dim(`${h.from} -> ${h.to} by ${h.by}`, 2 + d));
    if (t.report !== null) out.push(dim(`report ${t.report}`, 2 + d));
  }
  return out;
}
