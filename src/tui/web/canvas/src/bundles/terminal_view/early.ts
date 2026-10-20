// The frames HELD while an attach ran (the job's output that landed before its
// ring replay came back), drained once the replay is drawn (owner 2026-10-09).
// The first `covered` of them are in the replay already: the host's replay
// resets the job's unacked count to 0 (pty.rs output_reply, exec_bridge
// _output), so their chars are no longer owed. An ack for them would take those
// chars off what arrived after the reset, so they are neither drawn nor acked.
// Pure: the view runs the plan.

export type Frame = Record<string, unknown>;

export type EarlyStep = { op: "end"; frame: Frame } | { op: "skip" } | { op: "draw"; frame: Frame };

/** What to do with each held frame, in order: a `job_done` ends the terminal; the
 *  job's own output within the first `covered` is in the replay (skipped, not
 *  acked); any other frame is drawn (a frame of another job is dropped there). */
export function earlyPlan(frames: Frame[], covered: number, jobId: number | undefined): EarlyStep[] {
  return frames.map((frame, i): EarlyStep => {
    if (frame["type"] === "job_done") return { op: "end", frame };
    if (i < covered && frame["job_id"] === jobId) return { op: "skip" };
    return { op: "draw", frame };
  });
}

/** Runs a plan: a drawn frame goes to `draw` (the view's draw acks what it
 *  writes), a skipped one goes nowhere (so it is not acked), a `job_done` to `end`. */
export function runEarly(plan: EarlyStep[], draw: (frame: Frame) => void, end: (frame: Frame) => void): void {
  for (const s of plan) {
    if (s.op === "end") end(s.frame);
    else if (s.op === "draw") draw(s.frame);
  }
}
