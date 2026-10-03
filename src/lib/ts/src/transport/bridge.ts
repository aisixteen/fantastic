import type { Bridge, Kernel } from "../kernel/kernel.ts";
import type { Json, Payload } from "../kernel/json.ts";
import type { Item } from "../kernel/outcome.ts";
import type { Mark } from "../kernel/secret.ts";
import { encodeFrame, decodeFrame } from "./frame.ts";
import type { Envelope } from "./frame.ts";

// The WS bridge: the frontend kernel's federation transport. Speaks the EXACT
// wire python web_ws/_proxy.py serves (call/emit/watch/unwatch/reply/error/event +
// the binary frame), the same one transport.js + ws_bridge speak — no new
// dialect.
//
// A call frame is `{type:"call", target, payload, id, caller?, secret?}` in BOTH
// directions. `secret` is the batch item's marks — dotted strings
// (`payload.<field…>` / `result[.<field…>]`) and/or arrays of steps
// (`["payload", "a.b"]`: a string step a literal key, an int step a list index),
// `kernel/secret.ts` — carried verbatim: a host-bound send puts its item's marks
// on the frame (only when it marked some — never `[]`), and a host's call is
// dispatched here WITH the frame's marks, so whichever kernel receives the call
// records `<secret>` exactly as the caller does. The reply carries the real
// result back. Nothing here logs or keeps a frame.
//
// FEDERATION ROUTING. The `event` frame carries no source id (the proxy mirrors
// every watched inbox into one undifferentiated stream). So instead of one
// muxed socket, we keep ONE WS PER WATCHED UPSTREAM: dialing `/<src>/ws`
// auto-watches `src` server-side, so every event on that socket is unambiguously
// `src`'s → kernel.emit(src, …). This is exactly how the browser works today
// (each iframe = one page = one WS to its own agent). Calls ride the endpoint's
// own socket when one exists, else a lazy control socket; replies correlate by id.

/** Minimal surface common to the browser `WebSocket` and node's global one. */
export interface WebSocketLike {
  binaryType: string;
  send(data: string | ArrayBuffer): void;
  close(): void;
  onopen: ((ev: unknown) => void) | null;
  onclose: ((ev: unknown) => void) | null;
  onerror: ((ev: unknown) => void) | null;
  onmessage: ((ev: { data: unknown }) => void) | null;
}
export type SocketFactory = (url: string) => WebSocketLike;

export interface BridgeOptions {
  /** WS origin, e.g. "ws://127.0.0.1:8888" (no trailing slash needed). */
  origin: string;
  /** Agent id whose socket carries calls to targets without their own socket. */
  controlEndpoint: string;
  /** Inject a socket constructor (tests); defaults to the global WebSocket. */
  makeSocket?: SocketFactory;
  /** Reconnect backoff ceiling, ms (default 16000). */
  maxReconnectDelay?: number;
}

interface Pending {
  resolve: (value: Json) => void;
  reject: (err: Error) => void;
}

/** One auto-reconnecting socket to `/<endpoint>/ws`; its events are `endpoint`'s. */
class Conn {
  readonly endpoint: string;
  private readonly url: string;
  private readonly make: SocketFactory;
  private readonly maxDelay: number;
  private readonly onEvent: (src: string, payload: Payload) => void;

  private ws: WebSocketLike | null = null;
  private connected = false;
  private closedByUs = false;
  /**
   * Per-CONNECTION correlator prefix. Minted once, never reused.
   *
   * MEASURED 2026-08-27 against the real python kernel: this used to be a bare
   * counter (`"1"`, `"2"`, …), and the host now treats a frame's `id` as the
   * EXECUTION id (#38) — the ledger's dedup/join key. Two browser tabs each
   * open a socket, each start at `"1"`, and the second tab's very first call
   * came back
   *
   *     {"error": "send: id '1' already names an execution on 'alpha',
   *                not 'beta'", "reason": "bad_arg"}
   *
   * — refused outright. Aimed at the SAME target it was worse than refused: it
   * silently JOINED the other tab's execution and returned that tab's result.
   *
   * A correlator must therefore be unique across every socket, tab and reload
   * that can reach one host. It is a STRING — on the host, the execution's
   * NAME (message ids are the kernel's ints) — minted as `{prefix}-{n}`.
   */
  private readonly idPrefix = mintPrefix();
  private nextId = 1;
  private delay = 1000;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private readonly lifecycle = new Set<(state: "connected" | "disconnected") => void>();
  // host state stream (telemetry) rides this conn when it's the control conn
  private wantsState = false;
  private onState: ((frame: Envelope) => void) | null = null;
  private readonly pending = new Map<string, Pending>();
  // THE PEER ROLE (cross-kernel addressing, owner 2026-09-30): the conn that says
  // `hello {guid}` becomes this kernel's address on the host — the host's calls to
  // `<guid>|<agent>` arrive on it as `call` frames and are answered by `reply`.
  // The guid is the tree's: every tab of one identity says hello with it, and the
  // host holds them as ONE principal (a call goes to all, the first reply wins).
  private peerGuid: (() => string | null) | null = null;
  private onCall: ((frame: Envelope) => void) | null = null;
  // the host EMITTING on one of this kernel's agents, and a reply another tab of
  // this identity got (`reply_mirror`) — both only on the peer conn
  private onPeerEmit: ((frame: Envelope) => void) | null = null;
  private onMirror: ((frame: Envelope) => void) | null = null;
  private ready!: Promise<void>;
  private resolveReady: (() => void) | null = null;

  constructor(
    endpoint: string,
    url: string,
    make: SocketFactory,
    maxDelay: number,
    onEvent: (src: string, payload: Payload) => void,
  ) {
    this.endpoint = endpoint;
    this.url = url;
    this.make = make;
    this.maxDelay = maxDelay;
    this.onEvent = onEvent;
    this.arm();
    this.connect();
  }

  private arm(): void {
    this.ready = new Promise<void>((res) => {
      this.resolveReady = res;
    });
  }

  private connect(): void {
    if (this.closedByUs) return;
    const ws = this.make(this.url);
    ws.binaryType = "arraybuffer";
    this.ws = ws;
    ws.onopen = () => {
      this.connected = true;
      this.delay = 1000;
      // `/<endpoint>/ws` auto-watches `endpoint`; replay the state stream
      // subscription so a reconnect resyncs (a fresh snapshot follows).
      if (this.wantsState) this.sendFrame({ type: "state_subscribe" });
      // a reconnect is a new connection: the host forgot this address with it
      this.announce();
      const r = this.resolveReady;
      this.resolveReady = null;
      if (r !== null) r();
      for (const l of this.lifecycle) {
        try {
          l("connected");
        } catch {
          /* a listener throwing must not break the others */
        }
      }
    };
    ws.onmessage = (ev) => this.onMessage(ev.data);
    ws.onerror = () => {
      /* a close event follows; handle there */
    };
    ws.onclose = () => {
      this.connected = false;
      this.rejectAll(new Error("disconnected"));
      for (const l of this.lifecycle) {
        try {
          l("disconnected");
        } catch {
          /* keep notifying the rest */
        }
      }
      if (this.closedByUs) return;
      this.arm();
      this.reconnectTimer = setTimeout(() => {
        this.reconnectTimer = null;
        this.connect();
      }, this.delay);
      this.delay = Math.min(this.delay * 2, this.maxDelay);
    };
  }

  private onMessage(data: unknown): void {
    // A malformed frame is SKIPPED, loudly, and the socket kept (python
    // `ws_bridge`: "skipping malformed frame") — never silently.
    let msg: Envelope;
    try {
      msg = decodeFrame(data as string | ArrayBuffer);
    } catch (e) {
      warn(`ws_bridge: skipping malformed frame: ${e instanceof Error ? e.message : String(e)}`);
      return;
    }
    if (msg === null || typeof msg !== "object" || Array.isArray(msg)) {
      warn("ws_bridge: skipping non-object frame");
      return;
    }
    const type = msg["type"];
    if (type === "reply" || type === "error") {
      const p = this.pending.get(String(msg["id"]));
      if (p === undefined) return;
      this.pending.delete(String(msg["id"]));
      if (type === "error") p.reject(new Error(String(msg["error"] ?? "error")));
      else p.resolve((msg["data"] ?? null) as Json);
    } else if (type === "call") {
      // a HOST calling one of THIS kernel's agents (only on the peer conn)
      this.onCall?.(msg);
    } else if (type === "emit") {
      this.onPeerEmit?.(msg);
    } else if (type === "reply_mirror") {
      // never resolves a pending call: the call was another tab's
      this.onMirror?.(msg);
    } else if (type === "event") {
      const payload = msg["payload"];
      if (payload !== null && typeof payload === "object") {
        this.onEvent(this.endpoint, payload as Payload);
      }
    } else if (type === "state_snapshot" || type === "state_event") {
      // kernel telemetry stream — top-level frames, NOT event-wrapped
      this.onState?.(msg);
    }
  }

  private rejectAll(err: Error): void {
    const snapshot = [...this.pending.values()];
    this.pending.clear();
    for (const p of snapshot) {
      try {
        p.reject(err);
      } catch {
        /* listener threw; keep rejecting the rest */
      }
    }
  }

  async call(
    envelopePayload: Envelope,
    target: string,
    caller?: string,
    secret?: readonly Mark[],
  ): Promise<Json> {
    await this.ready;
    if (!this.connected || this.ws === null) throw new Error("disconnected");
    const id = `${this.idPrefix}-${this.nextId++}`;
    // WHO IS CALLING, QUALIFIED: `<this kernel's guid>|<the agent that asked>`
    // (`kernel` for the operator) — the far side keys what the call writes by
    // it. Absent until this kernel has a guid (its root has booted).
    const env: Envelope = { type: "call", target, payload: envelopePayload, id };
    if (caller !== undefined) env["caller"] = caller;
    // the item's marks, so the FAR kernel masks its records of this call too
    if (secret !== undefined && secret.length > 0) env["secret"] = [...secret];
    const frame = encodeFrame(env);
    return new Promise<Json>((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      try {
        this.ws!.send(frame.data);
      } catch (e) {
        this.pending.delete(id);
        reject(e instanceof Error ? e : new Error(String(e)));
      }
    });
  }

  /** Send a raw top-level frame; returns false if not connected. */
  private sendFrame(env: Envelope): boolean {
    if (!this.connected || this.ws === null) return false;
    try {
      this.ws.send(encodeFrame(env).data);
      return true;
    } catch {
      return false;
    }
  }

  /** Make this conn the kernel's peer address: say `hello` whenever there is a
   *  guid to say, and hand inbound `call` frames to `onCall`. */
  asPeer(
    guid: () => string | null,
    onCall: (frame: Envelope) => void,
    onEmit: (frame: Envelope) => void,
    onMirror: (frame: Envelope) => void,
  ): void {
    this.peerGuid = guid;
    this.onCall = onCall;
    this.onPeerEmit = onEmit;
    this.onMirror = onMirror;
    this.announce();
  }

  /** Say who this kernel is, if it knows yet (its root mints the guid at boot).
   *  Sent on every (re)connect and when the guid first appears. */
  announce(): void {
    const g = this.peerGuid?.() ?? null;
    if (g !== null) this.sendFrame({ type: "hello", guid: g });
  }

  /** Answer one host call: its outcome's result, whatever it was (a refusal too). */
  answer(id: string, data: Json): void {
    this.sendFrame({ type: "reply", id, data });
  }

  /** Fire-and-forget emit; dropped if not connected (no buffering of emits). */
  emit(envelopePayload: Envelope, target: string): void {
    this.sendFrame({ type: "emit", target, payload: envelopePayload });
  }

  /** Register a connect/disconnect listener; returns an unsubscribe fn. */
  onLifecycle(fn: (state: "connected" | "disconnected") => void): () => void {
    this.lifecycle.add(fn);
    return () => this.lifecycle.delete(fn);
  }

  /** Route this conn's state_snapshot/state_event frames to `sink`. */
  setStateSink(sink: ((frame: Envelope) => void) | null): void {
    this.onState = sink;
  }

  /** Turn the host state stream on/off on this conn (sends subscribe now if
   *  connected). Conversation rows are not on that stream: an AI agent announces
   *  its own rows to its watchers (`watch` on that agent, gated there). */
  setWantsState(on: boolean): void {
    if (this.wantsState === on) return;
    this.wantsState = on;
    this.sendFrame(on ? { type: "state_subscribe" } : { type: "state_unsubscribe" });
  }

  close(): void {
    this.closedByUs = true;
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    this.rejectAll(new Error("closed"));
    try {
      this.ws?.close();
    } catch {
      /* already closing */
    }
  }
}

function warn(msg: string): void {
  (globalThis as { console?: { warn(m: string): void } }).console?.warn(msg);
}

/** A short unguessable per-connection prefix, so correlators from different
 *  tabs/reloads can never collide in the host's execution ledger. */
function mintPrefix(): string {
  const c = (globalThis as {
    crypto?: { getRandomValues?: (a: Uint8Array) => Uint8Array };
  }).crypto;
  if (c !== undefined && typeof c.getRandomValues === "function") {
    const b = new Uint8Array(6);
    c.getRandomValues(b);
    return [...b].map((x) => x.toString(16).padStart(2, "0")).join("");
  }
  // No CSPRNG here: still must not collide, so mix time with randomness rather
  // than falling back to a counter that starts at 1 in every tab.
  return `${Date.now().toString(36)}${Math.floor(Math.random() * 0xffffff).toString(36)}`;
}

export class WsBridge implements Bridge {
  private readonly kernel: Kernel;
  private readonly origin: string;
  private readonly controlEndpoint: string;
  private readonly make: SocketFactory;
  private readonly maxDelay: number;
  private readonly conns = new Map<string, Conn>();
  private readonly stateHandlers = new Set<(frame: Envelope) => void>();

  constructor(kernel: Kernel, opts: BridgeOptions) {
    this.kernel = kernel;
    this.origin = opts.origin.replace(/\/+$/, "");
    this.controlEndpoint = opts.controlEndpoint;
    this.maxDelay = opts.maxReconnectDelay ?? 16000;
    this.make = opts.makeSocket ?? defaultSocketFactory();
    kernel.bridge = this;
    // The guid appears when the root boots — possibly after the control socket
    // is open. Say hello then, on the conn that already exists.
    kernel.addStateSubscriber((e) => {
      const changed = (e as { changed?: unknown }).changed;
      if (e.kind === "updated" && Array.isArray(changed) && changed.includes("guid")) {
        this.conns.get(this.controlEndpoint)?.announce();
      }
    });
  }

  private connFor(endpoint: string): Conn {
    let c = this.conns.get(endpoint);
    if (c === undefined) {
      c = new Conn(
        endpoint,
        `${this.origin}/${endpoint}/ws`,
        this.make,
        this.maxDelay,
        (src, payload) => this.kernel.emit(src, payload),
      );
      this.conns.set(endpoint, c);
      if (endpoint === this.controlEndpoint) {
        const conn = c;
        conn.asPeer(
          () => this.kernel.guid,
          (frame) => void this.answerCall(conn, frame),
          (frame) => this.deliverEmit(frame),
          (frame) =>
            // TODO (owner, open): DESYNC between the tabs of one identity is
            // envisioned and unsolved — the mirror shows another tab's reply, it
            // reconciles nothing. Think later.
            this.kernel.emit(conn.endpoint, {
              type: "reply_mirror",
              id: (frame["id"] ?? null) as Json,
              data: (frame["data"] ?? null) as Json,
            }),
        );
      }
    }
    return c;
  }

  /**
   * A HOST calling `frame.target` on THIS kernel as `frame.caller`
   * (`<host guid>|<agent>`). Dispatched through the ordinary send pipeline under
   * a FOREIGN origin: the target's `callable_by` is the whole local check (the
   * host already judged its sender's `can_call`), and only this kernel's own
   * agents are reachable. The frame's `secret` marks go on the item as they came.
   * The outcome's result — a refusal included — goes back as the `reply`.
   */
  private async answerCall(conn: Conn, frame: Envelope): Promise<void> {
    const id = String(frame["id"] ?? "");
    const caller = frame["caller"];
    const target = frame["target"];
    const payload = frame["payload"];
    if (
      typeof caller !== "string" ||
      !this.kernel.isForeign(caller) ||
      typeof target !== "string" ||
      payload === null ||
      typeof payload !== "object" ||
      Array.isArray(payload)
    ) {
      // A caller claiming THIS kernel's own guid would borrow a local agent's
      // grants; a call must name who is calling, from where.
      conn.answer(id, {
        error:
          "call: needs caller `<guid>|<agent>` of ANOTHER kernel, a target id and an object payload",
        reason: "bad_arg",
        arg: "caller",
      });
      return;
    }
    const item: Item = { target, payload: payload as Payload };
    // SECRET MARKS cross the leg verbatim (`kernel/secret.ts`): this kernel
    // records the call as shown, exactly as for a local caller — its own check
    // refuses a malformed mark (any JSON value the frame carried) before
    // anything is dispatched (python web_ws `_on_call`, io_bridge's inbound call).
    if (Object.hasOwn(frame, "secret")) item.secret = frame["secret"] as Mark[];
    const [out] = await this.kernel.foreignView(caller).send([item]);
    conn.answer(id, (out?.result ?? null) as Json);
  }

  /**
   * The HOST emitting on `frame.target` of THIS kernel — every tab of this
   * identity gets it (owner, 2026-10-03). Gated exactly like a host call: the
   * target's `callable_by` against the qualified `frame.caller`, verb =
   * `payload.type`, and only this kernel's own agents. A frame with no `caller`
   * is the host's operator (python `Agent.emit` with no sender). A refused or
   * malformed frame is dropped — an emit has no reply.
   */
  private deliverEmit(frame: Envelope): void {
    const caller = frame["caller"];
    const target = frame["target"];
    const payload = frame["payload"];
    if (
      typeof target !== "string" ||
      payload === null ||
      typeof payload !== "object" ||
      Array.isArray(payload) ||
      typeof (payload as Payload)["type"] !== "string" ||
      (caller !== undefined && (typeof caller !== "string" || !this.kernel.isForeign(caller)))
    ) {
      warn("ws_bridge: dropping malformed emit frame");
      return;
    }
    // TODO (owner, open): DESYNC between the tabs of one identity is envisioned
    // and unsolved — each tab gets the event, nothing reconciles what they do
    // with it. Think later.
    const id = this.kernel.resolveId(target);
    if (!this.kernel.agents.has(id)) return;
    const verb = (payload as Payload)["type"] as string;
    if (caller !== undefined && this.kernel.ingressGate(caller as string, verb, id) !== null) return;
    this.kernel.emit(id, payload as Payload);
  }

  /** Make this kernel addressable from the host now: open the control socket
   *  (the peer conn) — a page that only listens would otherwise never say hello. */
  announce(): void {
    this.connFor(this.controlEndpoint).announce();
  }

  /** Forward a call to `target`: its own socket if watched, else the control one.
   *  `secret` — the item's marks — rides the frame when there are some. */
  forward(target: string, payload: Payload, caller?: string, secret?: readonly Mark[]): Promise<Json> {
    const conn = this.conns.get(target) ?? this.connFor(this.controlEndpoint);
    return conn.call(payload, target, caller, secret);
  }

  /** Forward a call whose payload carries a binary blob (e.g. paste_image);
   *  `secret` rides the frame's JSON head as on `forward`. */
  forwardBinary(target: string, payload: Envelope, caller?: string, secret?: readonly Mark[]): Promise<Json> {
    const conn = this.conns.get(target) ?? this.connFor(this.controlEndpoint);
    return conn.call(payload, target, caller, secret);
  }

  /** Fire-and-forget emit to a host agent's inbox (e.g. reload_html). */
  emitRemote(target: string, payload: Payload): void {
    const conn = this.conns.get(target) ?? this.connFor(this.controlEndpoint);
    conn.emit(payload, target);
  }

  watchRemote(src: string): void {
    this.connFor(src); // dialing /<src>/ws auto-watches src; events → emit(src)
  }

  unwatchRemote(src: string): void {
    if (src === this.controlEndpoint) return; // keep the control socket for calls
    const c = this.conns.get(src);
    if (c !== undefined) {
      c.close();
      this.conns.delete(src);
    }
  }

  /** connect/disconnect notifications for an endpoint's socket. */
  onLifecycle(
    endpoint: string,
    fn: (state: "connected" | "disconnected") => void,
  ): () => void {
    return this.connFor(endpoint).onLifecycle(fn);
  }

  /** Subscribe to the host kernel state stream (telemetry). Rides the control
   *  socket: first subscriber opens the stream, last one closes it. */
  subscribeState(handler: (frame: Record<string, unknown>) => void): () => void {
    const control = this.connFor(this.controlEndpoint);
    if (this.stateHandlers.size === 0) {
      control.setStateSink((frame) => {
        for (const h of this.stateHandlers) {
          try {
            h(frame);
          } catch {
            /* keep dispatching */
          }
        }
      });
      control.setWantsState(true);
    }
    this.stateHandlers.add(handler);
    return () => {
      this.stateHandlers.delete(handler);
      if (this.stateHandlers.size === 0) {
        control.setWantsState(false);
        control.setStateSink(null);
      }
    };
  }

  close(): void {
    for (const c of this.conns.values()) c.close();
    this.conns.clear();
  }
}

function defaultSocketFactory(): SocketFactory {
  const Ctor = (globalThis as { WebSocket?: new (url: string) => WebSocketLike })
    .WebSocket;
  if (Ctor === undefined) {
    return () => {
      throw new Error("no global WebSocket available; pass opts.makeSocket");
    };
  }
  return (url) => new Ctor(url);
}
