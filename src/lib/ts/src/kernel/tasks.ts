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
  /** A service loop that outlives every dispatch (the flusher): a stop neither
   *  waits for it nor cancels it — the stop ends it last (python `_keepalive`). */
  keepalive: boolean;
  /** Spawned work, not a dispatch: it never settles on its own, so a stop
   *  cancels it rather than waiting for it (python `_forked`). */
  forked: boolean;
}

/**
 * A KERNEL SERVICE TASK — a keepalive loop the ROOT owns: the execution flusher.
 * `cancel` never kills one: killing it would stop ledger durability until the
 * process restarted, and nothing restarts it. It
 * stays listed in `ps`; the kernel's own stop ends it. Python
 * `Kernel.is_service_task`.
 */
export function isService(row: TaskRow, root: string | null): boolean {
  return row.keepalive && root !== null && row.agent === root;
}

export interface TaskOpts {
  keepalive?: boolean;
  forked?: boolean;
}

export class TaskTable {
  private readonly rows = new Map<number, TaskRow>();

  register(id: number, agent: string, verb: string, parent: number | null, opts: TaskOpts = {}): TaskRow {
    const row: TaskRow = {
      id, agent, verb, killed: false, children: new Set(), parent, startedAt: nowSeconds(), onCancel: [],
      keepalive: opts.keepalive === true, forked: opts.forked === true,
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

  /** Drop one row. Surviving children are RE-PARENTED to its parent — init
   *  adoption (python `forget_task`): work that outlives the dispatch that
   *  spawned it stays in its caller's process group. */
  done(id: number): void {
    const row = this.rows.get(id);
    if (row === undefined) return;
    const up = row.parent !== null ? this.rows.get(row.parent) : undefined;
    up?.children.delete(id);
    for (const cid of row.children) {
      const c = this.rows.get(cid);
      if (c === undefined) continue;
      c.parent = row.parent;
      up?.children.add(cid);
    }
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
   * the caller usually cannot know whether the race was already lost. A kernel
   * service task below `id` (`isService` against `root`) is not part of the
   * group: it and its own work are left out.
   */
  cancel(id: number, root: string | null): number[] {
    const row = this.rows.get(id);
    if (row === undefined) return [];
    // POST-ORDER (python `task_subtree`): children before the parent that awaits
    // them — the kill order of a process group. A row already killed is on its
    // way out, and is not killed twice.
    const order: TaskRow[] = [];
    const walk = (r: TaskRow): void => {
      if (r.killed) return;
      for (const cid of [...r.children]) {
        const c = this.rows.get(cid);
        if (c !== undefined && !isService(c, root)) walk(c);
      }
      order.push(r);
    };
    walk(row);
    // MARK FIRST, abort second: a doomed handler's sends in between are refused
    // (`cancelled`) instead of starting work that outlives the cancel.
    for (const r of order) r.killed = true;
    for (const r of order) {
      // Hooks run BEFORE the abort lands, so a bundle can release what it holds.
      for (const fn of r.onCancel) {
        try {
          fn();
        } catch {
          // one bundle's failing hook must not strand the rest of the subtree
        }
      }
    }
    return order.map((r) => r.id);
  }

  size(): number {
    return this.rows.size;
  }
}
