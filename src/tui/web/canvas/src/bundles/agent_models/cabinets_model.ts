import { asDict, asList, dim, err, head, line, num, refusalOf, str } from "./rows.ts";
import type { Row } from "./rows.ts";

// cabinet: a manager's `cabinets`, or one cabinet cell's `view`. Which one it is
// comes from the reply: a manager answers {cabinets:[..]}, a cell {task, worker, status}.

export interface Cab {
  id: string;
  task: string;
  worker: string;
  canvas: string;
  line: string;
  progress: number | null;
  equipment: number;
}
export interface CabinetsState {
  manager: boolean;
  cabs: Cab[];
  refusal: string | null;
}

function cab(c: unknown): Cab {
  const st = asDict(asDict(c)?.["status"]);
  const eq = asDict(c)?.["equipment"];
  return {
    id: str(c, "id"),
    task: str(c, "task"),
    worker: str(c, "worker"),
    canvas: str(c, "canvas"),
    line: st === null ? "" : str(st, "line"),
    progress: st === null ? null : num(st, "progress"),
    equipment: Array.isArray(eq) ? eq.length : eq !== null && typeof eq === "object" ? Object.keys(eq).length : typeof eq === "number" ? eq : 0,
  };
}

export function cabinetsState(reply: unknown): CabinetsState {
  const d = asDict(reply);
  const refusal = refusalOf(reply);
  if (d !== null && Array.isArray(d["cabinets"])) return { manager: true, cabs: asList(d["cabinets"]).map(cab), refusal };
  if (d !== null && ["task", "worker", "status"].some((k) => k in d)) return { manager: false, cabs: [cab(d)], refusal };
  return { manager: false, cabs: [], refusal };
}

export function cabinetsRows(s: CabinetsState): Row[] {
  const out: Row[] = [];
  if (s.refusal !== null) out.push(err(`refused: ${s.refusal}`));
  if (s.manager) out.push(head(`CABINETS · ${s.cabs.length}`), ...(s.cabs.length === 0 ? [dim("no cabinets yet", 1)] : []));
  for (const c of s.cabs) {
    out.push(line(`${c.id}  ${c.task}`, 1));
    out.push(dim(`worker ${c.worker}  canvas ${c.canvas}  ${c.equipment} equipment`, 2));
    out.push(dim(`${c.line === "" ? "not reported yet" : c.line}${c.progress === null ? "" : `  ${Math.round(c.progress)}%`}`, 2));
  }
  return out;
}
