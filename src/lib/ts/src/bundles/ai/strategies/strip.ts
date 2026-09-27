import { estimateOne, estimateTokens } from "../context.ts";
import type { Strategy, Turn } from "../context.ts";
import { NOTICE_ENVELOPE_RESERVE, dropOrphanTools, fitTail } from "./base.ts";
import { PID_TAG, isReply, pidOf, replyBlocks, stubBlock } from "../replies.ts";
import { loadView } from "../memory.ts";

// `strip` — the context is the ledger at MIXED RESOLUTION: bodies go, the call
// they answered stays. Port of python `strategies/strip.py`.
//
// When the window fills, the cheapest safe move is not to drop TURNS but to drop
// reply BODIES: a big read becomes one line — its tag, its title, its size, its
// pid — and stays reachable by `grep` while its body is held.
//
//     L0  full row     [reply to pid · notes read · "survey chunk 2"]\n<6800 bytes>
//     L1  stub         [reply to pid · notes read · "survey chunk 2"] (stripped: 6800 bytes) Every assistant turn survives
// untouched: the model's own prose after a reply IS its distillation of it.
//
// TWO RULES THAT KEEP THE KV CACHE WARM (an append after a cached prefix reuses
// it; any edit inside it reuses nothing):
//   * HYSTERESIS. Strip only past the HIGH watermark, and strip down to LOW in one
//     batch: ten stubs in one event is one cache miss, not ten.
//   * MONOTONE, OLDEST FIRST. Once stripped, always stripped, and the stripped
//     region is a contiguous run from the front — the stable prefix only extends.
// No LLM call. If every body is stripped and the prose alone still overflows, the
// second stage is the CUT — not a fallback, the next stage of the same strategy,
// and it says so in the artifact.

/** Strip down to this fraction of the budget once over it. */
export const LOW = 0.6;

/** Per-agent stripped pids — a VIEW decision (the store is untouched), so losing
 *  it costs one re-strip; seeded from the view array so stubs survive a restart. */
export const stripped: Map<string, Set<number | string>> = new Map();

/** The back half, snapped so it never STARTS on an orphaned `role:tool`. */
function halve(body: Turn[]): Turn[] {
  let cut = Math.floor(body.length / 2);
  while (cut > 0 && body[cut]!["role"] === "tool") cut--;
  return dropOrphanTools(body.slice(cut));
}

/** The pids of a reply turn's blocks. */
function pidsOf(m: Turn): (number | string)[] {
  const out: (number | string)[] = [];
  for (const b of replyBlocks(String(m["content"]))) {
    const pm = PID_TAG.exec(b);
    if (pm !== null) out.push(pidOf(pm[1] as string));
  }
  return out;
}

/** Project `body` with every stripped block reduced to its stub. Deterministic in
 *  (body, stripped) — the same inputs render the same bytes, which is what keeps
 *  the prefix stable between strip events. */
function renderStripped(body: Turn[], done: Set<number | string>): [Turn[], number, number] {
  let n = 0;
  let saved = 0;
  const out = body.map((m) => {
    if (!isReply(m)) return m;
    const parts = replyBlocks(String(m["content"])).map((b) => {
      const pm = PID_TAG.exec(b);
      if (pm === null || !done.has(pidOf(pm[1] as string))) return b;
      const stub = stubBlock(b);
      n += 1;
      saved += b.length - stub.length;
      return stub;
    });
    return { ...m, content: parts.join("\n\n") };
  });
  return [out, n, saved];
}

export const strip: Strategy = async (body, _systemBlock, _rec, ctx) => {
  if (body.length <= 1) return { body: fitTail(body, ctx.budget) };
  if (!stripped.has(ctx.selfId) && ctx.kernel !== null) {
    // SEEDED FROM THE VIEW ARRAY on first use — stubs survive a restart, and the
    // prefix bytes are the same before and after it.
    const seed = new Set<number | string>();
    const view = await loadView(ctx.selfId, ctx.kernel);
    if (view !== null) {
      const stubbed = new Set(
        (view["ids"] as { id: number; res: string }[]).filter((e) => e.res === "stub").map((e) => e.id),
      );
      for (const m of body) {
        if (stubbed.has(m["id"] as number) && isReply(m)) for (const p of pidsOf(m)) seed.add(p);
      }
    }
    stripped.set(ctx.selfId, seed);
  }
  let done = stripped.get(ctx.selfId);
  if (done === undefined) stripped.set(ctx.selfId, (done = new Set()));
  const budget = ctx.budget - NOTICE_ENVELOPE_RESERVE;
  let [rendered, n, saved] = renderStripped(body, done);
  if (estimateTokens(rendered) > budget) {
    // OVER THE HIGH WATERMARK: strip oldest-first, in ONE batch, down to LOW.
    const target = Math.floor(budget * LOW);
    for (const m of body.slice(1, -1)) { // never the goal, never the live turn
      if (estimateTokens(rendered) <= target) break;
      if (!isReply(m)) continue;
      for (const p of pidsOf(m)) done.add(p);
      [rendered, n, saved] = renderStripped(body, done);
    }
  }
  let cut = false;
  if (estimateTokens(rendered) > budget) {
    // Everything strippable is stripped and the prose alone still overflows.
    // Stage two: the cut. The goal turn stays pinned.
    const first = rendered[0]!;
    const tail = fitTail(halve(rendered.slice(1)), Math.max(0, budget - estimateOne(first)));
    rendered = [first, ...tail];
    cut = true;
  }
  return { body: rendered, omittedMarker: cut, stripped: n, strippedBytes: saved };
};
