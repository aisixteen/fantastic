import type { Kernel } from "../../kernel/kernel.ts";
import type { Item } from "../../kernel/outcome.ts";
import type { Payload } from "../../kernel/json.ts";
import type { AgentRecord, StateEvent } from "../../kernel/state.ts";

// proxy_loader — the frontend kernel's ONE auto-added loader: the JS runtime's
// single autoagent (mirroring the host's root kernel_state; the lone exception to
// "no automation"). It owns NO storage — it PROXIES the loader contract
// (`load_tree` / `persist_record` / `forget_record`) over the WS channel to the
// host's `web_loader` (a `web/kernel_state` rooted at .fantastic/web/, reached by
// its id). Hydration flows: host disk → web_loader → channel → proxy_loader
// → kernel.load. The browser kernel is a real STATEFUL peer whose view-agent
// records persist on the host and rehydrate on reload. Registration (which
// bundles can RUN) is separate; the proxy_loader supplies the RECORDS.
//
// Re-rooting: on the host, the JS subtree nests under `web_loader`'s namespace
// anchor (the loader's own record, id `hostLoaderId`). `loadTree` re-roots the JS
// root (parent_id == hostLoaderId → null) and drops the anchor; `flush` rewrites
// the JS root's parent_id (local null → hostLoaderId) so it lands as a direct
// child of the namespace. The middle of the tree is untouched.

export interface ProxyLoaderOptions {
  /** Coalesce window before flushing dirty records (ms, default 150). */
  debounceMs?: number;
}

export class ProxyLoader {
  private readonly kernel: Kernel;
  private readonly hostLoaderId: string;
  private readonly debounceMs: number;
  private unsub: (() => void) | null = null;
  private timer: ReturnType<typeof setTimeout> | null = null;
  private readonly dirtyPersist = new Set<string>();
  /** id -> its (rewritten) parent id, so a `removed` event can rmtree the
   *  right NESTED dir on the host even though the agent is already gone. */
  private readonly dirtyForget = new Map<string, string>();
  private readonly parents = new Map<string, string>();

  constructor(
    kernel: Kernel,
    hostLoaderId: string,
    opts: ProxyLoaderOptions = {},
  ) {
    this.kernel = kernel;
    this.hostLoaderId = hostLoaderId;
    this.debounceMs = opts.debounceMs ?? 150;
  }

  /** Read the JS subtree from the host (forward `load_tree` over the bridge),
   *  drop the host loader's namespace anchor (its OWN record, id
   *  `hostLoaderId`), and re-root the JS root so `kernel.load(records)` builds a
   *  clean, self-rooted tree. */
  async loadTree(): Promise<AgentRecord[]> {
    const reply = (await this.kernel.send([
      { target: this.hostLoaderId, payload: { type: "load_tree" } },
    ]))[0]!.result as unknown as { records?: AgentRecord[] };
    const records = reply.records ?? [];
    const out: AgentRecord[] = [];
    for (const r of records) {
      if (r.id === this.hostLoaderId) continue; // the namespace anchor — not a JS agent
      const parent = r.parent_id ?? null;
      // re-root the JS root: its parent is the anchor
      if (parent === this.hostLoaderId) {
        out.push({ ...r, parent_id: null });
      } else {
        out.push(r);
      }
    }
    return out;
  }

  /** Subscribe to the LOCAL state stream + enqueue the current tree for an
   *  initial persist (idempotent merge-write on the host — self-heals a fresh
   *  session's seeded root and keeps the host in sync after a reboot). */
  start(): void {
    if (this.unsub !== null) return;
    for (const a of this.kernel.agents.values()) {
      this.parents.set(a.id, this.rewrittenParent(a.parentId));
      this.dirtyPersist.add(a.id);
    }
    this.unsub = this.kernel.addStateSubscriber((e) => this.onState(e));
    if (this.dirtyPersist.size > 0) this.schedule();
  }

  /** The JS root's local null parent maps to the host loader's REAL id (so it
   *  nests directly under the namespace the host resolves on disk); every other
   *  parent is unchanged. */
  private rewrittenParent(parentId: string | null): string {
    return parentId ?? this.hostLoaderId;
  }

  private onState(e: StateEvent): void {
    const aid = e.agent_id;
    if (e.kind === "added" || e.kind === "updated") {
      this.dirtyForget.delete(aid);
      this.dirtyPersist.add(aid);
      const a = this.kernel.get(aid);
      if (a !== undefined) this.parents.set(aid, this.rewrittenParent(a.parentId));
      this.schedule();
    } else if (e.kind === "removed") {
      this.dirtyPersist.delete(aid);
      this.dirtyForget.set(aid, this.parents.get(aid) ?? this.hostLoaderId);
      this.parents.delete(aid);
      this.schedule();
    }
  }

  private schedule(): void {
    if (this.timer !== null) return; // coalesce — one flush per debounce window
    this.timer = setTimeout(() => {
      this.timer = null;
      void this.flush();
    }, this.debounceMs);
  }

  /** Depth of a page agent: how many ancestors it has in this kernel (root = 0). */
  private depth(id: string): number {
    let d = 0;
    let cur = this.kernel.get(id)?.parentId ?? null;
    while (cur !== null) {
      d += 1;
      cur = this.kernel.get(cur)?.parentId ?? null;
    }
    return d;
  }

  /** Push all pending persists/forgets to the host loader over the bridge. */
  async flush(): Promise<void> {
    const persist = [...this.dirtyPersist];
    this.dirtyPersist.clear();
    const forget = [...this.dirtyForget];
    this.dirtyForget.clear();
    // PARENTS BEFORE CHILDREN — one batch per DEPTH, each awaited before the next.
    // The host loader scopes a record verb to the caller's own subtree by walking
    // the record's parent chain on disk, so a child sent in the same batch as its
    // new parent races it and is refused (measured on the canvas: `view_root` under
    // a `canvas` still in flight). Within one depth the records are independent and
    // go as ONE concurrent batch — a wave costs one round-trip, not one per record.
    const waves = new Map<number, Item[]>();
    for (const id of persist) {
      const a = this.kernel.get(id);
      if (a === undefined) continue;
      const record = a.record();
      record.parent_id = this.rewrittenParent(a.parentId);
      const d = this.depth(id);
      const wave = waves.get(d) ?? [];
      wave.push({
        target: this.hostLoaderId,
        payload: { type: "persist_record", record } as unknown as Payload,
      });
      waves.set(d, wave);
    }
    const forgets: Item[] = forget.map(([id, parentId]) => ({
      target: this.hostLoaderId,
      payload: { type: "forget_record", id, parent_id: parentId } as unknown as Payload,
    }));
    const refused: string[] = [];
    const settle = (items: Item[], outs: { result?: unknown; status?: string }[]) => {
      outs.forEach((o, n) => {
        const r = o.result as { error?: string } | null | undefined;
        if (o.status !== "completed" || (r && typeof r === "object" && r.error)) {
          const p = items[n]?.payload as { type?: string; record?: { id?: string }; id?: string };
          refused.push(`${p?.type} ${p?.record?.id ?? p?.id}: ${r?.error ?? o.status}`);
        }
      });
    };
    for (const d of [...waves.keys()].sort((x, y) => x - y)) {
      const wave = waves.get(d)!;
      settle(wave, await this.kernel.send(wave));
    }
    if (forgets.length > 0) settle(forgets, await this.kernel.send(forgets));
    // A REFUSED WRITE IS NOT DROPPED SILENTLY: the record would stay unsaved with
    // nothing left to say so. It fails loud, naming each refusal.
    if (refused.length > 0) {
      throw new Error(`ProxyLoader: the host loader refused ${refused.length} write(s): ${refused.join("; ")}`);
    }
  }

  /** Stop watching + final flush — a clean teardown loses nothing. */
  async stop(): Promise<void> {
    this.unsub?.();
    this.unsub = null;
    if (this.timer !== null) {
      clearTimeout(this.timer);
      this.timer = null;
    }
    await this.flush();
  }
}
