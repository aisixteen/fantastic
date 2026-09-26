import { estimateOne } from "../context.ts";
import type { Strategy } from "../context.ts";
import { NOTICE_ENVELOPE_RESERVE, fitTail, recentSplit, safeSummary } from "./base.ts";

/**
 * `compact` — the DEFAULT strategy (python `strategies/compact.py`). Keep the
 * recent turns verbatim + an LLM summary of the overflow. The summary rides the
 * `Projection` artifact (NOT a fabricated turn); the seam wraps it into the one
 * canonical context-notice.
 *
 * PIN THE GOAL. The first turn is the task framing, and it is the OLDEST — the
 * first thing the overflow split takes. A summary is a lossy retelling; the one
 * turn that must survive verbatim is the instruction the whole run serves.
 */
export const compact: Strategy = async (body, _systemBlock, _rec, ctx) => {
  const [overflow, recent] = recentSplit(body, ctx.recentN);
  if (overflow.length === 0) return { body: fitTail(body, ctx.budget) };
  const summary = await safeSummary(ctx, overflow);
  // Reserve room for the notice the seam will prepend: the wrapper envelope + the
  // summary text itself (the seam embeds this exact summary). One budget authority.
  const summaryCost = estimateOne({ role: "user", content: summary });
  const avail = Math.max(0, ctx.budget - NOTICE_ENVELOPE_RESERVE - summaryCost);
  const first = body[0]!;
  const kept = fitTail(recent, Math.max(0, avail - estimateOne(first)));
  // Only re-add it if the recent window does not already carry it (short bodies).
  if (kept.length > 0 && kept[0] === first) return { body: kept, summary };
  return { body: [first, ...kept], summary };
};
