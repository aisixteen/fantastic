import type { Bridge, Kernel } from "../kernel/kernel.ts";
import type { Json, Payload } from "../kernel/json.ts";
import { encodeFrame, decodeFrame } from "./frame.ts";
import type { Envelope } from "./frame.ts";

// The WS bridge: the frontend kernel's federation transport. Speaks the EXACT
// wire python web_ws/_proxy.py serves (call/emit/watch/unwatch/reply/error/event +
// the binary frame), the same one transport.js + ws_bridge speak — no new
// dialect.
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
  private nextId = 1;
  private delay = 1000;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private readonly lifecycle = new Set<(state: "connected" | "disconnected") => void>();
  // host state stream (telemetry) rides this conn when it's the control conn
  private wantsState = false;
  private onState: ((frame: Envelope) => void) | null = null;
  private readonly pending = new Map<string, Pending>();
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
    let msg: Envelope;
    try {
      msg = decodeFrame(data as string | ArrayBuffer);
    } catch {
      return;
    }
    const type = msg["type"];
    if (type === "reply" || type === "error") {
      const p = this.pending.get(String(msg["id"]));
      if (p === undefined) return;
      this.pending.delete(String(msg["id"]));
      if (type === "error") p.reject(new Error(String(msg["error"] ?? "error")));
      else p.resolve((msg["data"] ?? null) as Json);
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

  async call(envelopePayload: Envelope, target: string): Promise<Json> {
    await this.ready;
    if (!this.connected || this.ws === null) throw new Error("disconnected");
    const id = String(this.nextId++);
    const frame = encodeFrame({
      type: "call",
      target,
      payload: envelopePayload,
      id,
    });
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

  /** Turn the host state stream on/off on this conn (sends subscribe now if connected). */
  setWantsState(on: boolean): void {
    if (this.wantsState === on) return;
    this.wantsState = on;
    this.sendFrame({ type: on ? "state_subscribe" : "state_unsubscribe" });
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
    }
    return c;
  }

  /** Forward a call to `target`: its own socket if watched, else the control one. */
  forward(target: string, payload: Payload): Promise<Json> {
    const conn = this.conns.get(target) ?? this.connFor(this.controlEndpoint);
    return conn.call(payload, target);
  }

  /** Forward a call whose payload carries a binary blob (e.g. paste_image). */
  forwardBinary(target: string, payload: Envelope): Promise<Json> {
    const conn = this.conns.get(target) ?? this.connFor(this.controlEndpoint);
    return conn.call(payload, target);
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
