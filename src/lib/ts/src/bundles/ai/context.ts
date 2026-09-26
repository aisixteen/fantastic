import type { Kernel } from "../../kernel/kernel.ts";
import type { Json, Payload } from "../../kernel/json.ts";
import { pyJsonLen } from "../../kernel/ledger.ts";

// Context-window budgeting primitives for the compaction strategies — port of
// python `ai_core/context.py` + the `Projection` contract of `strategies/base.py`.
//
// A char-based token ESTIMATE — deliberately tokenizer-agnostic. Exactness is
// irrelevant for a fit-to-window budget (we under-fill with an output reserve),
// and any one tokenizer would be the wrong one for most models. Measured over
// python's `json.dumps` form (`pyJsonLen`), so a row costs the same here as on
// the host runtimes.

export const CHARS_PER_TOKEN = 4;
/** The window a caller gets when they say NOTHING — one that fits this kernel's
 *  own prompt with room to work. A smaller real window is named on the record
 *  (`context_window`, or `num_ctx`). */
export const DEFAULT_CONTEXT_WINDOW = 131072;
export const DEFAULT_OUTPUT_RESERVE = 1024;
/** Never project to a budget below this. */
export const BUDGET_FLOOR = 256;

/** A row or a message as the view holds it: role + content, plus the row's keys
 *  (id, parent, kind, refs, reply, …) that ride along. */
export type Turn = Payload & { role?: string; content?: Json };

/** Rough token estimate for ONE message — its SERIALIZED form, because every key
 *  a message carries consumes real context. */
export function estimateOne(message: Payload): number {
  return Math.ceil(pyJsonLen(message) / CHARS_PER_TOKEN);
}

export function estimateTokens(messages: readonly Payload[]): number {
  let n = 0;
  for (const m of messages) n += estimateOne(m);
  return n;
}

function posInt(v: Json | undefined): number | null {
  if (typeof v === "number" && v > 0) return Math.floor(v);
  if (typeof v === "string" && /^\d+$/.test(v.trim()) && Number(v) > 0) return Number(v);
  return null;
}

/** The model's usable window, by STATIC precedence (no fallback chain):
 *  `context_window` → `num_ctx` → the default. */
export function resolveContextWindow(rec: Payload): number {
  return posInt(rec["context_window"]) ?? posInt(rec["num_ctx"]) ?? DEFAULT_CONTEXT_WINDOW;
}

export function outputReserve(rec: Payload): number {
  const v = rec["output_reserve"];
  if (typeof v === "number" && Number.isInteger(v) && v >= 0) return v;
  if (typeof v === "string" && /^\d+$/.test(v.trim())) return Number(v);
  return DEFAULT_OUTPUT_RESERVE;
}

/** Token budget for the INPUT (window minus output headroom), floored. */
export function budget(rec: Payload): number {
  return Math.max(resolveContextWindow(rec) - outputReserve(rec), BUDGET_FLOOR);
}

/** Tokens a reply may occupy INLINE in the model's next turn; above this it is
 *  held in RAM and digested to its origin line. An eighth of the input budget. */
export function inlineCap(rec: Payload): number {
  return Math.max(BUDGET_FLOOR, Math.floor(budget(rec) / 8));
}

/** The side call that writes a summary: a plain completion over the provider.
 *  `maxTokens` bounds its output; `lastCut` says whether the provider stopped AT
 *  that bound, so a cut-off summary is never kept as if it were whole. */
export interface Summarizer {
  (msgs: Turn[], maxTokens?: number): Promise<string>;
  lastCut?: boolean;
}

/** Everything a strategy needs. `budget` is the budget for the conversation
 *  BODY (the window budget minus the already-measured system block). */
export interface ProjectionCtx {
  budget: number;
  recentN: number;
  summarize: Summarizer | null;
  selfId: string;
  /** null in a pure unit test; `fold` needs one (it mints ids and writes rows). */
  kernel: Kernel | null;
  /** The conversation this projection is for — `fold`, the one strategy that
   *  WRITES, persists its rows only when set. */
  clientId: string | null;
}

/** What a strategy returns: the projected body + the artifact the seam needs to
 *  compose the ONE canonical context-notice. Never a fabricated turn. */
export interface Projection {
  body: Turn[];
  /** compact: the summary text. */
  summary?: string | null;
  /** truncate / a cut: an earlier span was elided in place. */
  omittedMarker?: boolean;
  /** reply bodies reduced to their stub this projection, and the bytes that left. */
  stripped?: number;
  strippedBytes?: number;
  /** fold: fold rows written this projection. */
  folded?: number;
  /** fold: spans NOT folded because the summary stayed over its budget. */
  foldRefused?: number;
}

/** (body, system block, record, ctx) → Projection. The body excludes the system
 *  block; the durable store is never touched (except by `fold`, which appends). */
export type Strategy = (
  body: Turn[],
  systemBlock: Turn[],
  rec: Payload,
  ctx: ProjectionCtx,
) => Promise<Projection>;
