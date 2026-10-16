// A RE-ATTACH STARTS THE COUNT OVER (owner 2026-10-16, `claude` hung with no
// IO): the exec agent pauses a PTY's reader past 100 000 unacked chars and
// resumes it under 5 000, and what an earlier page drew but never acked (its
// batch under ACK_SIZE, the frames in flight at a reload) stays counted —
// reload after reload, until the job paused for good. The ring replay shows
// this page everything so far, so the attach acks what is left counted: the
// count is read (`ack chars:0` answers `{unacked}`), then acked in full.

type Ack = { type: "ack"; job_id: number; chars: number };
type Call = (target: string, payload: Ack) => Promise<unknown>;

export const clearBacklog = async (call: Call, backend: string, jobId: number): Promise<void> => {
  const r = (await call(backend, { type: "ack", job_id: jobId, chars: 0 })) as { unacked?: unknown } | null;
  const n = r?.unacked;
  if (typeof n === "number" && Number.isInteger(n) && n > 0) await call(backend, { type: "ack", job_id: jobId, chars: n });
};
