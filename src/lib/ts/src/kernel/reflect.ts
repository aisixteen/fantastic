import type { Agent } from "./agent.ts";
import type { Json, Payload } from "./json.ts";
import type { Kernel } from "./kernel.ts";

// Uniform `reflect`, mirroring python's `_reflect_identity` +
// `_apply_reflect_flags`. This is a CROSS-LANGUAGE CONTRACT: the shape here
// must match the python/rust/swift kernels' reflect output. tests/reflect.test.ts
// pins it against a shared fixture so the two implementations can't drift.

export interface BundleInfo {
  name: string;
  handler_module: string;
}

const ROOT_SENTENCE =
  "Fantastic kernel. Everything is reachable by sending messages to agents.";
const BARE_SENTENCE =
  "Bare agent (no handler_module) — answers substrate verbs only.";

/** The identity block every reflect starts from. */
export function reflectIdentity(agent: Agent, rootId: string | null): Payload {
  const isRoot = agent.id === rootId || agent.parentId === null;
  const obj: Payload = {
    id: agent.id,
    sentence: agent.sentence ?? (isRoot ? ROOT_SENTENCE : BARE_SENTENCE),
    parent_id: agent.parentId,
    handler_module: agent.handlerModule,
    display_name: agent.displayName,
  };
  if (agent.description !== null) obj["description"] = agent.description;
  if (agent.verbs !== null) obj["verbs"] = agent.verbs;
  for (const [k, v] of Object.entries(agent.meta)) {
    if (!(k in obj)) obj[k] = v;
  }
  return obj;
}

/**
 * Post-process a reflect reply with the composable flags:
 *   tree    = all (default) | ids | none
 *   bundles = none (default) | all | ids
 *   readme  = false (default) | true
 * Plus: surface `description` if the agent has one and the reply omitted it.
 */
export function applyReflectFlags(
  kernel: Kernel,
  target: Agent,
  payload: Payload,
  reply: Json,
): Json {
  if (reply === null || typeof reply !== "object" || Array.isArray(reply)) {
    return reply;
  }
  const obj = reply as Payload;

  if (!("description" in obj) && target.description !== null) {
    obj["description"] = target.description;
  }

  // Kernel runtime identity + deployment context — surfaced on the ROOT
  // reflect so a client that hops to this kernel learns, in one round-trip:
  // which runtime (`runtime`), WHERE it runs (`env`), and which build
  // (`version`). This kernel runs in the BROWSER — it has no OS env, so env is
  // always "host" and version null (a browser frontend is never the
  // containerized host; the host it federates to reports its own env/version).
  // Same field names + key order (runtime → env → version) as the other
  // runtimes, which read these from FANTASTIC_ENV / FANTASTIC_VERSION.
  if (target.parentId === null) {
    obj["runtime"] = "ts";
    obj["env"] = "host";
    obj["version"] = null;
    // WHICH provider the frontend persists records THROUGH — for this browser
    // kernel that's the host's loader (the `web_loader` alias), reached over the
    // bridge by the proxy_loader (cross-kernel delegation). Mirrors py's root
    // `persistence:{provider}`; `null` when no loader is wired.
    obj["persistence"] = { provider: kernel.persistenceProvider };
  }

  const tree = flag(payload["tree"], "all");
  if (tree === "all") obj["tree"] = treeNode(target);
  else if (tree === "ids") obj["tree"] = descendantIds(target);

  const bundles = flag(payload["bundles"], "none");
  if (bundles === "all") {
    obj["bundles"] = kernel.availableBundles() as unknown as Json;
  } else if (bundles === "ids") {
    obj["bundles"] = kernel.availableBundles().map((b) => b.name);
  }

  if (payload["readme"] === true) {
    const rd = target.meta["readme"];
    obj["readme"] =
      typeof rd === "string"
        ? rd
        : target.handlerModule !== null
          ? (kernel.bundleReadme(target.handlerModule) ?? null)
          : null;
  }

  return obj;
}

function flag(v: Json | undefined, fallback: string): string {
  return typeof v === "string" ? v : fallback;
}

// Wiring/posture meta surfaced inline in the distilled tree (mirrors python's
// `_POSTURE_KEYS` in kernel/_agent.py), so a client reads the IO landscape — which
// legs are open vs sealed-by-default, what a consumer persists through — from ONE
// root reflect. Same keys + order across runtimes.
const POSTURE_KEYS = [
  "ingress_rule",
  "egress_rule",
  "auth",
  "root",
  "file_bridge_id",
] as const;

function treeNode(agent: Agent): Payload {
  const obj: Payload = {
    id: agent.id,
    parent_id: agent.parentId,
    handler_module: agent.handlerModule,
    display_name: agent.displayName,
  };
  if (agent.description !== null) obj["description"] = agent.description;
  for (const k of POSTURE_KEYS) {
    const v = agent.meta[k];
    if (v !== undefined) obj[k] = v;
  }
  obj["children"] = sortedChildren(agent).map(treeNode);
  return obj;
}

function descendantIds(agent: Agent): string[] {
  const out: string[] = [agent.id];
  for (const child of sortedChildren(agent)) {
    out.push(...descendantIds(child));
  }
  return out;
}

function sortedChildren(agent: Agent): Agent[] {
  return [...agent.children.values()].sort((a, b) =>
    a.id < b.id ? -1 : a.id > b.id ? 1 : 0,
  );
}
