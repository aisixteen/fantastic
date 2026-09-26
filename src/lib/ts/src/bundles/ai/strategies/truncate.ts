import { estimateOne } from "../context.ts";
import type { Strategy } from "../context.ts";
import { NOTICE_ENVELOPE_RESERVE, fitTail } from "./base.ts";

/** `truncate` (python `strategies/truncate.py`) — keep the first (task-framing)
 *  turn + the recent turns, drop the middle. Cheap: NO summarizer call. The
 *  elision is reported via `omittedMarker`, never a fabricated turn. */
export const truncate: Strategy = async (body, _systemBlock, _rec, ctx) => {
  if (body.length <= 1) return { body: fitTail(body, ctx.budget) };
  const first = body[0]!;
  // Reserve room for the notice the seam prepends (no summary in truncate).
  const headCost = estimateOne(first) + NOTICE_ENVELOPE_RESERVE;
  const tail = fitTail(body.slice(1), Math.max(0, ctx.budget - headCost));
  return { body: [first, ...tail], omittedMarker: true };
};
