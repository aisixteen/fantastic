import { estimateOne } from "../context.ts";
import type { ProjectionCtx, Turn } from "../context.ts";

// Shared helpers + the contract for the context-overflow strategies — port of
// python `ai_core/strategies/base.py`.
//
// A strategy is an async projection: given the conversation BODY (history + the
// live turn, WITHOUT the system block), return a `Projection` — the shortened body
// that fits `ctx.budget`, plus an artifact describing what it did. It does NOT
// fabricate a notice turn: the ONE canonical `[context-notice]` is composed at the
// seam. The durable store is never touched (except by `fold`, which appends).

/** A fixed token budget reserved at the seam for the notice WRAPPER prose
 *  (everything in the notice except a summary, which the strategy pays for) —
 *  ONE budget authority: the strategy nets this out before fitting its tail. An
 *  under-reserve puts the projection over budget and wakes the seam's backstop,
 *  which trims the pinned goal — so re-measure this if the notice text grows. */
export const NOTICE_ENVELOPE_RESERVE = 160;

export const STUB_SUMMARY = "[Earlier conversation omitted — summary unavailable]";

/** Drop leading `role:tool` messages whose owning call turn is not present — a
 *  wire that pairs tool replies with calls would reject them. */
export function dropOrphanTools(turns: Turn[]): Turn[] {
  let i = 0;
  while (i < turns.length && turns[i]!["role"] === "tool") i++;
  return turns.slice(i);
}

/** Keep the largest SUFFIX of `turns` that fits `budget`, always including the
 *  last turn (the live request), then drop any orphaned leading tool replies. */
export function fitTail(turns: Turn[], budget: number): Turn[] {
  if (turns.length === 0) return [];
  const last = turns[turns.length - 1] as Turn;
  const kept: Turn[] = [last];
  let used = estimateOne(last);
  for (let i = turns.length - 2; i >= 0; i--) {
    const c = estimateOne(turns[i] as Turn);
    if (used + c > budget) break;
    kept.unshift(turns[i] as Turn);
    used += c;
  }
  return dropOrphanTools(kept);
}

/** Split `body` into [overflow, recent] at the last `recentN` turns, snapping the
 *  boundary back so `recent` never STARTS on an orphaned `role:tool`. */
export function recentSplit(body: Turn[], recentN: number): [Turn[], Turn[]] {
  let start = Math.max(0, body.length - recentN);
  while (start > 0 && body[start]!["role"] === "tool") start--;
  return [body.slice(0, start), body.slice(start)];
}

/** Summarize the overflow via the provider; degrade to a stub on ANY failure (a
 *  degraded artifact — the full transcript is whole in the memory store). */
export async function safeSummary(ctx: ProjectionCtx, overflow: Turn[]): Promise<string> {
  if (ctx.summarize === null) return STUB_SUMMARY;
  let s: string;
  try {
    s = await ctx.summarize(overflow);
  } catch {
    return STUB_SUMMARY;
  }
  return (s ?? "").trim() || STUB_SUMMARY;
}
