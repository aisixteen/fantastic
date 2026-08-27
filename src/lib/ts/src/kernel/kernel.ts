import { Agent } from "./agent.ts";
import type { Handler } from "./agent.ts";
import type { Json, Payload } from "./json.ts";
import { str } from "./json.ts";
import { reflectIdentity, applyReflectFlags } from "./reflect.ts";
import type { BundleInfo } from "./reflect.ts";
import { CURRENT_VERSION, validateRecords } from "./state.ts";
import type { AgentRecord, Snapshot, StateEvent } from "./state.ts";
import { ExecutionLedger } from "./ledger.ts";
import { TaskTable } from "./tasks.ts";
import { runBatch } from "./batch.ts";
import type { Item, Outcome } from "./outcome.ts";
import { UNHANDLED, refusal } from "./refusal.ts";
import { answerKernelVerb, isKernelVerb } from "./kernel_verbs.ts";

/**
 * The bridge transport (P2). The pure core depends only on this interface, so
 * it stays headless-testable — a test injects a fake bridge, the browser
 * injects the real WS one. Non-local sends + remote watches go through here.
 */
export interface Bridge {
  forward(target: string, payload: Payload): Promise<Json>;
  /** like forward, but the payload may carry a binary blob (Uint8Array). */
  forwardBinary(target: string, payload: Record<string, unknown>): Promise<Json>;
  emitRemote(target: string, payload: Payload): void;
  watchRemote(src: string): void;
  unwatchRemote(src: string): void;
  onLifecycle(
    endpoint: string,
    fn: (state: "connected" | "disconnected") => void,
  ): () => void;
  subscribeState(handler: (frame: Record<string, unknown>) => void): () => void;
}

type InboxListener = (payload: Payload) => void;

/**
 * The frontend kernel: one primitive (`send`) over a registry of agents.
 * View-AGNOSTIC — no DOM, no view import (enforced by src/kernel/tsconfig.json's
 * DOM-less lib + tests/boundary.test.ts). View concerns live in bundles/.
 */
export class Kernel {
  readonly agents: Map<string, Agent> = new Map();
  rootId: string | null = null;
  bridge: Bridge | null = null;
  /** WHICH provider this kernel persists records THROUGH — surfaced on the root
   *  reflect as `persistence:{provider}` (mirrors py). For the browser frontend the
   *  loader sets this to the host loader id/alias it proxies to; null = unwired. */
  persistenceProvider: string | null = null;

  /** Every execution this kernel has run — dedup/join/retry/dead-letter. */
  readonly ledger = new ExecutionLedger();
  /** In-flight dispatches, as a tree: a cancel takes the subtree with it. */
  readonly tasks = new TaskTable();

  /**
   * The id that ORIGINATED the dispatch now running, or null for an operator.
   *
   * The browser has no `AsyncLocalStorage` and a module-global would leak
   * across every await, so this is not ambient state: `scopedFor` returns a
   * prototype-linked VIEW of this kernel carrying the origin, and a handler is
   * handed that view as its `kernel`. A handler therefore cannot forge an
   * origin — it never names one, it just sends.
   */
  readonly currentOrigin: string | null = null;
  /** The task id of the dispatch now running — a nested send parents to it. */
  readonly currentTask: string | null = null;

  /** handler_module -> the JS handler that runs that view bundle locally. */
  private readonly handlers: Map<string, Handler> = new Map();
  /** handler_module -> the bundle's capability/client readme. Surfaced by
   *  reflect readme=true when an agent of that type carries no per-record
   *  readme — the frontend's self-description, mirroring how host bundles
   *  attach a bundle-level readme. */
  private readonly bundleReadmes: Map<string, string> = new Map();
  /** src id -> watcher ids subscribed to its inbox. */
  private readonly watchers: Map<string, Set<string>> = new Map();
  /** id -> inbox listeners (a view-agent observing an agent's events). */
  private readonly inboxes: Map<string, InboxListener[]> = new Map();
  /** LOCAL state-stream taps (added/updated/removed). Distinct from
   *  `subscribeState`, which taps the HOST stream over the bridge. A loader
   *  agent subscribes here to persist the local tree. */
  private readonly stateSubscribers: Set<(e: StateEvent) => void> = new Set();

  /** Register a JS view bundle by its handler_module name. */
  registerBundle(handlerModule: string, handler: Handler): void {
    this.handlers.set(handlerModule, handler);
  }

  /** Attach a bundle-level readme (a frontend bundle's self-description —
   *  e.g. "html client for a host PTY"). Surfaced by reflect readme=true. */
  setBundleReadme(handlerModule: string, readme: string): void {
    this.bundleReadmes.set(handlerModule, readme);
  }

  /** The bundle-level readme for a handler_module, if one was registered. */
  bundleReadme(handlerModule: string): string | undefined {
    return this.bundleReadmes.get(handlerModule);
  }

  /** Add an agent to the tree, wiring it under its parent and (if its
   *  handler_module names a registered bundle) attaching that handler.
   *  Publishes an `added` state event for parented agents (the root, with no
   *  parent, publishes nothing — mirrors python `Agent.__init__`). */
  register(agent: Agent): Agent {
    if (agent.handler === null && agent.handlerModule !== null) {
      const h = this.handlers.get(agent.handlerModule);
      if (h !== undefined) agent.handler = h;
    }
    this.agents.set(agent.id, agent);
    if (agent.parentId !== null) {
      const parent = this.agents.get(agent.parentId);
      if (parent !== undefined) parent.children.set(agent.id, agent);
      this.publishState({
        agent_id: agent.id,
        kind: "added",
        name: agent.displayName,
        parent_id: agent.parentId,
      });
    }
    return agent;
  }

  setRoot(agent: Agent): void {
    if (!this.agents.has(agent.id)) this.register(agent);
    this.rootId = agent.id;
  }

  /** Patch a local agent's meta + publish an `updated` event (a loader
   *  re-persists the record). Returns the new record, or null if absent. */
  updateMeta(id: string, patch: Payload): AgentRecord | null {
    const a = this.agents.get(id);
    if (a === undefined) return null;
    Object.assign(a.meta, patch);
    this.publishState({
      agent_id: id,
      kind: "updated",
      changed: Object.keys(patch),
    });
    return a.record();
  }

  /** Cascade-remove a local agent + its subtree from the tree (deepest first)
   *  and publish a `removed` event per node (a loader rmtrees each). */
  remove(id: string): boolean {
    const a = this.agents.get(id);
    if (a === undefined) return false;
    for (const cid of [...a.children.keys()]) this.remove(cid);
    this.agents.delete(id);
    this.inboxes.delete(id);
    this.watchers.delete(id);
    if (a.parentId !== null) this.agents.get(a.parentId)?.children.delete(id);
    this.publishState({ agent_id: id, kind: "removed", name: a.displayName });
    return true;
  }

  get(id: string): Agent | undefined {
    return this.agents.get(id);
  }

  // ---- snapshot (medium-agnostic; a LOADER agent owns the medium) --------

  /** Snapshot the live tree as a flat record list (mirror python
   *  `Kernel.save` / rust `Kernel::save`). Sorted by id for deterministic
   *  output. A loader turns this into bytes (here: frames over the bridge). */
  save(): Snapshot {
    const records = [...this.agents.values()].map((a) => a.record());
    records.sort((x, y) => (x.id < y.id ? -1 : x.id > y.id ? 1 : 0));
    return { version: CURRENT_VERSION, records };
  }

  /** Replace the tree with a flat record list (mirror python `Kernel.load`).
   *  Accepts a bare list or a `{version, records}` envelope. Validates (one
   *  root / unique ids / resolvable parents), drops the current tree, then
   *  DFS-rebuilds from the root.
   *
   *  Weak-load: a record whose `handler_module` names a bundle NOT registered
   *  in this runtime is logged + skipped along with its whole subtree, left
   *  untouched in the snapshot. (The JS kernel loads only its own view-agent
   *  subtree — host agents are reached by id over the bridge, never loaded —
   *  so an unknown handler_module is genuinely not-installed here, exactly as
   *  in the python host.) Builds in-memory; fires no `boot`. */
  load(snapshot: Snapshot | AgentRecord[]): void {
    const records = Array.isArray(snapshot) ? snapshot : snapshot.records;
    const version = Array.isArray(snapshot)
      ? CURRENT_VERSION
      : (snapshot.version ?? CURRENT_VERSION);
    validateRecords(records, version);

    const children = new Map<string, AgentRecord[]>();
    let rootRec: AgentRecord | null = null;
    for (const r of records) {
      const pid = r.parent_id ?? null;
      if (pid === null) rootRec = r;
      else {
        const arr = children.get(pid) ?? [];
        arr.push(r);
        children.set(pid, arr);
      }
    }

    this.agents.clear();
    this.inboxes.clear();
    this.watchers.clear();
    this.rootId = null;

    const build = (rec: AgentRecord, parentId: string | null): void => {
      const hm = rec.handler_module ?? null;
      if (hm !== null && !this.handlers.has(hm)) {
        // canonical log shape — CI + selftests grep it verbatim, identical
        // across runtimes. Guarded globalThis.console keeps the pure kernel
        // env-agnostic (no DOM/node lib).
        (
          globalThis as { console?: { warn(m: string): void } }
        ).console?.warn(
          `[kernel] skipping agent ${rec.id}: bundle ${hm} not installed in this runtime`,
        );
        return; // weak-load: skip this record AND its subtree
      }
      const meta: Payload = {};
      for (const [k, v] of Object.entries(rec)) {
        if (k !== "id" && k !== "handler_module" && k !== "parent_id") {
          meta[k] = v as Json;
        }
      }
      const agent = new Agent({ id: rec.id, handlerModule: hm, parentId, meta });
      this.register(agent);
      if (parentId === null) this.rootId = agent.id;
      for (const child of children.get(rec.id) ?? []) build(child, agent.id);
    };

    if (rootRec !== null) build(rootRec, null);
  }

  /** Resolve the `"kernel"` alias to the root id. */
  private resolve(targetId: string): string {
    return targetId === "kernel" && this.rootId !== null
      ? this.rootId
      : targetId;
  }

  /** A view of this kernel whose sends are attributed to `agentId`. Shares all
   *  state by prototype — the ledger, the table and the tree are the SAME
   *  objects, only the origin differs. */
  scopedFor(agentId: string, taskId: string | null): Kernel {
    const view = Object.create(this) as Kernel;
    Object.defineProperty(view, "currentOrigin", { value: agentId, enumerable: true });
    Object.defineProperty(view, "currentTask", { value: taskId, enumerable: true });
    return view;
  }

  /** Resolve the `"kernel"` alias for callers outside this module. */
  resolveId(targetId: string): string {
    return this.resolve(targetId);
  }

  /**
   * THE ONE PRIMITIVE: `send([Item]) -> [Outcome]`, array-only.
   *
   * A single call is a one-element array — there is no sugar overload, and its
   * ABSENCE is what stops single and batch drifting apart:
   *
   *     const r = (await kernel.send([{ target, payload }]))[0].result;
   *
   * Launch is synchronous for the whole batch (so `ps` shows it at once), the
   * settle phase is concurrent, and the returned array preserves INPUT order.
   */
  async send(items: Item[]): Promise<Outcome[]> {
    return runBatch(this, items);
  }

  /**
   * Dispatch ONE resolved call — the inside of an execution, after validation,
   * the ledger and the task row. Local agent → run here; unknown target →
   * forward over the bridge to the host.
   *
   * `reflect` is answered uniformly by the KERNEL for every local agent, never
   * by the handler, so it cannot drift bundle by bundle.
   */
  async dispatch(targetId: string, payload: Payload, taskId: string): Promise<Json> {
    const id = this.resolve(targetId);
    const target = this.agents.get(id);
    const verb = str(payload, "type");

    if (target === undefined) {
      // FEDERATION: not ours, so it belongs to the host. `launch` already
      // refused the no-bridge case, so a bridge is present here.
      return (this.bridge as Bridge).forward(id, payload);
    }
    if (verb === "reflect") {
      const identity = reflectIdentity(target, this.rootId);
      return applyReflectFlags(this, target, payload, identity);
    }
    // Kernel verbs act on the TABLE and the LEDGER, so they are the root's to
    // answer and the named agent is the operand — refused HERE, in the handler,
    // not in the gate.
    if (isKernelVerb(verb)) return answerKernelVerb(this, id, verb, payload);
    if (target.handler !== null) {
      // The handler receives a SCOPED kernel: its own nested sends are
      // attributed to it, and parent to this task so a cancel reaches them.
      return target.handler(target.id, payload, this.scopedFor(target.id, taskId));
    }
    if (verb === "boot" || verb === "shutdown" || verb === "") {
      return null;
    }
    return refusal(UNHANDLED, `agent '${id}' is bare (no handler); cannot answer verb '${verb}'`, {
      hint: "give the agent a handler_module, or send a verb the kernel answers (reflect)",
      extra: { verb },
    });
  }

  // ---- events: inbox + watch fan-out -------------------------------------

  /** A view-agent subscribes to an agent's inbox; returns an unsubscribe fn. */
  onInbox(id: string, listener: InboxListener): () => void {
    const arr = this.inboxes.get(id) ?? [];
    arr.push(listener);
    this.inboxes.set(id, arr);
    return () => {
      const cur = this.inboxes.get(id);
      if (cur !== undefined) {
        this.inboxes.set(
          id,
          cur.filter((l) => l !== listener),
        );
      }
    };
  }

  /** Fire-and-forget emit to a REMOTE host agent's inbox (e.g. `reload_html`).
   *  Distinct from `emit`, which is LOCAL fan-out only — the bridge calls
   *  `emit` for inbound host events, so routing `emit` to the bridge would loop. */
  emitRemote(targetId: string, payload: Payload): void {
    this.bridge?.emitRemote(this.resolve(targetId), payload);
  }

  /** Call a REMOTE host agent with a binary-bearing payload (e.g. paste_image). */
  forwardBinary(targetId: string, payload: Record<string, unknown>): Promise<Json> {
    if (this.bridge === null) {
      return Promise.resolve({ error: `no bridge to forward to '${targetId}'` });
    }
    return this.bridge.forwardBinary(this.resolve(targetId), payload);
  }

  /** Call the HOST directly over the bridge, bypassing LOCAL resolution. The
   *  frontend's `resolve` maps `"kernel"` to the LOCAL root (the canvas), so a
   *  plain `send("kernel", …)` would dispatch locally; this forwards the
   *  LITERAL target so the HOST resolves `"kernel"` to ITS OWN root
   *  (`kernel_state` on python, `core` on rust/swift). Used to create host peers +
   *  read the host bundle catalog without the frontend naming a host id. */
  callHost(target: string, payload: Payload): Promise<Json> {
    if (this.bridge === null) {
      return Promise.resolve({ error: `no bridge to reach host '${target}'` });
    }
    return this.bridge.forward(target, payload);
  }

  /** connect/disconnect notifications for a remote endpoint's socket. */
  onLifecycle(
    endpoint: string,
    fn: (state: "connected" | "disconnected") => void,
  ): () => void {
    return this.bridge?.onLifecycle(this.resolve(endpoint), fn) ?? (() => {});
  }

  /** Subscribe to the host kernel state stream (telemetry), over the bridge. */
  subscribeState(handler: (frame: Record<string, unknown>) => void): () => void {
    return this.bridge?.subscribeState(handler) ?? (() => {});
  }

  // ---- local state stream (added/updated/removed) ------------------------

  /** Subscribe a synchronous tap to THIS kernel's own lifecycle events
   *  (added/updated/removed). Returns an unsubscribe closure. A loader agent
   *  uses this to persist the local tree. Distinct from `subscribeState`,
   *  which taps the remote HOST stream. */
  addStateSubscriber(cb: (e: StateEvent) => void): () => void {
    this.stateSubscribers.add(cb);
    return () => this.stateSubscribers.delete(cb);
  }

  /** Dispatch one event to every local state subscriber (adds `ts`). Never
   *  routes through send/emit/inboxes — no recursion path. */
  publishState(event: StateEvent): void {
    if (this.stateSubscribers.size === 0) return;
    const stamped = { ...event, ts: Date.now() };
    for (const cb of [...this.stateSubscribers]) {
      try {
        cb(stamped);
      } catch {
        // a subscriber throwing must not break dispatch to the others
      }
    }
  }

  /** Deliver an event to an agent's inbox and to everyone watching it. */
  emit(targetId: string, payload: Payload): void {
    const id = this.resolve(targetId);
    this.deliver(id, payload);
    const ws = this.watchers.get(id);
    if (ws !== undefined) for (const w of ws) this.deliver(w, payload);
  }

  private deliver(id: string, payload: Payload): void {
    const ls = this.inboxes.get(id);
    if (ls === undefined) return;
    for (const l of [...ls]) {
      try {
        l(payload);
      } catch {
        // a listener throwing must not break fan-out to the others
      }
    }
  }

  /** Watch `src` on behalf of `watcher`. If `src` is remote, ask the bridge. */
  watch(srcId: string, watcherId: string): void {
    const id = this.resolve(srcId);
    const set = this.watchers.get(id) ?? new Set();
    const firstLocally = set.size === 0;
    set.add(watcherId);
    this.watchers.set(id, set);
    const local = this.agents.get(id);
    if ((local === undefined || local.handler === null) && firstLocally) {
      this.bridge?.watchRemote(id);
    }
  }

  unwatch(srcId: string, watcherId: string): void {
    const id = this.resolve(srcId);
    const set = this.watchers.get(id);
    if (set === undefined) return;
    set.delete(watcherId);
    if (set.size === 0) {
      this.watchers.delete(id);
      const local = this.agents.get(id);
      if (local === undefined || local.handler === null) {
        this.bridge?.unwatchRemote(id);
      }
    }
  }

  // ---- reflect support ----------------------------------------------------

  /** The frontend's view-bundle catalog, for reflect `bundles=all|ids`. */
  availableBundles(): BundleInfo[] {
    return [...this.handlers.keys()]
      .map((hm) => ({ name: bundleName(hm), handler_module: hm }))
      .sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
  }
}

function bundleName(handlerModule: string): string {
  return handlerModule.replace(/\.(tools|js|ts)$/, "");
}
