// Shared fixtures for the ai compaction tests — the ts counterpart of python
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
import { wrap } from "../src/bundles/ai/replies.ts";

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

  /** A reply row: the model reads `<tool_response>body</tool_response>`; the
   *  row keeps the call it answered. */
  reply(tag: string | null, parent: string | null, body: string, verb = "read", target = "notes", pid?: number): Turn {
    return this.row("user", wrap(body), tag, parent, {
      reply: { pid: pid ?? this.k.mintId(), target, verb },
    });
  }

  /** root user msg → (assistant call → reply) × n, optionally ending on an
   *  unanswered call. */
  chain(tag: string, nCalls: number, big = true, openEnd = false): Turn[] {
    const rows = [this.row("user", `task ${tag}`, `user:${tag}`)];
    let prev = `user:${tag}`;
    for (let i = 0; i < nCalls; i++) {
      const a = `ai:${tag}${i}`;
      rows.push(this.row("assistant", `read ${tag}${i} ${callText("notes", { type: "read" })}`, a, prev));
      const r = `ai:${tag}${i}r`;
      rows.push(this.reply(r, a, big ? BIG : "ok"));
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

export function callText(target: string, payload: Payload): string {
  return `<tool_call>${JSON.stringify({ name: "send", arguments: { target_id: target, payload } })}</tool_call>`;
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
