import type { Json, Payload } from "./json.ts";
import type { Kernel } from "./kernel.ts";
import type { AgentRecord } from "./state.ts";

// A local handler answers an agent's DOMAIN verbs (render, write, resize, …).
// It never answers `reflect` — the kernel answers that uniformly for every
// agent (see reflect.ts), so reflect can't drift handler-by-handler.
export type Handler = (
  id: string,
  payload: Payload,
  kernel: Kernel,
) => Promise<Json> | Json;

export interface AgentInit {
  id: string;
  /** Names the JS view bundle (local) or a host bundle (forwarded). */
  handlerModule?: string | null;
  parentId?: string | null;
  /** One-line "what am I" — surfaced by reflect; defaults per root/bare. */
  sentence?: string | null;
  /** verb -> one-line doc; surfaced by reflect when present. */
  verbs?: { [verb: string]: string } | null;
  /** Free-form identity fields surfaced by reflect (display_name, description, …). */
  meta?: Payload;
  /** Local dispatch fn; null = bare or host-forwarded. */
  handler?: Handler | null;
}

/**
 * A node in the frontend kernel's tree. Either a local view-agent (has a
 * `handler`), a bare local agent (reflect-only), or a stand-in for a host
 * agent (no handler — sends to it are forwarded over the bridge).
 */
export class Agent {
  id: string;
  handlerModule: string | null;
  parentId: string | null;
  sentence: string | null;
  verbs: { [verb: string]: string } | null;
  meta: Payload;
  handler: Handler | null;
  children: Map<string, Agent>;

  constructor(init: AgentInit) {
    this.id = init.id;
    this.handlerModule = init.handlerModule ?? null;
    this.parentId = init.parentId ?? null;
    this.sentence = init.sentence ?? null;
    this.verbs = init.verbs ?? null;
    this.meta = init.meta ?? {};
    this.handler = init.handler ?? null;
    this.children = new Map();
  }

  get displayName(): string {
    const v = this.meta["display_name"];
    return typeof v === "string" ? v : this.id;
  }

  get description(): string | null {
    const v = this.meta["description"];
    return typeof v === "string" ? v : null;
  }

  /** The persistent record — id + handler_module + parent_id + meta, in the
   *  snake_case wire/disk shape a loader persists. Mirrors python
   *  `Agent.record`. */
  record(): AgentRecord {
    const rec: AgentRecord = { id: this.id };
    if (this.handlerModule !== null) rec.handler_module = this.handlerModule;
    if (this.parentId !== null) rec.parent_id = this.parentId;
    Object.assign(rec, this.meta);
    return rec;
  }
}
