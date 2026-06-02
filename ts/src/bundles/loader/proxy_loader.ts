import type { Kernel } from "../../kernel/kernel.ts";
import type { Payload } from "../../kernel/json.ts";
import type { AgentRecord, StateEvent } from "../../kernel/state.ts";

// proxy_loader — the frontend kernel's ONE auto-added loader: the JS runtime's
// single autoagent (mirroring the host's root fs_loader; the lone exception to
// "no automation"). It owns NO storage — it PROXIES the loader contract
// (`load_tree` / `persist_record` / `forget_record`) over the WS channel to the
// host's `web_loader` (a `web/fs_loader` rooted at .fantastic/web/, reached by
// that alias). Hydration flows: host disk → web_loader → channel → proxy_loader
// → kernel.load. The browser kernel is a real STATEFUL peer whose view-agent
// records persist on the host and rehydrate on reload. Registration (which
// bundles can RUN) is separate; the proxy_loader supplies the RECORDS.
//
// Re-rooting: on the host, the JS subtree nests under `web_loader`'s namespace
// anchor. The constructor's `hostLoaderId` is the ADDRESSING alias (`web_loader`)
// — but on disk the anchor carries the loader's REAL id, and host paths resolve
// against that id, not the alias. So `loadTree` learns the real id from the
// anchor (the record whose `alias`/`id` is the one we addressed), re-roots the
// JS root (parent_id == realId → null), and drops the anchor; `flush` rewrites
// the JS root's parent_id (local null → realId) so it lands as a direct child of
// the namespace. The middle of the tree is untouched. (Before any loadTree the
// real id is unknown, so we fall back to the alias — fine for an in-memory host
// that stores parent_ids verbatim.)

export interface ProxyLoaderOptions {
  /** Coalesce window before flushing dirty records (ms, default 150). */
  debounceMs?: number;
}

export class ProxyLoader {
  private readonly kernel: Kernel;
  private readonly hostLoaderId: string;
  /** The host loader's REAL id (the anchor's on-disk id) — learned from
   *  `loadTree`; the addressing alias until then. Host paths resolve against
   *  this, so re-rooting (load + persist) must use it, not the alias. */
  private hostRootId: string;
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
    this.hostRootId = hostLoaderId; // until loadTree learns the real id
    this.debounceMs = opts.debounceMs ?? 150;
  }

  /** Read the JS subtree from the host (forward `load_tree` over the bridge),
   *  drop the host loader's namespace anchor, and re-root the JS root so
   *  `kernel.load(records)` builds a clean, self-rooted tree. The anchor is the
   *  loader's OWN record — matched by the addressed id/alias — and its on-disk
   *  id (the real one the host nests under) is learned here for later flushes. */
  async loadTree(): Promise<AgentRecord[]> {
    const reply = (await this.kernel.send(this.hostLoaderId, {
      type: "load_tree",
    })) as unknown as { records?: AgentRecord[] };
    const records = reply.records ?? [];
    const anchor = records.find(
      (r) => r.id === this.hostLoaderId || r["alias"] === this.hostLoaderId,
    );
    const anchorId = anchor?.id;
    if (anchorId !== undefined) this.hostRootId = anchorId;
    const out: AgentRecord[] = [];
    for (const r of records) {
      if (r.id === anchorId) continue; // the namespace anchor — not a JS agent
      const parent = r.parent_id ?? null;
      // re-root the JS root: its parent is the anchor (real id) or the alias
      if (parent === anchorId || parent === this.hostLoaderId) {
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
    return parentId ?? this.hostRootId;
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
      this.dirtyForget.set(aid, this.parents.get(aid) ?? this.hostRootId);
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

  /** Push all pending persists/forgets to the host loader over the bridge. */
  async flush(): Promise<void> {
    const persist = [...this.dirtyPersist];
    this.dirtyPersist.clear();
    const forget = [...this.dirtyForget];
    this.dirtyForget.clear();
    for (const id of persist) {
      const a = this.kernel.get(id);
      if (a === undefined) continue;
      const record = a.record();
      record.parent_id = this.rewrittenParent(a.parentId);
      await this.kernel.send(this.hostLoaderId, {
        type: "persist_record",
        record,
      } as unknown as Payload);
    }
    for (const [id, parentId] of forget) {
      await this.kernel.send(this.hostLoaderId, {
        type: "forget_record",
        id,
        parent_id: parentId,
      } as unknown as Payload);
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
