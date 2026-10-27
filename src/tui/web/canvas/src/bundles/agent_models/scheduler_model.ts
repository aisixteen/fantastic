import { asDict, asList, brief, dim, err, head, line, num, refusalOf, str } from "./rows.ts";
import type { Row } from "./rows.ts";

// scheduler: one row per job, and its last result from the newest history event.

export interface Job {
  id: string;
  target: string;
  verb: string;
  every: number;
  nextRun: number | null;
  paused: boolean;
  runCount: number;
}
export interface Fire {
  scheduleId: string;
  ts: number;
  result: string;
  error: string | null;
}
export interface SchedulerState {
  jobs: Job[];
  history: Fire[];
  refusal: string | null;
}

export function schedulerState(list: unknown, history: unknown): SchedulerState {
  const jobs = asList(asDict(list)?.["schedules"]).map((s): Job => ({
    id: str(s, "id"),
    target: str(s, "target"),
    verb: str(asDict(s)?.["payload"], "type"),
    every: num(s, "interval_seconds") ?? 0,
    nextRun: num(s, "next_run"),
    paused: asDict(s)?.["paused"] === true,
    runCount: num(s, "run_count") ?? 0,
  }));
  const fires = asList(asDict(history)?.["history"]).map((e): Fire => {
    const d = asDict(e) ?? {};
    return {
      scheduleId: str(d, "schedule_id"),
      ts: num(d, "ts") ?? 0,
      result: d["result"] === null || d["result"] === undefined ? "" : brief(d["result"], 60),
      error: d["error"] === null || d["error"] === undefined ? null : brief(d["error"], 80),
    };
  });
  return { jobs, history: fires, refusal: refusalOf(list) ?? refusalOf(history) };
}

/** "in 42s", or "due". */
export function nextText(next: number | null, now: number): string {
  if (next === null) return "-";
  return next > now ? `in ${Math.ceil(next - now)}s` : "due";
}

/** The newest fire of a schedule: the latest `ts`, the later entry on a tie. */
export function lastFire(history: Fire[], id: string): Fire | null {
  let best: Fire | null = null;
  for (const f of history) if (f.scheduleId === id && (best === null || f.ts >= best.ts)) best = f;
  return best;
}

export function schedulerRows(s: SchedulerState, now: number): Row[] {
  const out: Row[] = [];
  if (s.refusal !== null) out.push(err(`refused: ${s.refusal}`));
  out.push(head(`JOBS · ${s.jobs.length}`));
  if (s.jobs.length === 0) out.push(dim("nothing scheduled", 1));
  for (const j of s.jobs) {
    out.push(line(`${j.id}  ${j.target} · ${j.verb}  every ${j.every}s  next ${nextText(j.nextRun, now)}  runs ${j.runCount}${j.paused ? "  PAUSED" : ""}`, 1));
    const f = lastFire(s.history, j.id);
    out.push(f === null ? dim("last: no runs yet", 2) : f.error !== null ? err(`last: ERROR ${f.error}`) : dim(`last: ok ${f.result}`, 2));
  }
  return out;
}
