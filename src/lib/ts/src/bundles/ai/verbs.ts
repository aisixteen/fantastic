import type { Kernel } from "../../kernel/kernel.ts";
import type { Json, Payload } from "../../kernel/json.ts";
import { BAD_ARG, NOT_FOUND, refusal } from "../../kernel/refusal.ts";
import {
  BUDGET_FLOOR,
  budget,
  estimateOne,
  inlineCap,
  outputReserve,
  resolveContextWindow,
} from "./context.ts";
import type { Turn } from "./context.ts";
import { expandIds, render as foldRender } from "./folds.ts";
import { applyRes, isReply, replyMeta, resOf } from "./replies.ts";
import {
  aiMem,
  asId,
  head,
  loadHistory,
  saveHistory,
  saveView,
  storeCall,
} from "./memory.ts";
import type { Row } from "./memory.ts";
import { DEFAULT_STRATEGY } from "./strategies/index.ts";
import { NOTICE_ENVELOPE_RESERVE } from "./strategies/base.ts";
import { LOW, stripped as stripState } from "./strategies/strip.ts";
import { TRIGGER } from "./seam.ts";
import type { Provider } from "./seam.ts";
import { withoutThinking } from "./think.ts";
import { extractToolCalls } from "./tool_parse.ts";

// The AI agent's context verbs — port of python `ai_core/core.py` `_history` /
// `_recall` / `_grep` / `_context_recall` / `_context_clear` / `_context_status`
// / `_derive_reaction`. Every verb an agent calls on itself answers UNDER the
// inline cap (`fitReply`), or its own answer would come back digested.

/** THE CAP INVARIANT: keep items from the front while the serialized reply fits
 *  (with headroom for the envelope the verb wraps around them). Returns
 *  [kept, truncated]. */
export function fitReply<T extends Json>(items: T[], capTokens: number, limit: number | null = null): [T[], boolean] {
  const cap = Math.floor(capTokens * 0.8);
  const kept: T[] = [];
  for (const it of items) {
    if (limit !== null && kept.length >= limit) return [kept, true];
    const trial = [...kept, it];
    if (estimateOne({ role: "user", content: JSON.stringify(trial) }) > cap) return [kept, true];
    kept.push(it);
  }
  return [kept, false];
}

function recOf(kernel: Kernel, id: string): Payload {
  return kernel.get(id)?.meta ?? {};
}

function digest(r: Row): Payload {
  const c = String(r["content"] ?? "");
  return {
    id: r.id ?? null,
    parent: (r["parent"] as Json | undefined) ?? null,
    kind: (r["kind"] as Json | undefined) ?? (r["role"] as Json | undefined) ?? null,
    role: (r["role"] as Json | undefined) ?? null,
    head: c.slice(0, 120).replace(/\n/g, " "),
    bytes: c.length,
  };
}

/** `history` — the conversation at DIGEST resolution ({id, parent, kind, role,
 *  head, bytes} per row), oldest first, paged under the inline cap. With `fold`,
 *  that fold's (or clear's) title + summary and its members, one level down. */
export async function history(id: string, payload: Payload, kernel: Kernel): Promise<Json> {
  let rows: Row[];
  let foldHead: Payload | null = null;
  if (payload["fold"] !== undefined && payload["fold"] !== null) {
    const foldId = asId(payload["fold"]);
    if (foldId === null) return refusal(BAD_ARG, "history: `fold` must be a fold id (int)", { extra: { arg: "fold" } });
    const everything = await loadHistory(id, kernel, { behind: true });
    const byId = new Map(everything.filter((r) => r.id !== undefined).map((r) => [r.id, r]));
    const f = byId.get(foldId);
    if (f === undefined || (f["kind"] !== "fold" && f["kind"] !== "clear")) {
      return refusal(NOT_FOUND, `history: ${foldId} is not a fold in your conversation`, { extra: { arg: "fold" } });
    }
    const refs = (f["refs"] as number[] | undefined) ?? [];
    rows = refs.filter((m) => byId.has(m)).map((m) => byId.get(m)!);
    foldHead = {
      id: foldId,
      kind: f["kind"] ?? null,
      title: (f["title"] as Json | undefined) ?? null,
      summary: (f["body"] as Json | undefined) ?? "",
      members: refs.length,
    };
  } else {
    rows = await loadHistory(id, kernel);
  }
  let digests = rows.filter((r) => r.id !== undefined).map(digest);
  const limit = Number.parseInt(String(payload["limit"] ?? "0"), 10) || 0;
  const after = asId(payload["after"]);
  if (after !== null) {
    const i = digests.findIndex((d) => d["id"] === after);
    digests = i >= 0 ? digests.slice(i + 1) : [];
  }
  const [kept, truncated] = fitReply(digests, inlineCap(recOf(kernel, id)), limit > 0 ? limit : null);
  const out: Payload = {
    rows: kept,
    next: truncated && kept.length > 0 ? (kept[kept.length - 1]!["id"] ?? null) : null,
    total: rows.length,
  };
  if (foldHead !== null) out["fold"] = foldHead;
  return out;
}

/** `recall` — pages turns back from the DURABLE store (the whole conversation,
 *  never trimmed): a substring `query`, a `limit` (default 20, max 100), `before`
 *  a store index. A page fits INLINE: trimmed from the OLDEST end when it would
 *  not, with the index to continue from. An empty page says why. */
export async function recall(id: string, payload: Payload, kernel: Kernel): Promise<Json> {
  const full = await loadHistory(id, kernel, { behind: true });
  const q = String(payload["query"] ?? "").toLowerCase().trim();
  let limit = Number.parseInt(String(payload["limit"] ?? "20"), 10);
  if (Number.isNaN(limit)) limit = 20;
  limit = Math.max(1, Math.min(limit, 100));
  let indexed: [number, Row][] = full.map((m, i) => [i, m]);
  const before = payload["before"];
  if (typeof before === "number" && Number.isInteger(before)) indexed = indexed.filter(([i]) => i < before);
  if (q !== "") indexed = indexed.filter(([, m]) => JSON.stringify(m).toLowerCase().includes(q));
  const truncated = indexed.length > limit;
  let pageRows = indexed.slice(-limit);
  const cap = inlineCap(recOf(kernel, id));
  const render = (m: Row): string => (typeof m["content"] === "string" ? m["content"] : JSON.stringify(m["content"] ?? null));
  const asMessages = (pg: [number, Row][]): Json[] =>
    pg.map(([i, m]) => ({ index: i, role: (m["role"] as Json | undefined) ?? null, content: render(m) }));
  // A PAGE MUST FIT THE WINDOW IT IS GOING INTO — measured on the whole reply.
  const serialized = (pg: [number, Row][]): number =>
    estimateOne({
      role: "user",
      content: JSON.stringify({
        messages: asMessages(pg),
        total: indexed.length,
        truncated: true,
        note: `page trimmed by 999 older turn(s) to fit your window (cap ${cap} tok); pass before=999999 to page further back.`,
      }),
    });
  let trimmed = 0;
  while (pageRows.length > 1 && serialized(pageRows) > Math.floor(cap * 0.8)) {
    pageRows = pageRows.slice(1);
    trimmed += 1;
  }
  const reply: Payload = { messages: asMessages(pageRows), total: indexed.length, truncated: truncated || trimmed > 0 };
  if (trimmed > 0) {
    reply["note"] =
      `page trimmed by ${trimmed} older turn(s) to fit your window (cap ${cap} tok); ` +
      `pass before=${pageRows[0]![0]} to page further back.`;
  }
  if (pageRows.length === 0) {
    const proj = aiMem(kernel).projection.get(id);
    if (q !== "" && full.length > 0) reply["note"] = `query '${q}' matched none of the ${full.length} stored turns.`;
    else if (proj === undefined || proj["fired"] !== true) {
      reply["note"] =
        "nothing has been compacted away — recall pages back turns that compaction DROPPED, and none " +
        "have been. If you can see this reply, your live context already holds the whole conversation.";
    } else reply["note"] = "no stored turns in this page.";
  }
  return reply;
}

/**
 * `grep` — the LINE that mentions something ANYWHERE in memory: reply bodies held
 * this run FIRST (a body never read whole is what grep is for), then every row
 * under the agent's key (behind every clear, inside every fold — the store
 * searches), then every scope another agent SHARED with it. `ids` scopes the
 * search (a fold id descends to its members, and a member reply's pid reaches its
 * held body). Identical lines are reported once. Fits the inline cap.
 */
export async function grep(id: string, payload: Payload, kernel: Kernel): Promise<Json> {
  const q = String(payload["query"] ?? "").trim().toLowerCase();
  if (q === "") return refusal(BAD_ARG, "grep: `query` is required", { extra: { arg: "query" } });
  let limit = Number.parseInt(String(payload["limit"] ?? "100"), 10);
  limit = Number.isNaN(limit) ? 100 : Math.max(1, Math.min(limit, 100));
  let width = Number.parseInt(String(payload["width"] ?? "200"), 10);
  width = Number.isNaN(width) ? 200 : Math.max(20, Math.min(width, 20_000));
  const rawIds = payload["ids"];
  let ids: number[] | null = null;
  if (rawIds !== undefined && rawIds !== null) {
    const parsed = Array.isArray(rawIds) ? rawIds.map((x) => asId(x)) : null;
    if (parsed === null || parsed.some((x) => x === null)) {
      return refusal(BAD_ARG, "grep: `ids` must be a list of message ids / pids (ints)", { extra: { arg: "ids" } });
    }
    ids = parsed as number[];
  }
  const matches: Payload[] = [];
  let total = 0;
  const seenLines = new Set<string>();
  const take = (rid: Json, n: number, line: string): void => {
    // IDENTICAL LINES ONCE: the first occurrence keeps its address.
    const key = line.split(/\s+/).filter(Boolean).join(" ");
    if (seenLines.has(key)) return;
    seenLines.add(key);
    total += 1;
    if (matches.length < limit) matches.push({ id: rid, line: n, text: line.trim().slice(0, width) });
  };
  const bodies = aiMem(kernel).bodies.get(id) ?? new Map<number, string>();
  let inReach: Set<number> | null = null;
  if (ids !== null) {
    // A fold id is a DESCENT: its members are reply rows, and their bodies hang off
    // the pids those rows answered. An id that is itself a pid counts.
    const rows = await loadHistory(id, kernel, { behind: true });
    const scope = new Set(expandIds(rows, ids) as number[]);
    inReach = new Set([...ids, ...scope]);
    for (const m of rows) {
      if (scope.has(m.id as number) && isReply(m)) {
        const pid = replyMeta(m)?.pid;
        if (typeof pid === "number") inReach.add(pid);
      }
    }
  }
  for (const [pid, text] of bodies) {
    if (inReach !== null && !inReach.has(pid)) continue;
    text.split(/\r?\n/).forEach((line, n) => {
      if (line.toLowerCase().includes(q)) take(pid, n + 1, line);
    });
  }
  // YOUR ROWS — the WHOLE conversation, searched by the store itself.
  const req: Payload = { type: "search", query: q, limit: 1000, width };
  if (ids !== null) req["ids"] = ids;
  const res = await storeCall(id, kernel, req);
  for (const m of (res["matches"] as Payload[] | undefined) ?? []) {
    take(m["id"] ?? null, Number(m["line"] ?? 0), String(m["text"] ?? ""));
  }
  // ROWS SHARED WITH YOU — every fork row's grant, searched under that grant.
  if (ids === null) {
    for (const r of await head(id, kernel)) {
      if (r["kind"] === "fork" && r["grant"] !== undefined && r["grant"] !== null) {
        const shared = await storeCall(id, kernel, { type: "search", query: q, grant: r["grant"], limit: 1000, width });
        for (const m of (shared["matches"] as Payload[] | undefined) ?? []) {
          take(`${String(r["owner"])}:${String(m["id"])}`, Number(m["line"] ?? 0), String(m["text"] ?? ""));
        }
      }
    }
  }
  const [kept, cut] = fitReply(matches, inlineCap(recOf(kernel, id)), limit);
  const more = cut || total > kept.length;
  const out: Payload = { matches: kept, total, truncated: more };
  if (more && kept.length > 0) {
    const last = kept[kept.length - 1]!;
    out["next"] = { id: last["id"] ?? null, line: last["line"] ?? null };
  }
  return out;
}

function firstJson(text: string): Payload | null {
  const i = text.indexOf("{");
  if (i < 0) return null;
  let depth = 0;
  for (let j = i; j < text.length; j++) {
    if (text[j] === "{") depth++;
    else if (text[j] === "}") depth--;
    if (depth === 0) {
      try {
        const out = JSON.parse(text.slice(i, j + 1)) as unknown;
        return out !== null && typeof out === "object" && !Array.isArray(out) ? (out as Payload) : null;
      } catch {
        return null;
      }
    }
  }
  return null;
}

async function compose(provider: Provider, prompt: string): Promise<string> {
  const parts: string[] = [];
  for await (const chunk of provider.chat([{ role: "user", content: prompt }])) parts.push(chunk);
  return withoutThinking(parts.join(""));
}

export const RECALL_PROMPT = (budgetN: number, used: number, done: string, lines: string): string =>
  `You are rebuilding your own working context. Below is the HEAD of your
conversation as one line per row: id · kind · title · size · ↑parent. Fold rows stand for
finished stretches (their members are one descent away). Everything is currently at
DIGEST resolution; your budget for the body is ${budgetN} tokens and the digests use
${used}. Replies ALREADY IN THIS HISTORY (their calls need not be repeated; the task itself may be unfinished):
${done}
Choose which rows you want IN FULL next turn (expansions cost their size) and say in one
sentence why. Reply JSON only:
{"expand": [<id>, ...], "why": "<one sentence>"}

${lines}`;

/**
 * `context_recall` — rebuild this agent's VIEW from its store: fit the head to
 * the budget at digest resolution (the goal turn always in, then newest first),
 * ask the model which rows it wants in full (unless `dry`, or no provider), persist
 * the id list, log the decision as a `recall` row naming the LIVE task, and drop
 * RAM scratch. Runs on the first turn after boot, after a fold batch, or on request.
 */
export async function contextRecall(
  id: string,
  payload: Payload,
  kernel: Kernel,
  provider: Provider | null,
  systemContent: string,
): Promise<Json> {
  const rec = recOf(kernel, id);
  const rows = await loadHistory(id, kernel);
  if (rows.length === 0) return { kept: 0, expanded: 0, dropped: 0, why: "empty store", degraded: false };
  const sysTokens = estimateOne({ role: "system", content: systemContent });
  const bodyBudget = Math.max(BUDGET_FLOOR, budget(rec) - sysTokens - NOTICE_ENVELOPE_RESERVE);
  const target = Math.floor(bodyBudget * LOW);
  const view = foldRender(rows).filter((r) => typeof r["id"] === "number");
  if (view.length === 0) return { kept: 0, expanded: 0, dropped: 0, why: "no addressable rows", degraded: false };
  const dig = (r: Turn): Turn => (resOf(r) === "full" && isReply(r) ? applyRes(r, "stub") : r);
  const cost = (r: Turn): number => estimateOne({ role: r["role"] ?? "user", content: r["content"] ?? "" });
  // 1. MECHANICAL FIT — never fails. The goal turn always in; then newest first.
  const picked = new Set([0]);
  let used = cost(dig(view[0]!));
  for (let i = view.length - 1; i > 0; i--) {
    const c = cost(dig(view[i]!));
    if (used + c > target) break;
    picked.add(i);
    used += c;
  }
  const headIdx = [...picked].sort((a, b) => a - b);
  // 2. ONE MODEL CALL — the model chooses expansions; a bad answer changes nothing.
  let why = "mechanical fit";
  let degraded = true;
  const expand = new Set<number>();
  if (provider !== null && payload["dry"] !== true) {
    const lines = headIdx.map((i) => {
      const r = view[i]!;
      const c = String(r["content"] ?? "");
      const m = /· "([^"]{1,140})"/.exec(c);
      const title = (r["title"] as string | undefined) ?? (m !== null ? m[1] : c.slice(0, 80).replace(/\n/g, " "));
      return `${String(r["id"])} · ${String(r["kind"] ?? r["role"])} · "${title}" · ${c.length}B · ↑${String(r["parent"] ?? "-")}`;
    });
    // what the store already answers: every reply row names the call it closed
    const done: string[] = [];
    for (const r of view) {
      const meta = isReply(r) ? replyMeta(r) : null;
      if (meta === null) continue;
      const line = `${meta.target} ${meta.verb}`;
      if (!done.includes(line)) done.push(line);
    }
    let ans: Payload | null = null;
    try {
      ans = firstJson(await compose(provider, RECALL_PROMPT(bodyBudget, used, done.slice(-40).join("\n") || "(nothing yet)", lines.join("\n"))));
    } catch {
      ans = null;
    }
    if (ans !== null) {
      const headIds = new Set(headIdx.map((i) => view[i]!["id"] as number));
      const rawWant = ans["expand"];
      const want = (Array.isArray(rawWant) ? rawWant : rawWant ? [rawWant] : [])
        .map((v) => asId(v))
        .filter((x): x is number => x !== null && headIds.has(x));
      why = String(ans["why"] ?? "").slice(0, 300) || "chosen";
      degraded = false;
      // charge each expansion at full size, newest first; drop what does not fit
      let spent = used;
      for (const x of [...want].reverse()) {
        const r = view.find((v) => v["id"] === x)!;
        const extra = cost(r) - cost(dig(r));
        if (spent + extra <= bodyBudget) {
          expand.add(x);
          spent += extra;
        }
      }
    }
  }
  const ids = headIdx.map((i) => {
    const r = view[i]!;
    const rid = r["id"] as number;
    const res = r["kind"] === "fold" ? "fold" : expand.has(rid) || !isReply(r) ? "full" : "stub";
    return { id: rid, res };
  });
  await saveView(id, kernel, ids, { budget: bodyBudget, why });
  // THE LIVE TASK IS NAMED — the newest root over the STORE (not the folded
  // render: a root a fold collapsed is still the live one if nothing newer came).
  const live = [...rows].reverse().find((r) =>
    r["role"] === "user" &&
    !r["parent"] &&
    (r["kind"] === undefined || r["kind"] === null || r["kind"] === "" || r["kind"] === "user") &&
    !isReply(r)
  );
  const liveTxt = live !== undefined
    ? ` · live task: ${String(live.id)} "` + String(live["content"] ?? "").split(/\s+/).filter(Boolean).join(" ").slice(0, 100) + '"'
    : "";
  const rid = kernel.mintId();
  await saveHistory(id, kernel, [
    {
      role: "user",
      id: rid,
      parent: null,
      kind: "recall",
      title: why,
      content: `[recall ${rid} · kept ${ids.length} rows, ${expand.size} in full] ${why}${liveTxt}`,
    },
  ]);
  // RAM is scratch, reconstructible by definition; the store is never cleared.
  const mem = aiMem(kernel);
  mem.projection.delete(id);
  mem.compactionMark.delete(id);
  stripState.delete(id);
  return { kept: ids.length, expanded: expand.size, dropped: view.length - ids.length, budget: bodyBudget, why, degraded };
}

/** `context_clear` — a NEW THREAD on the same store: a CLEAR-FOLD whose members
 *  are every row since the previous clear (no summary); the view begins at its
 *  `[cleared …]` line. Nothing is deleted: grep, recall and `history {fold}` reach
 *  every member. Empties the persisted view and drops RAM scratch. */
export async function contextClear(id: string, payload: Payload, kernel: Kernel): Promise<Json> {
  const rec = recOf(kernel, id);
  const members = (await head(id, kernel))
    .filter((r) => r["kind"] !== "view" && typeof r.id === "number")
    .map((r) => r.id as number);
  const why = String(payload["why"] ?? "").split(/\s+/).filter(Boolean).join(" ").slice(0, 200);
  const rid = kernel.mintId();
  await saveHistory(id, kernel, [
    {
      role: "user",
      id: rid,
      parent: null,
      kind: "clear",
      refs: members,
      title: why || "cleared",
      content:
        `[cleared ${rid} · ${members.length} rows]` +
        (why ? ` ${why}` : "") +
        " — a new thread; the rows before it are this clear's members, " +
        `reachable by id (grep, recall, history fold=${rid}).`,
    },
  ]);
  // The persisted view names rows behind the clear — empty it.
  await saveView(id, kernel, [], { budget: budget(rec), why: "cleared" });
  const mem = aiMem(kernel);
  mem.projection.delete(id);
  mem.compactionMark.delete(id);
  stripState.delete(id);
  mem.foldsSince.delete(id);
  mem.recallDue.delete(id);
  mem.viewSource.delete(id);
  return { cleared: rid, rows: members.length };
}

/** Read-model over the durable transcript: AFTER the last compaction, did the
 *  model react — a `recall` to its OWN id, or a memory write (`write`/`patch`/
 *  `append`) to any agent? The reaction IS the ack. null before any compaction. */
export async function deriveReaction(id: string, kernel: Kernel): Promise<Payload | null> {
  const mem = aiMem(kernel);
  const proj = mem.projection.get(id);
  const mark = mem.compactionMark.get(id);
  if (proj === undefined || proj["fired"] !== true || mark === undefined) return null;
  const store = await loadHistory(id, kernel);
  let recalled = false;
  let persisted = false;
  let recallCount = 0;
  for (const m of store.slice(mark)) {
    if (m["role"] !== "assistant") continue;
    for (const call of extractToolCalls(String(m["content"] ?? ""))) {
      const args = (call.arguments ?? {}) as Payload;
      const p = args["payload"];
      const ptype = p !== null && typeof p === "object" && !Array.isArray(p) ? p["type"] : null;
      if (args["target_id"] === id && ptype === "recall") {
        recalled = true;
        recallCount += 1;
      } else if (ptype === "write" || ptype === "patch" || ptype === "append") {
        persisted = true;
      }
    }
  }
  return { recalled, persisted, recall_count: recallCount };
}

/** `context_status` — the budget posture, the last projection and the model's
 *  derived reaction to it. */
export async function contextStatus(id: string, kernel: Kernel): Promise<Json> {
  const rec = recOf(kernel, id);
  const mem = aiMem(kernel);
  return {
    context_window: resolveContextWindow(rec),
    output_reserve: outputReserve(rec),
    budget: budget(rec),
    trigger: TRIGGER,
    calibration: Math.round((mem.calib.get(id) ?? 1.0) * 1000) / 1000,
    reported_prompt_tokens: mem.reported.get(id) ?? null,
    strategy: typeof rec["context_strategy"] === "string" ? rec["context_strategy"] : DEFAULT_STRATEGY,
    last_projection: mem.projection.get(id) ?? null,
    last_reaction: await deriveReaction(id, kernel),
    view_source: mem.viewSource.get(id) ?? null,
  };
}
