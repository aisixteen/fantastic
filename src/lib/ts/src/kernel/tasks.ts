// The TASK TABLE — one row per in-flight dispatch.
//
// ONE ID SPACE: a task id is the MESSAGE id of the call that started it — an int
// from the kernel's one counter (`Kernel.mintId`), the same id the call row, the
// settle row, `ps`, `cancel` and the outcome's `pid` carry. The table never
// mints; the kernel is the only allocator.

import type { Json, Payload } from "./json.ts";
import { page } from "./paging.ts";
import { nowSeconds } from "./timers.ts";

export interface TaskRow {
  id: number;
  agent: string;
  verb: string;
  /** Set when someone cancels; the dispatch settles `cancelled` at once. */
  killed: boolean;
  /** Ids of the tasks this one spawned — the table is a TREE, and a cancel
   *  takes the subtree with it. */
  children: Set<number>;
  parent: number | null;
  /** seconds since the epoch, when the dispatch started */
  startedAt: number;
  onCancel: (() => void)[];
}

export class TaskTable {
  private readonly rows = new Map<number, TaskRow>();

  register(id: number, agent: string, verb: string, parent: number | null): TaskRow {
    const row: TaskRow = {
      id, agent, verb, killed: false, children: new Set(), parent, startedAt: nowSeconds(), onCancel: [],
    };
    this.rows.set(id, row);
    if (parent !== null) this.rows.get(parent)?.children.add(id);
    return row;
  }

  get(id: number): TaskRow | undefined {
    return this.rows.get(id);
  }

  has(id: number): boolean {
    return this.rows.has(id);
  }

  ids(): number[] {
    return [...this.rows.keys()];
  }

  done(id: number): void {
    const row = this.rows.get(id);
    if (row === undefined) return;
    if (row.parent !== null) this.rows.get(row.parent)?.children.delete(id);
    this.rows.delete(id);
  }

  /** `ps` — the live table `{id, agent, parent, verb, started_at}`, paged, oldest
   *  first; `parent` chains the rows into the send tree. */
  list(payload: Payload): { tasks: Json; next: string | null } {
    const all = [...this.rows.values()].map((r) => ({
      id: r.id, agent: r.agent, parent: r.parent, verb: r.verb, started_at: r.startedAt,
    }));
    const p = page(all, payload, (t) => String(t.id));
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
  cancel(id: number): number[] {
    const row = this.rows.get(id);
    if (row === undefined) return [];
    const killed: number[] = [];
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
