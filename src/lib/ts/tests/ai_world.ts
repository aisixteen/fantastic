// Shared fixtures for the ai tests — the ts counterpart of python
// ai_core/tests/_fake.py + test_folds.py's row helpers. A real kernel with a
// real `ram_store` (`mem`) and a bare agent `ai` whose record names it, so every
// store call goes through the gate like it does in production.
//
// Fixture rows are named by TAGS ("user:goal", "ai:a0r") and each tag becomes an
// int the KERNEL minted, on first sight — one counter, like production.

import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import type { Json, Payload } from "../src/kernel/json.ts";
import { registerRamStore } from "../src/bundles/memory/ram_store.ts";
import type { ProjectionCtx, Summarizer, Turn } from "../src/bundles/ai/context.ts";
import { makeFold } from "../src/bundles/ai/folds.ts";
import { saveHistory } from "../src/bundles/ai/memory.ts";
import { replyTag } from "../src/bundles/ai/replies.ts";
import { registerAIBackend, type AIRecord } from "../src/bundles/ai/ai_backend.ts";
import { renderCall } from "../src/bundles/ai/tool_parse.ts";

export const BIG = "x".repeat(3000);

export class World {
  readonly k: Kernel;
  private readonly tags = new Map<string, number>();

  constructor(meta: Payload = {}) {
    const k = new Kernel();
    registerRamStore(k);
    k.setRoot(new Agent({ id: "kernel_state", meta: { guid: "g1" } }));
    k.register(
      new Agent({
        id: "mem",
        parentId: "kernel_state",
        handlerModule: "ram_store.ts",
        meta: { operator: true, admins: ["kernel"], callable_by: { ai: "*" } },
      }),
    );
    k.register(
      new Agent({
        id: "ai",
        parentId: "kernel_state",
        meta: { memory_store_id: "mem", can_call: { mem: "*" }, context_window: 12000, output_reserve: 0, ...meta },
      }),
    );
    this.k = k;
  }

  /** The int a tag stands for (minted on first sight); ints pass through. */
  id(tag: string | number | null | undefined): number | null {
    if (tag === null || tag === undefined) return null;
    if (typeof tag === "number") return tag;
    let n = this.tags.get(tag);
    if (n === undefined) this.tags.set(tag, (n = this.k.mintId()));
    return n;
  }

  row(role: string, content: string, tag: string | number | null, parent: string | number | null = null, extra: Payload = {}): Turn {
    const r: Turn = { role, content, parent: this.id(parent), ...extra };
    const rid = this.id(tag);
    if (rid !== null) r["id"] = rid;
    return r;
  }

  /** A delivered reply row, exactly as routing writes one: its tag line
   *  `[reply to <pid> · <target> <verb> · "<title>"? · ↑<message>]`, then the body.
   *  The `↑` names the assistant row that made the call (the row's parent). */
  reply(
    tag: string | null,
    parent: string | null,
    body: string,
    verb = "read",
    target = "notes",
    pid?: number,
    title: string | null = null,
  ): Turn {
    const head = replyTag(pid ?? this.k.mintId(), target, verb, title, this.id(parent), 1, 1, "");
    return this.row("user", `${head}\n${body}`, tag, parent);
  }

  /** root user msg → (assistant call → reply) × n, optionally ending on an
   *  unanswered call. */
  chain(tag: string, nCalls: number, big = true, openEnd = false): Turn[] {
    const rows = [this.row("user", `task ${tag}`, `user:${tag}`)];
    let prev = `user:${tag}`;
    for (let i = 0; i < nCalls; i++) {
      const a = `ai:${tag}${i}`;
      rows.push(this.row("assistant", `read ${tag}${i} ${callText("notes", { type: "read" }, `read ${tag}${i}`)}`, a, prev));
      const r = `ai:${tag}${i}r`;
      rows.push(this.reply(r, a, big ? BIG : "ok", "read", "notes", undefined, `read ${tag}${i}`));
      prev = r;
    }
    if (openEnd) rows.push(this.row("assistant", `one more ${callText("notes", { type: "read" })}`, `ai:${tag}open`, prev));
    return rows;
  }

  fold(refs: (string | number | null)[], title: string, body: string, degraded = false, opts: { lvl?: number; parent?: Json } = {}): Turn {
    return makeFold("ai", refs.map((r) => this.id(r)), title, body, degraded, {
      id: this.k.mintId(),
      lvl: opts.lvl ?? 1,
      parent: opts.parent ?? null,
    });
  }

  async seed(rows: Turn[]): Promise<void> {
    await saveHistory("ai", this.k, rows);
  }

  /** Put a projection body's STORED rows (those with an id — not the live turn)
   *  in the store: in production every body row came from it, and the store
   *  enforces that a fold's refs name rows it holds. */
  async persistBody(body: Turn[]): Promise<Turn[]> {
    await saveHistory("ai", this.k, body.filter((r) => typeof r["id"] === "number"));
    return body;
  }

  /** Every row the store holds for `ai`, in id order (the operator is an admin). */
  async stored(): Promise<Turn[]> {
    const r = (await this.k.send([{ target: "mem", payload: { type: "read", owner: "ai" } }]))[0]!.result as Payload;
    return r["rows"] as Turn[];
  }

  async folds(): Promise<Turn[]> {
    return (await this.stored()).filter((r) => r["kind"] === "fold");
  }

  ctx(budget: number, summarize: Summarizer | null = fakeSummary, clientId: string | null = "c", recentN = 4): ProjectionCtx {
    return { budget, recentN, summarize, selfId: "ai", kernel: this.k, clientId };
  }
}

/** A `send` exactly as a model writes it — the one tag grammar. */
export function callText(target: string, payload: Payload, title?: string): string {
  const args: Payload = { target_id: target, payload };
  if (title !== undefined) args["title"] = title;
  return renderCall("send", args);
}

export const fakeSummary: Summarizer = async () => "TITLE of the fold\n\nBody of the fold: what happened.";

/** A deterministic interleave of several chains (a seeded LCG). */
export function interleave(...chains: Turn[][]): Turn[] {
  const out: Turn[] = [];
  const ptr = chains.map(() => 0);
  let seed = 7;
  const rand = (n: number): number => {
    seed = (seed * 1103515245 + 12345) % 2147483648;
    return seed % n;
  };
  for (;;) {
    const live = chains.map((c, i) => (ptr[i]! < c.length ? i : -1)).filter((i) => i >= 0);
    if (live.length === 0) return out;
    const i = live[rand(live.length)]!;
    out.push(chains[i]![ptr[i]!]!);
    ptr[i] = ptr[i]! + 1;
  }
}

export function turn(role: string, content: string): Turn {
  return { role, content };
}

export function body(n: number, size = 200): Turn[] {
  return Array.from({ length: n }, (_, i) => turn(i % 2 === 0 ? "user" : "assistant", `turn${i}-` + "x".repeat(size)));
}

// ── the turn loop: a scripted mind, and waiting for a chain to settle ────────

export const DONE = '<call name="done"></call>';

/** A scripted mind (python `test_done_and_nudge._Script`): yields the queued
 *  replies in order, records every incoming message. Only TURNS count — a pass
 *  that opens with the system block; `context_recall` / fold summaries are side
 *  calls and are answered with `sideAnswer`. Out of script it says `done`, so an
 *  exhausted script ends its chain instead of being nudged forever. */
export class Mind {
  readonly turns: string[];
  calls = 0;
  readonly incoming: string[] = [];
  readonly seen: { role: string; content: string }[][] = [];
  lastPromptTokens: number | null = null;
  lastStopReason: string | null = null;
  /** ms the FIRST turn waits before answering (so a second send queues behind it) */
  delay: number;
  /** what the endpoint reports as the prompt size, every pass */
  reported: number | undefined = undefined;
  sideAnswer = "TITLE\n\nsummary";
  midTurn: (() => Promise<void>) | undefined = undefined;

  constructor(turns: string[] = [], delay = 0) {
    this.turns = [...turns];
    this.delay = delay;
  }

  async *chat(messages: { role: string; content: string }[], _maxTokens?: number): AsyncIterable<string> {
    if (messages[0]?.role !== "system") {
      yield this.sideAnswer;
      return;
    }
    this.calls += 1;
    this.seen.push(messages.map((m) => ({ ...m })));
    this.incoming.push(messages[messages.length - 1]!.content);
    if (this.calls === 1 && this.delay > 0) await new Promise((r) => setTimeout(r, this.delay));
    if (this.midTurn !== undefined) {
      const f = this.midTurn;
      this.midTurn = undefined;
      await f();
    }
    if (this.reported !== undefined) this.lastPromptTokens = this.reported;
    yield this.turns.length > 0 ? (this.turns.shift() as string) : DONE;
  }
}

/** Wait until nothing is in flight in `k` — no task row, observed three polls in
 *  a row (a delivery is spawned after its batch settles, so one empty glimpse is
 *  not enough). Bounded: a chain that never settles fails the test, loudly. */
export async function settle(k: Kernel, timeoutMs = 5000): Promise<void> {
  const t0 = Date.now();
  let quiet = 0;
  while (quiet < 3) {
    if (Date.now() - t0 > timeoutMs) {
      throw new Error(`the chain never settled: tasks ${JSON.stringify(k.tasks.list({}).tasks)}`);
    }
    await new Promise((r) => setTimeout(r, 5));
    quiet = k.tasks.ids().length === 0 ? quiet + 1 : 0;
  }
}

/** Poll `fn` until it holds; bounded. */
export async function until(fn: () => boolean | Promise<boolean>, timeoutMs = 5000): Promise<void> {
  const t0 = Date.now();
  while (!(await fn())) {
    if (Date.now() - t0 > timeoutMs) throw new Error("condition never held");
    await new Promise((r) => setTimeout(r, 5));
  }
}

/** A kernel with a REAL ai backend `ai` (ts `ai_backend`) whose turns the given
 *  mind answers, its `mem` ram_store, and a `tool` agent that answers any verb
 *  (`size` → a body that long; `error` → an error reply). */
export function backendWorld(mind: Mind, meta: Payload = {}, opts: { sendTimeout?: number } = {}): Kernel {
  const k = new Kernel();
  registerRamStore(k);
  registerAIBackend(k, "ai.ts", (id) => k.get(id)!.meta as AIRecord, {
    makeProvider: () => mind,
    ...(opts.sendTimeout !== undefined ? { sendTimeout: opts.sendTimeout } : {}),
  });
  k.registerBundle("tool.tools", (_id, p) => {
    if (p["error"] !== undefined) return { error: p["error"] } as Json;
    return { text: "t".repeat(Number(p["size"] ?? 10)) };
  });
  k.setRoot(new Agent({ id: "kernel_state", meta: { guid: "g1" } }));
  k.register(new Agent({ id: "mem", parentId: "kernel_state", handlerModule: "ram_store.ts", meta: { operator: true, admins: ["kernel"], callable_by: { ai: "*" } } }));
  k.register(new Agent({ id: "tool", parentId: "kernel_state", handlerModule: "tool.tools", meta: { callable_by: { ai: "*" } } }));
  k.register(
    new Agent({
      id: "ai",
      parentId: "kernel_state",
      handlerModule: "ai.ts",
      meta: { memory_store_id: "mem", can_call: { mem: "*", tool: "*" }, context_window: 32768, output_reserve: 0, ...meta },
    }),
  );
  return k;
}
