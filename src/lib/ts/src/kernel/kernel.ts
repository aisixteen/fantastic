import { Agent } from "./agent.ts";
import type { Handler } from "./agent.ts";
import type { Json, Payload } from "./json.ts";
import { str } from "./json.ts";
import { pyRepr, pyTruthy } from "./pyjson.ts";
import { reflectIdentity, applyReflectFlags } from "./reflect.ts";
import type { BundleInfo } from "./reflect.ts";
import { CURRENT_VERSION, validateRecords } from "./state.ts";
import type { AgentRecord, Snapshot, StateEvent } from "./state.ts";
import { ExecutionLedger } from "./ledger.ts";
import { TaskTable } from "./tasks.ts";
import type { TaskRow } from "./tasks.ts";
import { KernelStore } from "./kernel_store.ts";
import { runBatch } from "./batch.ts";
import { nowSeconds } from "./timers.ts";
import type { Item, Outcome } from "./outcome.ts";
import { errorShaped } from "./outcome.ts";
import { BAD_ARG, LOCKED, UNHANDLED, refusal, unknown } from "./refusal.ts";
import { answerKernelVerb, isKernelVerb } from "./kernel_verbs.ts";
import { authorizeCall, denial } from "./authz.ts";

/**
 * The bridge transport (P2). The pure core depends only on this interface, so
 * it stays headless-testable — a test injects a fake bridge, the browser
 * injects the real WS one. Non-local sends + remote watches go through here.
 */
export interface Bridge {
  /** Call `target` on the host. The host judges the call as the leg this kernel
   *  is connected through — the frame names no caller. */
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
   *  loader sets this to the host loader id it proxies to; null = unwired. */
  persistenceProvider: string | null = null;

  /** Executions still in RAM — in flight, or settled and not yet flushed:
   *  dedup/join/retry/dead-letter. The rest live in the root's memory store. */
  readonly ledger = new ExecutionLedger();
  /** In-flight dispatches, as a tree: a cancel takes the subtree with it. */
  readonly tasks = new TaskTable();
  /** The one id counter, the execution rows not yet flushed, and the link to
   *  the root's memory store (`kernel_store.ts`). Shared by every view. */
  readonly store: KernelStore;
  /** Per-kernel RAM a bundle keeps for itself (never persisted) — e.g. an AI
   *  agent's head cache. Keyed by the bundle; shared by every view. */
  readonly bundleMem: Map<string, unknown> = new Map();

  /**
   * The id that ORIGINATED the dispatch now running, or null for an operator.
   *
   * IDENTITY COMES ONLY FROM DISPATCH. The browser has no `AsyncLocalStorage`
   * and a module-global would leak across every await, so this is not ambient
   * state: when the kernel runs an agent's handler it hands it a
   * prototype-linked VIEW of this kernel carrying that agent as the origin, and
   * the sends made through that view (and through anything the handler starts
   * with it) are the agent's. There is no API to send as anyone else: code that
   * runs outside every dispatch — the app, a transport's frame handler, a test
   * driver — holds the kernel itself and sends as the operator.
   */
  readonly currentOrigin: string | null = null;
  /** The task id of the dispatch now running — a nested send parents to it. */
  readonly currentTask: number | null = null;
  /**
   * The task-table ROW of the dispatch now running — the row object of THIS
   * attempt, not its id. JS cannot abort a handler, so a cancelled one keeps
   * running after its row has left the table (and a retry may re-register the
   * same id); the row it was handed still says `killed`, and every later send it
   * makes is refused `cancelled` (python: a dying task registers no new work).
   */
  readonly currentTaskRow: TaskRow | null = null;
  /**
   * WHO CALLED the dispatch now running (null = the operator, or the kernel's
   * own bookkeeping). A handler's own sends are attributed to itself
   * (`currentOrigin`), so without this it could not tell whose request it is
   * serving — which a verb whose PAYLOAD names a resource needs: a memory store
   * keys every row by its writer.
   */
  readonly currentCaller: string | null = null;
  /**
   * The SHUTDOWN of a stop (python `shutdown_context`): set on the view a
   * `shutdown` bookkeeping call runs under, so what that handler sends passes
   * the drain. Inherited by the dispatches those sends start.
   */
  readonly kernelTraffic: boolean = false;

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

  constructor() {
    this.store = new KernelStore(this);
  }

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
   *  and publish a `removed` event per node (a loader rmtrees each).
   *
   *  THIS kernel's delete: tree CRUD verbs belong to the host root, so a page
   *  removes its own agents here — and `delete_lock` is enforced here, exactly
   *  as the host kernels' `delete_agent` enforces it (owner, 2026-09-30: the
   *  canvas background is a page agent with `delete_lock: true`). If the target
   *  OR any descendant carries a truthy `delete_lock`, NOTHING is removed (no
   *  partial cascade) and the answer is the `locked` refusal naming the lock;
   *  clear it with `updateMeta(id, {delete_lock: false})`. An unknown id is the
   *  generic `unknown_agent` with `deleted: false`. THE ROOT IS NOT REMOVABLE
   *  (python `Agent.delete`): it answers the kernel verbs and its loader owns
   *  persistence, so cascading it would empty the kernel — and `forget` this
   *  kernel's whole namespace on the host. Success is `{deleted: true, id}` — the
   *  host's `delete_agent` reply. */
  remove(id: string): Payload {
    const a = this.agents.get(id);
    if (a === undefined) {
      return unknown(id, [...this.agents.keys()], { deleted: false });
    }
    if (a.parentId === null) {
      return refusal(
        BAD_ARG,
        `remove: ${id} is the ROOT — it owns persistence and answers the kernel verbs; deleting it would leave no kernel`,
        { extra: { arg: "id", id, deleted: false } },
      );
    }
    const blockedBy = this.findLockedDescendant(a);
    if (blockedBy !== null) {
      return refusal(
        LOCKED,
        `remove: '${id}' blocked by delete_lock on '${blockedBy}'; clear it via updateMeta before deleting`,
        { hint: "clear the lock with updateMeta(id, {delete_lock: false}), then remove again", extra: { blocked_by: blockedBy, id } },
      );
    }
    this.cascadeRemove(a);
    return { deleted: true, id };
  }

  /** The first agent in `a`'s subtree (itself included, depth-first) whose
   *  `delete_lock` is truthy — python truthiness, as the host kernels read it. */
  private findLockedDescendant(a: Agent): string | null {
    if (pyTruthy(a.meta["delete_lock"])) return a.id;
    for (const c of a.children.values()) {
      const hit = this.findLockedDescendant(c);
      if (hit !== null) return hit;
    }
    return null;
  }

  /** Remove `a` and its subtree, deepest first — only after the lock check. */
  private cascadeRemove(a: Agent): void {
    const id = a.id;
    for (const c of [...a.children.values()]) this.cascadeRemove(c);
    this.agents.delete(id);
    this.inboxes.delete(id);
    this.watchers.delete(id);
    if (a.parentId !== null) this.agents.get(a.parentId)?.children.delete(id);
    this.publishState({ agent_id: id, kind: "removed", name: a.displayName });
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
   *  Accepts a bare list or a `{version, records}` envelope. Validates FIRST
   *  (`validateRecords`: safe ids, no reserved keys, one root, resolvable
   *  parents, every record reachable — python `validate_records`), then drops
   *  the current tree and DFS-rebuilds from the root.
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

    if (rootRec !== null) {
      build(rootRec, null);
      this.store.resume(rootRec["next_id"]);
    }
  }

  /** Resolve the reserved `"kernel"` address to the root id. */
  private resolve(targetId: string): string {
    return targetId === "kernel" && this.rootId !== null
      ? this.rootId
      : targetId;
  }

  /** The view `agentId`'s handler runs under — the ONLY place an origin is
   *  made: its sends are the agent's, parented to `taskId`, and `caller` is who
   *  asked (null: the operator, or the kernel's own bookkeeping). Shares all
   *  state by prototype — the ledger, the table and the tree are the SAME
   *  objects, only the origin differs. */
  private viewFor(agentId: string, taskId: number | null, caller: string | null): Kernel {
    const view = Object.create(this) as Kernel;
    Object.defineProperty(view, "currentOrigin", { value: agentId, enumerable: true });
    Object.defineProperty(view, "currentCaller", { value: caller, enumerable: true });
    setTask(view, taskId, this.tasks);
    return view;
  }

  // ---- identity + the one counter ----------------------------------------

  /** The kernel's stable INSTANCE identity: minted once when the root boots
   *  (an explicit one wins, a minted one is never regenerated) and kept in the
   *  root record, so the proxy_loader persists it. Every memory-store key is
   *  `<guid>|<agent id>`. null until the root has booted. */
  get guid(): string | null {
    const root = this.rootId !== null ? this.agents.get(this.rootId) : undefined;
    const g = root?.meta["guid"];
    return typeof g === "string" && g !== "" ? g : null;
  }

  /** The next message id — the ONLY allocator (see kernel_store.ts). */
  mintId(): number {
    return this.store.mintId();
  }

  private ensureGuid(): void {
    if (this.rootId === null || this.guid !== null) return;
    this.updateMeta(this.rootId, { guid: mintGuid() });
  }

  /**
   * Run the two-sided gate for one edge. Returns a refusal, or null to allow.
   *
   * BOTH ends must grant it: the sender's `can_call` (egress) and the target's
   * `callable_by` (ingress). `side` on the refusal says which end refused, so a
   * caller can tell "I may not call" from "they do not accept me". A kernel
   * verb the root answers takes the SAME two-sided check against the root —
   * there is no ingress skip.
   */
  gate(senderId: string, verb: string, targetId: string): Payload | null {
    return this.egressGate(senderId, verb, targetId) ?? this.ingressGate(senderId, verb, targetId);
  }

  /**
   * Every id ABOVE `id` — what `$subtree` matches against: a grant holder reaches
   * the other end when it is one of these. An id with no local agent (a host-bound
   * target) has none.
   */
  ancestorIds(id: string): Set<string> {
    const out = new Set<string>();
    let next = this.agents.get(id)?.parentId ?? null;
    while (next !== null && !out.has(next)) {
      out.add(next);
      next = this.agents.get(next)?.parentId ?? null;
    }
    return out;
  }

  /**
   * The INGRESS half alone — does `targetId` accept `senderId` for `verb`? Its
   * `callable_by`, tokens relative to the target. A sender is always a LOCAL
   * agent, so a `<guid>|…` key there never matches.
   */
  ingressGate(senderId: string, verb: string, targetId: string): Payload | null {
    const target = this.agents.get(targetId);
    if (target === undefined) return null;
    const ok = authorizeCall(target.meta["callable_by"] as Payload | undefined, verb, senderId, {
      holderId: target.id,
      holderParent: target.parentId,
      otherAncestors: this.ancestorIds(senderId),
    });
    return ok ? null : denial("ingress", senderId, targetId, verb);
  }

  /**
   * The EGRESS half alone — may the SENDER call `targetId` with `verb`? Its
   * `can_call`, tokens relative to the sender (`$self`/`$parent`/`$subtree`).
   *
   * It is the WHOLE local check for a HOST-BOUND send: the target lives on the
   * host, so its `callable_by` is the host's to apply to the arriving call — but
   * whether this agent may SEND is decided here, BEFORE anything reaches the
   * bridge. Forwarding first would make every agent's reach to the host the
   * operator's.
   *
   * A sender that is not an agent of this kernel has NO record here, so it holds
   * NO grant: python reads its record as `{id}` (empty `can_call`) and refuses
   * it, whatever the target's `callable_by` says.
   */
  egressGate(senderId: string, verb: string, targetId: string): Payload | null {
    const sender = this.agents.get(senderId);
    const ok = authorizeCall(sender?.meta["can_call"] as Payload | undefined, verb, targetId, {
      holderId: senderId,
      holderParent: sender?.parentId ?? null,
      otherAncestors: this.ancestorIds(targetId),
    });
    return ok ? null : denial("egress", senderId, targetId, verb);
  }

  /** Resolve a SEND target (python `_launch`): the reserved `"kernel"` address,
   *  and `<this kernel's guid>|<agent>` — this kernel's own qualifier names the
   *  LOCAL agent (`<guid>|kernel` the root). Any other id is returned as is. */
  resolveId(targetId: string): string {
    const own = this.ownQualified(targetId);
    return own !== null ? this.resolve(own) : this.resolve(targetId);
  }

  /** The local id `<this kernel's guid>|<agent>` names, or null when the id is
   *  not qualified by this kernel's own guid (python `_qualified_target`). */
  ownQualified(targetId: string): string | null {
    const bar = targetId.indexOf("|");
    if (bar <= 0) return null;
    const guid = targetId.slice(0, bar);
    const agentId = targetId.slice(bar + 1);
    if (agentId === "" || agentId.includes("|") || guid !== this.guid) return null;
    return agentId;
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
   */
  async dispatch(targetId: string, payload: Payload, taskId: number): Promise<Json> {
    const id = this.resolve(targetId);
    const target = this.agents.get(id);
    if (target === undefined) {
      // FEDERATION: not ours, so it belongs to the host. `launch` already
      // refused the no-bridge case, so a bridge is present here.
      return (this.bridge as Bridge).forward(id, payload);
    }
    // `this` is the CALLER's view; the handler runs under the target's own.
    return this.run(target, payload, this.viewFor(target.id, taskId, this.currentOrigin));
  }

  /**
   * ONE KERNEL BOOKKEEPING CALL — the kernel runs `agentId`'s handler DIRECTLY
   * (python `Kernel.bookkeeping`): a boot at `kernel_start`, a `shutdown` at a
   * stop, a read or write of the kernel's ledger in its memory store. A plain
   * call, not a send — no gate, no task row, no call row — so it is the same
   * whoever's dispatch asked for it (the operator, or the root answering a
   * kernel verb). The handler runs as the agent (its sends are its own) with no
   * caller: the kernel asked — inside `task` (default: the task now running; the
   * flusher passes its own). `shutdown` marks the view as the stop's own
   * traffic, so what the handler sends passes the drain.
   *
   * Answers the outcome shape a caller branches on — `{status, result}`: a throw
   * is `failed`, a refusal `errored`, anything else `completed`.
   */
  async bookkeeping(
    agentId: string,
    payload: Payload,
    opts: { shutdown?: boolean; task?: number | null } = {},
  ): Promise<{ status: Outcome["status"]; result: Json }> {
    const target = this.agents.get(agentId);
    if (target === undefined) return { status: "failed", result: unknown(agentId, [...this.agents.keys()]) };
    const view = this.viewFor(target.id, opts.task !== undefined ? opts.task : this.currentTask, null);
    if (opts.shutdown === true) Object.defineProperty(view, "kernelTraffic", { value: true, enumerable: true });
    try {
      const result = await this.run(target, payload, view);
      return { status: errorShaped(result) ? "errored" : "completed", result };
    } catch (err) {
      const msg = err instanceof Error ? `${err.name}: ${err.message}` : String(err);
      return { status: "failed", result: { error: msg, raised: true } };
    }
  }

  /**
   * Run one LOCAL agent for `payload` under `view` (the agent's own): the
   * payload is mirrored, then answered — `reflect` by the KERNEL for every
   * agent (so it cannot drift bundle by bundle), a kernel verb by the root,
   * anything else by the handler.
   */
  private run(target: Agent, payload: Payload, view: Kernel): Json | Promise<Json> {
    const id = target.id;
    const verb = str(payload, "type");
    // EVERY DISPATCH IS MIRRORED (python `_fanout(kind="send")`): the payload
    // lands in the target's own inbox and in every watcher's — which is what a
    // `watch` subscribes to, and why it is gated like a call.
    this.fanout(id, payload);
    if (verb === "reflect") {
      const identity = reflectIdentity(target, this.rootId);
      return applyReflectFlags(this, target, payload, identity);
    }
    // Kernel verbs act on the TABLE, the LEDGER and the LIFECYCLE, so they are
    // the root's to answer and the named agent is the operand — refused HERE, in
    // the handler, not in the gate.
    if (isKernelVerb(verb)) return answerKernelVerb(view, id, verb, payload);
    // THE ROOT'S BOOT MINTS THE GUID every memory-store key carries.
    if (verb === "boot" && id === this.rootId) this.ensureGuid();
    if (target.handler !== null) {
      // The handler receives its own view: its nested sends are attributed to
      // it, and parent to this task so a cancel reaches them.
      return target.handler(target.id, payload, view);
    }
    if (verb === "boot" || verb === "shutdown" || verb === "") {
      return null;
    }
    return refusal(UNHANDLED, `agent ${pyRepr(id)} has no handler_module; cannot answer verb ${pyRepr(verb)}`, {
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
    const id = this.resolve(targetId);
    const denied = this.hostBound(id, payload);
    if (denied !== null) return Promise.resolve(denied);
    return this.bridge.forwardBinary(id, payload);
  }

  /**
   * The check a host-bound call passes before the bridge sees a byte: an
   * AGENT's own EGRESS gate (the operator, with no origin, is the root of
   * trust). These calls bypass the batch, so this kernel keeps no record of
   * them. Answers the refusal, or null.
   */
  private hostBound(targetId: string, payload: Record<string, unknown>): Payload | null {
    if (this.currentOrigin === null) return null;
    const verb = typeof payload["type"] === "string" ? payload["type"] : "";
    return this.egressGate(this.currentOrigin, verb, targetId);
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
    const denied = this.hostBound(target, payload);
    if (denied !== null) return Promise.resolve(denied);
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

  /** Dispatch one event to every local state subscriber (adds `ts`, seconds
   *  since the epoch — python `time.time()`, like every other kernel stamp).
   *  Never routes through send/emit/inboxes — no recursion path. */
  publishState(event: StateEvent): void {
    if (this.stateSubscribers.size === 0) return;
    const stamped = { ...event, ts: nowSeconds() };
    for (const cb of [...this.stateSubscribers]) {
      try {
        cb(stamped);
      } catch {
        // a subscriber throwing must not break dispatch to the others
      }
    }
  }

  /** Deliver an event to an agent's inbox and to everyone watching it.
   *  GATED LIKE A SEND when an agent emits onto ANOTHER agent (audit
   *  2026-09-28, a16): the emitter's `can_call` and the target's `callable_by`
   *  must both allow verb `emit`. A denied emit is dropped; an emit has no
   *  reply. */
  emit(targetId: string, payload: Payload): void {
    const id = this.resolve(targetId);
    if (this.currentOrigin !== null && this.currentOrigin !== id) {
      if (this.gate(this.currentOrigin, "emit", id) !== null) return;
    }
    this.fanout(id, payload);
  }

  /** `id`'s own inbox, then every watcher's (python `_fanout`). */
  private fanout(id: string, payload: Payload): void {
    this.deliver(id, payload);
    const ws = this.watchers.get(id);
    if (ws !== undefined) for (const w of [...ws]) this.deliver(w, payload);
  }

  /** Whether anything reads `id`'s inbox (a watcher with no reader receives
   *  nothing, so a verb that would deliver there refuses up front). */
  hasInbox(id: string): boolean {
    return (this.inboxes.get(id)?.length ?? 0) > 0;
  }

  /** Put one payload in `id`'s inbox ONLY — no fan-out to its watchers (an AI
   *  agent's row event goes to exactly the watcher it is for). */
  deliverInbox(id: string, payload: Payload): void {
    this.deliver(id, payload);
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

  /** Subscribe `watcher` to `src`'s inbox. If `src` is remote, ask the bridge. */
  watch(srcId: string, watcherId: string): Payload | null {
    const id = this.resolve(srcId);
    // #39 — THE READ SIDE OF THE GATE. A watcher receives its target's raw
    // inbound payloads VERBATIM: the model's prompt, the document body, the
    // packed context. Ungated, `watch` is a read side-channel AROUND
    // `callable_by` — an agent that may not CALL another could still read
    // everything sent TO it. The `call` path went through the gate; this never
    // did, which is exactly why it was worth nothing to hold.
    //
    // The edge is gated as `<whoever is running> → src` under the verb `watch`
    // (python `_current_sender`): the agent whose code asks is judged by ITS
    // grants, never by those of the id it names as the reader. An operator watch
    // (no origin) stays ungated, as with send.
    if (this.currentOrigin !== null) {
      const d = this.gate(this.currentOrigin, "watch", id);
      if (d !== null) return d;
    }
    const set = this.watchers.get(id) ?? new Set();
    const firstLocally = set.size === 0;
    set.add(watcherId);
    this.watchers.set(id, set);
    const local = this.agents.get(id);
    if ((local === undefined || local.handler === null) && firstLocally) {
      // Watching a REMOTE agent goes out as the BRIDGE, not as this watcher:
      // a local id means nothing in the host's grant space, and the local gate
      // above already authorized reaching it.
      this.bridge?.watchRemote(id);
    }
    return null;
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

/** Stamp a view's task: its id, and the row this attempt runs under. */
function setTask(view: Kernel, taskId: number | null, tasks: TaskTable): void {
  Object.defineProperty(view, "currentTask", { value: taskId, enumerable: true });
  Object.defineProperty(view, "currentTaskRow", {
    value: taskId !== null ? (tasks.get(taskId) ?? null) : null,
    enumerable: true,
  });
}

function bundleName(handlerModule: string): string {
  return handlerModule.replace(/\.(tools|js|ts)$/, "");
}

/** A fresh instance guid: 128 random bits, hex. NO FALLBACK — a guid that is
 *  not random would let two kernels key their memory-store rows alike. */
function mintGuid(): string {
  const c = (globalThis as {
    crypto?: { getRandomValues?: (a: Uint8Array) => Uint8Array };
  }).crypto;
  if (c === undefined || typeof c.getRandomValues !== "function") {
    throw new Error("kernel: this runtime has no crypto.getRandomValues — cannot mint the kernel guid");
  }
  const b = new Uint8Array(16);
  c.getRandomValues(b);
  return [...b].map((x) => x.toString(16).padStart(2, "0")).join("");
}
