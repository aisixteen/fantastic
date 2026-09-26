import type { Json } from "../../../kernel/json.ts";
import { estimateOne, estimateTokens } from "../context.ts";
import type { ProjectionCtx, Strategy, Turn } from "../context.ts";
import { chainText, isFold, level, makeFold, render } from "../folds.ts";
import { NOTICE_ENVELOPE_RESERVE } from "./base.ts";
import { LOW, strip } from "./strip.ts";
import { isReply, replyMeta } from "../replies.ts";
import { saveHistory } from "../memory.ts";

// `fold` — self-inflicted, mechanical compaction that leaves MARKS the model can
// walk. Port of python `strategies/fold.py`.
//
// At the watermark, oldest first, one batch down to LOW:
//   1. FOLD the oldest stretch of the view — one summarizing call per span, one
//      appended fold row each with `refs`; the members stay one descent away
//   2. still over → STRIP bodies in what is left (the `strip` strategy)
//   3. still over → CUT (strip's own second stage)
//
// THE ONE STRATEGY THAT WRITES: a fold is a mark that must persist — the next
// turn has to see the same collapse, or the prefix rewrites every turn. Nothing
// is ever edited. A summary that stays over its budget (or is CUT OFF at it)
// after ONE re-summarise folds NOTHING — the span stays in view and the refusal
// is reported, never a truncated summary. A summarizer that FAILS still writes a
// degraded fold (refs + the members' call names) that says so on its own line.

const MAX_TITLE = 140;

/** A summary may take a fifth of the span it replaces, and never more than a
 *  quarter of the body budget: a fold that is itself a large fraction of the
 *  window buys nothing. */
export const SUMMARY_SHARE = 0.2;
export const WINDOW_SHARE = 0.25;
/** tokens — a title and one sentence always fit */
export const SUMMARY_FLOOR = 48;
/** THE THINKING ALLOWANCE on a summary call's `max_tokens`: the endpoint counts a
 *  reasoning model's thought against it; at a bare budget every small summary was
 *  cut. The summary TEXT, thought stripped, must still fit the budget. */
export const THINK_ALLOWANCE = 1024;

export const FOLD_PROMPT = (budget: number): string =>
  `FOLD this finished piece of work in AT MOST ~${budget} tokens. First line: a ` +
  "one-sentence title (what it was and how it ended). Then a blank line. Then these " +
  "sections, each one line or a few, in this order, omitting none (write `none`):\n" +
  "ASKED: what was asked.\n" +
  "DONE: what was found or decided — AND WHY.\n" +
  "BINDING: rules, constraints and choices that still bind.\n" +
  "ERRORS: every error, quoted verbatim.\n" +
  "OPEN: what is unfinished.\n" +
  "If the piece contains earlier fold summaries, MERGE them — on a conflict the newer " +
  "statement wins, and unfinished work is carried forward. Output only that.\n\n";

export const RESUMMARIZE = (got: number, budget: number): string =>
  `Your summary below is ~${got} tokens; the budget is ${budget}. Rewrite it to fit — ` +
  "keep the title line, the reasons, the binding rules and every quoted error; drop " +
  "narration. Output only the summary.\n\n";

export const RESUMMARIZE_CUT = (budget: number): string =>
  `Your summary below was CUT OFF at the budget (${budget} tokens) before it ended. ` +
  "Write it again, shorter, so it ENDS within the budget — keep the title line, the " +
  "reasons, the binding rules and every quoted error; drop narration. Output only " +
  "the summary.\n\n";

/** The degraded title: what the members were — the calls their replies answered
 *  (the ts counterpart of the titles on python's reply tags), or any quoted title. */
function titlesOf(rows: Turn[], chain: number[]): string {
  const out: string[] = [];
  for (const i of chain) {
    const r = rows[i]!;
    const meta = isReply(r) ? replyMeta(r) : null;
    if (meta !== null) out.push(`${meta.target} ${meta.verb}`);
    else {
      const m = /· "([^"]{1,140})"/.exec(String(r["content"] ?? ""));
      if (m !== null) out.push(m[1] as string);
    }
  }
  return [...new Set(out)].join("; ") || "(untitled)";
}

const PATH = /[\w./-]+\.(?:txt|md|py|json|ya?ml|js|ts|rs|swift|toml|csv|log|sh|html|css)\b/g;
const IDENT = /\b[0-9a-f]{8,}\b/g;
/** Runtime lines, not the user's words: python's set, plus the ts reply wrapper
 *  and the ts malformed-envelope notice. */
const RUNTIME = [
  "[reply to ",
  "[no call",
  "[context-notice",
  "[fold ",
  "[recall",
  "[late reply",
  "[cleared ",
  "[fork of ",
  "<tool_response>",
];
const TS_NOTICE = /^\[\d+ tool_call envelope/;
/** the user's own words, verbatim, at most this much per fold */
export const INDEX_USER_CHARS = 1200;

/** What a fold must never lose, extracted WITHOUT the model: the files and
 *  identifiers its members name, and what the USER said, verbatim (newest kept
 *  first when over the cap). Appended to the summary — no summariser can drop it. */
export function mechanicalIndex(rows: Turn[], chain: number[]): string {
  const text = chain.map((i) => String(rows[i]!["content"] ?? "")).join("\n");
  const files = [...new Set(text.match(PATH) ?? [])].sort().slice(0, 30);
  const idents = [...new Set(text.match(IDENT) ?? [])].sort().slice(0, 20);
  const said: string[] = [];
  let used = 0;
  for (const i of [...chain].reverse()) {
    const r = rows[i]!;
    let c = String(r["content"] ?? "").trim();
    if (r["role"] !== "user" || r["kind"] || c === "" || RUNTIME.some((p) => c.startsWith(p)) || TS_NOTICE.test(c)) {
      continue;
    }
    c = c.split(/\s+/).filter(Boolean).join(" ");
    if (used + c.length > INDEX_USER_CHARS) c = c.slice(0, Math.max(0, INDEX_USER_CHARS - used)) + " …";
    said.unshift(c);
    used += c.length;
    if (used >= INDEX_USER_CHARS) break;
  }
  const lines: string[] = [];
  if (files.length > 0) lines.push("files: " + files.join(", "));
  if (idents.length > 0) lines.push("identifiers: " + idents.join(", "));
  for (const u of said) lines.push(`user said: ${u}`);
  return lines.length > 0 ? "INDEX (mechanical):\n" + lines.join("\n") : "";
}

export function summaryBudget(ctx: ProjectionCtx, rows: Turn[], chain: number[]): number {
  const span = estimateTokens(chain.map((i) => rows[i]!));
  return Math.max(SUMMARY_FLOOR, Math.min(Math.floor(span * SUMMARY_SHARE), Math.floor(ctx.budget * WINDOW_SHARE)));
}

function stripEnds(s: string, chars: string): string {
  let a = 0;
  let b = s.length;
  while (a < b && chars.includes(s[a] as string)) a++;
  while (b > a && chars.includes(s[b - 1] as string)) b--;
  return s.slice(a, b);
}

function split(text: string): [string, string] | null {
  const lines = text.split(/\r?\n/).map((l) => l.trim()).filter((l) => l !== "");
  if (lines.length === 0 || (lines[0] as string).startsWith("<think")) return null;
  const title = stripEnds(stripEnds(lines[0] as string, '"'), "*#").trim();
  const body = lines.slice(1).join("\n").trim();
  return [title.slice(0, MAX_TITLE) || "(untitled)", body || title];
}

/**
 * [title, body, degraded], or null when the summary stays OVER ITS BUDGET (or is
 * CUT OFF by it) after one re-summarise — then nothing is folded.
 *
 * The budget is stated in the prompt; the call's `max_tokens` is the budget plus
 * `THINK_ALLOWANCE`; `reserve` is what the fold row spends on its mechanical
 * index, so summary + index fit. A provider that stopped at `max_tokens` says so
 * (`lastCut` on the summarizer) — a cut-off summary is treated like an oversized
 * one, never kept.
 */
export async function summarizeSpan(
  ctx: ProjectionCtx,
  rows: Turn[],
  chain: number[],
  reserve = 0,
): Promise<[string, string, boolean] | null> {
  const summarize = ctx.summarize;
  if (summarize === null) {
    const t = titlesOf(rows, chain);
    return [t.slice(0, MAX_TITLE), t, true];
  }
  const budget = Math.max(SUMMARY_FLOOR, summaryBudget(ctx, rows, chain) - reserve);
  const ask = async (content: string): Promise<[string, boolean]> => {
    let text: string;
    try {
      text = ((await summarize([{ role: "user", content }], budget + THINK_ALLOWANCE)) ?? "").trim();
    } catch {
      return ["", false];
    }
    return [text, Boolean(summarize.lastCut)];
  };
  let [text, cut] = await ask(FOLD_PROMPT(budget) + chainText(rows, chain));
  if (text === "") {
    const t = titlesOf(rows, chain);
    return [t.slice(0, MAX_TITLE), t, true];
  }
  const got = estimateOne({ role: "user", content: text });
  if (cut || got > budget) {
    // ONE re-summarise, telling the model the numbers.
    const tell = cut ? RESUMMARIZE_CUT(budget) : RESUMMARIZE(got, budget);
    [text, cut] = await ask(tell + text);
    if (text === "" || cut || estimateOne({ role: "user", content: text }) > budget) return null;
  }
  const parts = split(text);
  if (parts === null) {
    const t = titlesOf(rows, chain);
    return [t.slice(0, MAX_TITLE), t, true];
  }
  return [parts[0], parts[1], false];
}

/** rows at the end of the view that never fold — the live tail */
export const KEEP_TAIL = 6;
/** rows per fold at the watermark — small enough to summarize, large enough to matter */
export const SPAN = 8;
/** Adjacent folds at the head of the view before they fold as a group. A real run
 *  folds at most once per turn, so the head accumulates folds one at a time and
 *  this threshold is the whole story (at four, measured runs never nested). */
export const FOLD_RUN = 3;

/**
 * Summarize `idxs` and append ONE fold row for them — or null when the summary
 * would not fit its budget (nothing folded). A fold row among `idxs` is just a
 * member — that is how folds nest: its level is one above the deepest member.
 */
export async function writeFold(ctx: ProjectionCtx, rows: Turn[], idxs: number[]): Promise<Turn | null> {
  if (ctx.kernel === null) throw new Error("fold: no kernel to mint the fold's id from");
  const ids = idxs.map((i) => (rows[i]!["id"] ?? null) as Json);
  const lvl = 1 + Math.max(0, ...idxs.filter((i) => isFold(rows[i]!)).map((i) => level(rows, rows[i]!)));
  // THE INDEX IS INSIDE THE BUDGET: built first, its size reserved from the summary's.
  const index = mechanicalIndex(rows, idxs);
  const reserve = index !== "" ? estimateOne({ role: "user", content: index }) : 0;
  const summarized = await summarizeSpan(ctx, rows, idxs, reserve);
  if (summarized === null) return null;
  const [title, body, degraded] = summarized;
  const text = index !== "" ? `${body}\n\n${index}` : body;
  const fr = makeFold(ctx.selfId, ids, title, text, degraded, {
    id: ctx.kernel.mintId(),
    lvl,
    parent: rows[idxs[0] as number]!["parent"] ?? null,
  });
  if (ctx.clientId !== null) await saveHistory(ctx.selfId, ctx.kernel, [fr]);
  return fr;
}

/**
 * The oldest foldable stretch of the VIEW, as indices into `rows`. Never the goal
 * turn, never the last `keepTail` rows, never ending between a call and its
 * answer. Folds are ordinary members but NOT the first pick: a stretch that starts
 * on the previous fold wraps it, and doing that every time builds a linked list of
 * summaries-of-summaries. So: skip the folds at the head and take the oldest
 * NON-fold rows; only when `FOLD_RUN` folds sit adjacent at the head do they
 * become the span themselves. Depth then grows with the log of the history.
 */
export function nextSpan(rows: Turn[], view: Turn[], keepTail = KEEP_TAIL, size = SPAN): number[] {
  const byId = new Map<Json | undefined, number>();
  rows.forEach((r, i) => {
    if (r["id"] !== undefined && r["id"] !== null) byId.set(r["id"], i);
  });
  const head = view.length > keepTail + 1 ? view.slice(1, -keepTail).filter((r) => byId.has(r["id"])) : [];
  let lead = 0;
  while (lead < head.length && isFold(head[lead]!)) lead++;
  let take: Turn[];
  if (lead >= FOLD_RUN) {
    take = head.slice(0, lead); // fold the folds — same operation, same summarizer
  } else {
    take = head.slice(lead, lead + size);
    while (take.length > 0 && take[take.length - 1]!["role"] === "assistant") take = take.slice(0, -1);
  }
  return take.length >= 2 ? take.map((r) => byId.get(r["id"]) as number) : [];
}

export const fold: Strategy = async (body, systemBlock, rec, ctx) => {
  const rows = [...body];
  let view = render(rows);
  const budget = ctx.budget - NOTICE_ENVELOPE_RESERVE;
  let folded = 0;
  let refused = 0;
  if (estimateTokens(view) > budget && rows.length > 2) {
    const target = Math.floor(budget * LOW);
    // ONE MOVE, REPEATED: fold the oldest stretch of the view — rows, folds, or a
    // mix — until under the low watermark or nothing foldable is left.
    while (estimateTokens(view) > target) {
      const span = nextSpan(rows, view);
      if (span.length === 0) break;
      const fr = await writeFold(ctx, rows, span);
      if (fr === null) {
        refused += 1; // the summary would not fit: this span stays; stop here
        break;
      }
      rows.push(fr);
      folded += 1;
      view = render(rows);
    }
  }
  // TIDY THE HEAD, budget or no budget: FOLD_RUN adjacent folds at the head fold
  // as a group even when the view already fits — else a session grows one fold
  // per span at the head, forever. Terminates: each pass replaces FOLD_RUN folds
  // with ONE, and nextSpan stops selecting once fewer than FOLD_RUN remain.
  while (refused === 0) {
    const span = nextSpan(rows, view);
    if (span.length === 0 || !span.every((i) => isFold(rows[i]!))) break;
    const fr = await writeFold(ctx, rows, span);
    if (fr === null) {
      refused += 1;
      break;
    }
    rows.push(fr);
    folded += 1;
    view = render(rows);
  }
  // Stages two and three: strip bodies in what is left, then cut — strip's own logic.
  const out = await strip(view, systemBlock, rec, ctx);
  return {
    body: out.body,
    omittedMarker: out.omittedMarker ?? false,
    stripped: out.stripped ?? 0,
    strippedBytes: out.strippedBytes ?? 0,
    folded,
    foldRefused: refused,
  };
};
