// The TASK TABLE — one row per in-flight dispatch, and the pid that names it.
//
// ONE ID SPACE: the pid minted here is the SAME id a caller may supply on a
// batch item, the same id `ps` lists and `cancel` kills, and the same id a
// bridge frame carries. There is no second `exec_id` field anywhere.

import type { Json, Payload } from "./json.ts";
import { page } from "./paging.ts";

/** Opaque per-kernel salt so a pid cannot be predicted or replayed across
 *  kernels. Random per instance, never persisted. */
function mintSalt(): string {
  // Structural, not the DOM `Crypto` type: the pure kernel compiles without
  // the DOM lib (src/kernel/tsconfig.json), so it must not NAME a DOM type.
  const c = (globalThis as {
    crypto?: { getRandomValues?: (a: Uint8Array) => Uint8Array };
  }).crypto;
  if (c !== undefined && typeof c.getRandomValues === "function") {
    const b = new Uint8Array(8);
    c.getRandomValues(b);
    return [...b].map((x) => x.toString(16).padStart(2, "0")).join("");
  }
  // No CSPRNG in this runtime. The salt is an UNPREDICTABILITY nicety, not the
  // uniqueness mechanism — that is the monotonic sequence below — so a weaker
  // salt degrades opacity, never correctness.
  return Math.floor(Math.random() * 0xffffffff).toString(16);
}

export interface TaskRow {
  id: string;
  agent: string;
  verb: string;
  /** Set when someone cancels; the dispatch checks it at every await point. */
  killed: boolean;
  /** Ids of the tasks this one spawned — the table is a TREE, and a cancel
   *  takes the subtree with it. */
  children: Set<string>;
  parent: string | null;
  onCancel: (() => void)[];
}

export class TaskTable {
  private readonly rows = new Map<string, TaskRow>();
  private readonly salt = mintSalt();
  private seq = 0;

  /**
   * Next task id for `agentId` — `"<agent>:<hex8>"`.
   *
   * The token is OPAQUE, not a visible counter. Uniqueness comes from the
   * monotonic sequence (an id is never reused, so a stale `cancel` kills
   * nothing); the hash only hides the ordering. A VISIBLE counter invites an
   * ordering inference that is false — the sequence increments on EVERY
   * dispatch, so a caller sees gaps it did not cause. Measured 2026-08-20
   * (qwen3.8-27b): given `[reply to file_bridge_830365:10]` after issuing what
   * it thought was `:5`, the model spent a whole turn reconciling the gap
   * instead of doing the task.
   */
  mint(agentId: string): string {
    this.seq += 1;
    const n = this.seq;
    let h = 0x811c9dc5;
    for (const ch of `${this.salt}:${agentId}:${n}`) {
      h ^= ch.charCodeAt(0);
      h = Math.imul(h, 0x01000193) >>> 0;
    }
    return `${agentId}:${h.toString(16).padStart(8, "0")}`;
  }

  register(id: string, agent: string, verb: string, parent: string | null): TaskRow {
    const row: TaskRow = {
      id, agent, verb, killed: false, children: new Set(), parent, onCancel: [],
    };
    this.rows.set(id, row);
    if (parent !== null) this.rows.get(parent)?.children.add(id);
    return row;
  }

  get(id: string): TaskRow | undefined {
    return this.rows.get(id);
  }

  done(id: string): void {
    const row = this.rows.get(id);
    if (row === undefined) return;
    if (row.parent !== null) this.rows.get(row.parent)?.children.delete(id);
    this.rows.delete(id);
  }

  /** `ps` — the live table, paged, oldest first. */
  list(payload: Payload): { tasks: Json; next: string | null } {
    const all = [...this.rows.values()].map((r) => ({
      id: r.id, agent: r.agent, verb: r.verb,
    }));
    const p = page(all, payload, (t) => t.id);
    return { tasks: p.page as unknown as Json, next: p.next };
  }

  /**
   * Kill `id` AND ITS SUBTREE — a cancelled parent whose children kept running
   * would leave orphan work nobody can reach.
   *
   * Returns the ids actually killed. An unknown or already-finished id kills
   * nothing and says so, rather than erroring: `cancel` is idempotent because
   * the caller usually cannot know whether the race was already lost.
   */
  cancel(id: string): string[] {
    const row = this.rows.get(id);
    if (row === undefined) return [];
    const killed: string[] = [];
    const walk = (r: TaskRow): void => {
      if (r.killed) return;
      r.killed = true;
      killed.push(r.id);
      for (const cid of [...r.children]) {
        const c = this.rows.get(cid);
        if (c !== undefined) walk(c);
      }
      // Hooks run BEFORE the abort lands, so a bundle can release what it holds.
      for (const fn of r.onCancel) {
        try {
          fn();
        } catch {
          // one bundle's failing hook must not strand the rest of the subtree
        }
      }
    };
    walk(row);
    return killed;
  }

  size(): number {
    return this.rows.size;
  }
}
