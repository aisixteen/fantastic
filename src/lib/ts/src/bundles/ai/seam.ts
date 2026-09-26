import type { Kernel } from "../../kernel/kernel.ts";
import type { Json, Payload } from "../../kernel/json.ts";
import { nowSeconds } from "../../kernel/timers.ts";
import {
  BUDGET_FLOOR,
  budget,
  estimateOne,
  estimateTokens,
  resolveContextWindow,
} from "./context.ts";
import type { ProjectionCtx, Summarizer, Turn } from "./context.ts";
import { withoutThinking } from "./think.ts";
import {
  applyRes,
  dedupeRepeatBodies,
  dedupeStaleReads,
  dropAnsweredReflects,
  resOf,
} from "./replies.ts";
import { aiMem, loadHistory, loadView, saveView } from "./memory.ts";
import { DEFAULT_STRATEGY, getStrategy, strategyNames } from "./strategies/index.ts";
import { NOTICE_ENVELOPE_RESERVE, dropOrphanTools } from "./strategies/base.ts";

// THE CONTEXT SEAM — view assembly + the per-agent projection. Port of python
// `ai_core/core.py` `_assemble` / `_project_context` / `_context_notice` /
// `_make_summarizer`.
//
// Every provider pass (python: every turn) the model's view is ASSEMBLED from the
// memory store — the view array when one resolves, the trimmed head otherwise —
// and PROJECTED to fit the agent's budget by its `context_strategy`: the ONE
// canonical `[context-notice]` is prepended, a `context` event is pushed to the
// caller, and the view array is written back so the next pass rebuilds the same
// bytes. The durable store keeps every turn; only the view is shaped.

/** Compaction fires once the calibrated prompt passes this share of the budget —
 *  early enough that the summariser itself has headroom. */
export const TRIGGER = 0.8;
/** fold rows between recalls */
export const RECALL_EVERY = 3;

/** What a provider streams, plus what it reports about the last stream. */
export interface Provider {
  chat(messages: { role: string; content: string }[], maxTokens?: number): AsyncIterable<string>;
  /** The endpoint's own count of the last prompt (calibrates the estimate), or null. */
  lastPromptTokens?: number | null;
  /** Why the last stream ended — "max_tokens" when it stopped AT the output bound. */
  lastStopReason?: string | null;
}

export function recentN(rec: Payload): number {
  const n = Number.parseInt(String(rec["recent_n"] ?? ""), 10);
  return Math.max(1, Math.min(Number.isNaN(n) ? 6 : n, 50));
}

/** The rows the model reads as chat messages: role + content, nothing else. */
export function renderForModel(messages: Turn[]): { role: string; content: string }[] {
  return messages.map((m) => {
    const role = String(m["role"] ?? "user");
    const c = m["content"];
    return { role: role === "tool" ? "user" : role, content: typeof c === "string" ? c : JSON.stringify(c ?? null) };
  });
}

/** Route a stream/context event to the caller: the agent's OWN inbox, tagged
 *  with the client id (its watchers — a view — see it). This kernel has no `cli`
 *  agent to send it to. */
export function toCaller(kernel: Kernel, selfId: string, clientId: string, ev: Payload): void {
  kernel.emit(selfId, { ...ev, client_id: clientId });
}

/**
 * Build the view for one pass. The conversation is ALWAYS this agent's own (its
 * memory store). THE VIEW ARRAY IS THE STATE: when one exists and every id it
 * names still resolves, the model sees exactly those rows at those resolutions,
 * then every row appended since, trimmed; otherwise the trimmed head. Trimmed =
 * repeat bodies stubbed, stale whole-state reads deduped, answered reflects
 * dropped — the VIEW only; the store keeps every turn. `live` is the incoming
 * message not yet stored (the first pass of a `send`), or null.
 *
 * Returns the messages and `storeLen` — how many turns the head held; the view is
 * deduped, so its length is NOT a store index, and the reaction cursor needs one.
 */
export async function assemble(
  selfId: string,
  kernel: Kernel,
  live: string | null,
  systemContent: string,
): Promise<{ messages: Turn[]; storeLen: number }> {
  const messages: Turn[] = [{ role: "system", content: systemContent }];
  const hist = await loadHistory(selfId, kernel);
  const view = await loadView(selfId, kernel);
  const entries = view !== null ? (view["ids"] as { id: number; res: string }[]) : [];
  const byId = new Map<Json | undefined, Turn>();
  for (const r of hist) if (r["id"] !== undefined) byId.set(r["id"], r);
  const mem = aiMem(kernel);
  if (entries.length > 0 && entries.every((e) => byId.has(e.id))) {
    for (const e of entries) messages.push(applyRes(byId.get(e.id)!, e.res ?? "full"));
    // THE ARRAY IS THE HEAD AS OF ITS WRITE; THE LOG KEEPS GROWING. Every row
    // appended since rides behind it, in full.
    const idx = new Map<Json | undefined, number>();
    hist.forEach((r, i) => idx.set(r["id"], i));
    const newest = Math.max(...entries.map((e) => idx.get(e.id) as number));
    messages.push(...dropAnsweredReflects(dedupeStaleReads(dedupeRepeatBodies(hist.slice(newest + 1)))));
    mem.viewSource.set(selfId, "array");
  } else {
    messages.push(...dropAnsweredReflects(dedupeStaleReads(dedupeRepeatBodies(hist))));
    mem.viewSource.set(selfId, view === null || entries.length === 0 ? "store" : "store (stale array)");
  }
  if (live !== null) messages.push({ role: "user", content: live });
  return { messages, storeLen: hist.length };
}

/** The summary side call over the provider: plain text, no tools; `maxTokens`
 *  bounds the output, `lastCut` records whether the provider stopped AT it. */
export function makeSummarizer(provider: Provider): Summarizer {
  const s: Summarizer = async (msgs, maxTokens) => {
    const rendered = msgs.map((m) => `${String(m["role"] ?? "?")}: ${String(m["content"] ?? "")}`).join("\n");
    const prompt = [
      {
        role: "system",
        content:
          "Summarize the conversation excerpt below concisely, PRESERVING names, decisions, " +
          "facts, preferences, and unresolved tasks. Output ONLY the summary.",
      },
      { role: "user", content: rendered },
    ];
    provider.lastStopReason = null;
    const parts: string[] = [];
    for await (const chunk of provider.chat(prompt, maxTokens)) parts.push(chunk);
    s.lastCut = provider.lastStopReason === "max_tokens";
    // A reasoning model thinks out loud here too — side calls strip like the main turn.
    return withoutThinking(parts.join(""));
  };
  s.lastCut = false;
  return s;
}

/** The ONE canonical inbound context-notice — composed at the SEAM from a
 *  strategy's artifact. A `role:user` turn in the MODEL view only — never the
 *  durable store. Carries the affordances: `recall`, and persist-to-memory. */
export function contextNotice(
  strategy: string,
  summary: string | null | undefined,
  omittedMarker: boolean,
  droppedN: number,
  stripped = 0,
  strippedBytes = 0,
): Turn {
  const lines = [
    `[context-notice] Your conversation exceeded the window and was compacted ` +
      `(strategy=${strategy}, ${droppedN} earlier turn(s) dropped from THIS view).`,
  ];
  if (summary !== null && summary !== undefined) lines.push("Summary of the dropped span:\n" + summary);
  else if (omittedMarker) lines.push("An earlier span was omitted in place.");
  if (stripped > 0) {
    lines.push(
      `${stripped} reply bod(ies) were STRIPPED to their tag line (${strippedBytes} bytes left ` +
        "your view); grep finds a line in a body held this run — after a restart, re-read it " +
        "from its origin.",
    );
  }
  lines.push(
    "The full transcript is intact in your memory store. To page dropped turns back, send " +
      'your OWN id {"type":"recall"} (optional "query", "limit"). If the dropped span holds ' +
      "durable facts (names, decisions, preferences), persist them to your memory agent now " +
      "via the send tool — the earlier turns are leaving your live view.",
  );
  return { role: "user", content: lines.join("\n") };
}

export interface ProjectArgs {
  rec: Payload;
  provider: Provider | null;
  selfId: string;
  kernel: Kernel;
  clientId: string;
  messages: Turn[];
  storeLen: number;
}

/**
 * Shape the assembled `messages` to fit the agent's budget via its
 * `context_strategy` (default `compact`), prepend the ONE canonical notice and
 * push a `context:compacted` event. Returns the projected list, OR `{error}` when
 * even the system block + the live turn will not fit (the `too_small` failsafe —
 * a failfast that ALSO pushes `context:too_small`, never a fallback) or the
 * strategy name is unknown. The budget is measured in ESTIMATE units scaled by
 * the calibration (what the endpoint reported ÷ what we estimated), and the
 * projection fires at TRIGGER × budget.
 */
export async function projectContext(a: ProjectArgs): Promise<Turn[] | { error: string }> {
  const { rec, selfId, kernel, clientId, messages } = a;
  const mem = aiMem(kernel);
  const calib = mem.calib.get(selfId) ?? 1.0;
  const b = calib > 0 ? Math.max(BUDGET_FLOOR, Math.floor(budget(rec) / calib)) : budget(rec);
  if (estimateTokens(messages) <= Math.floor(b * TRIGGER)) {
    mem.projection.set(selfId, { fired: false, trigger: TRIGGER, calibration: round3(calib) });
    return messages;
  }
  const systemBlock = messages.slice(0, 1);
  const body = messages.slice(1);
  if (body.length === 0) {
    mem.projection.set(selfId, { fired: false });
    return messages;
  }
  const sysTokens = estimateTokens(systemBlock);
  const bodyBudget = b - sysTokens;
  // The live turn AND the notice envelope are both non-negotiable — no room for
  // both is a loud failure, so the trim below can never drop the live turn.
  if (bodyBudget < estimateOne(body[body.length - 1]!) + NOTICE_ENVELOPE_RESERVE) {
    const hint =
      `the system prompt (${sysTokens} tok) leaves no room in the ${resolveContextWindow(rec)}-token ` +
      "window for even one turn; reduce the prompt or raise context_window";
    mem.projection.set(selfId, { fired: false, too_small: true });
    mem.compactionMark.delete(selfId);
    const last = body[body.length - 1]!;
    toCaller(kernel, selfId, clientId, {
      type: "context",
      source: selfId,
      ts: nowSeconds(),
      phase: "too_small",
      detail: {
        context_window: resolveContextWindow(rec),
        system_tokens: sysTokens,
        // NAME THE KILLER: which message was that big.
        last_message_tokens: estimateOne(last),
        last_message_head: String(last["content"] ?? "").slice(0, 200),
        hint,
      },
    });
    return { error: `ai_backend: context_insufficient — ${hint}` };
  }
  const stratName = typeof rec["context_strategy"] === "string" ? rec["context_strategy"] : DEFAULT_STRATEGY;
  const strat = getStrategy(stratName);
  if (strat === null) {
    return { error: `ai_backend: unknown context_strategy '${stratName}' (valid: ${strategyNames().join(", ")})` };
  }
  const ctx: ProjectionCtx = {
    budget: bodyBudget,
    recentN: recentN(rec),
    summarize: a.provider !== null ? makeSummarizer(a.provider) : null,
    selfId,
    kernel,
    clientId,
  };
  const proj = await strat(body, systemBlock, rec, ctx);
  const notice = contextNotice(
    stratName,
    proj.summary,
    proj.omittedMarker ?? false,
    body.length - proj.body.length,
    proj.stripped ?? 0,
    proj.strippedBytes ?? 0,
  );
  // Single budget authority: guard the final fit anyway — trim from the MIDDLE,
  // never the front: body[0] is the goal every strategy pins, the last entry is
  // the live turn.
  let outBody = proj.body;
  while (outBody.length > 2 && estimateTokens([...systemBlock, notice, ...outBody]) > b) {
    outBody = dropOrphanTools([outBody[0]!, ...outBody.slice(2)]);
  }
  // Last resort: [goal, live] still over budget — the goal goes, loudly (in the
  // notice's own count), never the live turn.
  while (outBody.length > 1 && estimateTokens([...systemBlock, notice, ...outBody]) > b) {
    outBody = dropOrphanTools(outBody.slice(1));
  }
  const droppedN = Math.max(0, body.length - outBody.length);
  const viewIds = outBody.filter((m) => typeof m["id"] === "number");
  mem.projection.set(selfId, {
    fired: true,
    trigger: TRIGGER,
    calibration: round3(calib),
    reported_prompt_tokens: mem.reported.get(selfId) ?? null,
    strategy: stratName,
    kept_messages: outBody.length,
    dropped_messages: droppedN,
    summarized: proj.summary !== null && proj.summary !== undefined,
    stripped: proj.stripped ?? 0,
    stripped_bytes: proj.strippedBytes ?? 0,
    folded: proj.folded ?? 0,
    // spans the fold strategy did NOT fold: the summary stayed over its budget
    fold_refused: proj.foldRefused ?? 0,
    // what the model actually sees this pass — measured from the view
    view_ids: viewIds.slice(0, 200).map((m) => m["id"] as number),
    view_ids_truncated: viewIds.length > 200,
  });
  // THE VIEW ARRAY, written by every projection: what the model sees, at what
  // resolution — so the next pass (and the next incarnation) rebuilds the same bytes.
  await saveView(selfId, kernel, viewIds.map((m) => ({ id: m["id"] as number, res: resOf(m) })), {
    budget: b,
    why: `projection: ${stratName}`,
  });
  if ((proj.folded ?? 0) > 0) {
    // A recall after a fold batch — DEBOUNCED: every RECALL_EVERY-th fold row.
    const n = (mem.foldsSince.get(selfId) ?? 0) + (proj.folded ?? 0);
    mem.foldsSince.set(selfId, n);
    if (n >= RECALL_EVERY) {
      mem.foldsSince.set(selfId, 0);
      mem.recallDue.add(selfId);
    }
  }
  // PRIVATE reaction cursor: the head index where THIS pass's new turns land.
  mem.compactionMark.set(selfId, a.storeLen);
  toCaller(kernel, selfId, clientId, {
    type: "context",
    source: selfId,
    ts: nowSeconds(),
    phase: "compacted",
    detail: {
      strategy: stratName,
      dropped_messages: droppedN,
      kept_messages: outBody.length,
      summarized: proj.summary !== null && proj.summary !== undefined,
      stripped: proj.stripped ?? 0,
      stripped_bytes: proj.strippedBytes ?? 0,
      folded: proj.folded ?? 0,
    },
  });
  return [...systemBlock, notice, ...outBody];
}

/** Record what the endpoint reported for the prompt just sent: the calibration
 *  is reported ÷ our estimate of the rendered messages (1.0 until reported). */
export function calibrate(kernel: Kernel, selfId: string, provider: Provider, sent: { role: string; content: string }[]): void {
  const reported = provider.lastPromptTokens;
  if (typeof reported === "number" && reported > 0) {
    const mem = aiMem(kernel);
    mem.calib.set(selfId, reported / Math.max(1, estimateTokens(sent)));
    mem.reported.set(selfId, reported);
  }
}

function round3(x: number): number {
  return Math.round(x * 1000) / 1000;
}
