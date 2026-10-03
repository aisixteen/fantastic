import type { Agent } from "./agent.ts";
import { KERNEL_VERB_DOCS } from "./kernel_verbs.ts";
import type { Json, Payload } from "./json.ts";
import type { Kernel } from "./kernel.ts";
import { page } from "./paging.ts";

// Uniform `reflect`, a port of python's `_reflect_identity` + `_roster` +
// `_apply_reflect_flags` (kernel/_agent.py; rust `reflect.rs` and swift match it).
// This is a CROSS-LANGUAGE CONTRACT: tests/reflect.test.ts pins it against a
// fixture captured from the python kernel's own replies.
//
// THE DEFAULT IS DISTILLED. It answers the question a caller has on most
// messages — who do I send to? — with a ROSTER `{id, sentence, description?,
// mode?, children}`, `kernel_verbs` cut to what precedes each doc's first
// em-dash, and a `more` line naming the escalation. `detail:"full"` is the
// orientation view: the WIRING tree (handler_module + posture on every node)
// and every verb doc whole. The full view is not a superset — it carries no
// `sentence` — so it is asked for once, not by default.

export interface BundleInfo {
  name: string;
  handler_module: string;
}

const ROOT_SENTENCE =
  "Fantastic kernel. Everything is reachable by sending messages to agents.";
const BARE_SENTENCE =
  "Bare agent (no handler_module) — answers substrate verbs only.";

/** The escalation line every DISTILLED reflect carries — python `_MORE_HINT`,
 *  verbatim (rust and swift carry the same bytes). */
export const MORE_HINT =
  "Distilled: every agent and what it is FOR. Reflect one agent for its verbs " +
  "(<type>reflect</type>). Manual: <type>reflect</type> <readme>index</readme> - keys " +
  "include system-verbs (ps/cancel/create_agent args) and self-extension (author your " +
  "own durable tools). <detail>full</detail> on a reflect adds wiring and posture - ask " +
  "for it only when you need to see how agents are plumbed, not to find one.";

/** PURPOSE, not wiring — the reflect keys a roster node copies beyond its
 *  sentence (python `_PURPOSE_KEYS`). */
const PURPOSE_KEYS = ["mode"] as const;

/** Wiring/posture meta on every node of the FULL tree (python `_POSTURE_KEYS`):
 *  a client reads the IO landscape — which legs are open vs sealed, what a
 *  consumer persists through, who may call whom — from ONE root reflect. */
const POSTURE_KEYS = [
  "ingress_rule",
  "egress_rule",
  "auth",
  "root",
  "allow",
  "can_call",
  "callable_by",
] as const;

/** python `display_name or id` — an empty name is no name. */
function displayName(agent: Agent): string {
  return agent.displayName || agent.id;
}

/** The identity block every reflect starts from: id + sentence + record
 *  fields + flat meta. This kernel answers reflect for EVERY agent (a bundle
 *  never builds its own), so a record's `sentence`/`verbs` are read here. */
export function reflectIdentity(agent: Agent, rootId: string | null): Payload {
  const isRoot = agent.id === rootId || agent.parentId === null;
  const obj: Payload = {
    id: agent.id,
    sentence: agent.sentence ?? (isRoot ? ROOT_SENTENCE : BARE_SENTENCE),
    parent_id: agent.parentId,
    handler_module: agent.handlerModule,
    display_name: displayName(agent),
  };
  if (agent.description !== null) obj["description"] = agent.description;
  if (agent.verbs !== null) obj["verbs"] = agent.verbs;
  for (const [k, v] of Object.entries(agent.meta)) {
    if (!(k in obj)) obj[k] = v;
  }
  return obj;
}

/** `kernel_verbs` at a resolution: FULL carries every doc whole, in order;
 *  DISTILLED carries every verb, sorted, cut at its doc's first em-dash — the
 *  docs state the consequence before it. The `_` line is never cut. */
export function kernelVerbsView(full: boolean): Payload {
  const { _: preamble, ...docs } = KERNEL_VERB_DOCS;
  const out: Payload = { _: preamble as string };
  const names = Object.keys(docs);
  if (!full) names.sort();
  for (const name of names) {
    const doc = docs[name] as string;
    out[name] = full ? doc : (doc.split("—")[0] as string).trim();
  }
  return out;
}

/**
 * Compose any reflect reply with the universal flags (python
 * `_apply_reflect_flags`), keys in python's order: identity, `description`,
 * the gate fields, `kernel_verbs`, the root frame, `tree`, `next`, `more`,
 * `bundles`, `bundles_next`, `readme`.
 *   detail  = distilled (default) | full
 *   tree    = all (default) | ids — decided by KEY PRESENCE: absent is `all`,
 *             any other present value (`none`, null, …) omits it; `ids` pages.
 *   bundles = all | ids (default none) — the catalog, paged (`bundles_next`)
 *   readme  = true (the agent's readme, or null) | "index" | "<key>"
 *
 * `kernel` is the CALLER's view: the distilled roster is gated per node on its
 * `currentOrigin`, exactly as python gates on `_current_sender`.
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
  // A REFUSAL IS NOT A REFLECT: decorated, it reads like a healthy reflect with
  // a stray error field.
  if ("error" in obj) return obj;
  const full = payload["detail"] === "full";

  if (!("description" in obj) && target.description !== null) {
    obj["description"] = target.description;
  }
  // YOUR OWN AUTHORITY, on your own reflect — absent means baseline only.
  for (const gate of ["can_call", "callable_by"]) {
    if (gate in target.meta && !(gate in obj)) obj[gate] = target.meta[gate] as Json;
  }
  // #26 — advertised on EVERY agent, not just the root. Root-only was tried and
  // REVERTED: a model that never reflected the root never saw the signatures.
  obj["kernel_verbs"] = kernelVerbsView(full);

  // Kernel runtime identity + deployment context, on the ROOT only. This kernel
  // runs in the BROWSER — no OS env, so env is always "host" and version null.
  // Same field names + key order (runtime → peers → env → version) as the others.
  // `peers` are the kernels reachable from here as `<guid>|<agent>`: this kernel
  // registers none (the host it federates to is reached by bare id over the
  // bridge), so the list is always empty — present, as everywhere.
  if (target.parentId === null) {
    obj["runtime"] = "ts";
    obj["peers"] = [];
    obj["env"] = "host";
    obj["version"] = null;
    // The loader-owned root frame (python's `reflect_root_extra`): WHICH provider
    // records persist THROUGH — the host loader the proxy_loader reaches — and
    // the kernel's instance `guid` (null until the root has booted).
    obj["persistence"] = { provider: kernel.persistenceProvider };
    obj["guid"] = kernel.guid;
  }

  const tree = "tree" in payload ? payload["tree"] : "all";
  if (tree === "all") {
    obj["tree"] = full ? treeNode(target, (a) => mayReflect(kernel, a)) : roster(kernel, target);
  } else if (tree === "ids") {
    // The FLAT id index is the one tree shape that is a plain list, so it is the
    // one that pages; the nested tree is never sliced (a page of it would be a
    // forest with no root).
    const p = page(descendantIds(target), payload);
    obj["tree"] = p.page;
    if (p.next !== null) obj["next"] = p.next;
  }
  if (!full) obj["more"] = MORE_HINT;

  const bundles = payload["bundles"];
  if (bundles === "all") {
    const p = page(kernel.availableBundles(), payload, (b) => b.name);
    obj["bundles"] = p.page as unknown as Json;
    if (p.next !== null) obj["bundles_next"] = p.next;
  } else if (bundles === "ids") {
    const p = page(kernel.availableBundles().map((b) => b.name), payload);
    obj["bundles"] = p.page;
    if (p.next !== null) obj["bundles_next"] = p.next;
  }

  const rm = payload["readme"];
  if (rm === true) {
    const rd = target.meta["readme"];
    obj["readme"] =
      typeof rd === "string"
        ? rd
        : target.handlerModule !== null
          ? (kernel.bundleReadme(target.handlerModule) ?? null)
          : null;
  } else if (typeof rm === "string" && rm !== "") {
    obj["readme"] = readmeView(rm);
  }
  return obj;
}

/** python `_may_reflect`: may the caller now running `reflect` `a`? The one
 *  per-node gate both tree views apply (`roster`, and `treeNode` under
 *  `detail:"full"`); an operator (no origin) may reflect everything. */
function mayReflect(kernel: Kernel, a: Agent): boolean {
  const sender = kernel.currentOrigin;
  return sender === null || kernel.gate(sender, "reflect", a.id) === null;
}

/** python `_roster`: `{id, sentence?, description?, mode?, children}` — the tree
 *  carrying what each agent is FOR. GATED PER NODE: a caller allowed to reflect
 *  one agent must not read every descendant's sentence through the tree; a
 *  denied node keeps its `id` and `children` (the shape is public through
 *  `tree:"ids"`). An operator (no origin) sees everything. */
function roster(kernel: Kernel, a: Agent): Payload {
  const may = mayReflect(kernel, a);
  const raw: Payload = may ? reflectIdentity(a, kernel.rootId) : {};
  const node: Payload = { id: a.id };
  if (raw["sentence"]) node["sentence"] = raw["sentence"];
  if (may && a.description !== null) node["description"] = a.description;
  for (const k of PURPOSE_KEYS) {
    const v = raw[k];
    if (v !== undefined && v !== null) node[k] = v;
  }
  node["children"] = [...a.children.values()].map((c) => roster(kernel, c));
  return node;
}

/** python `_tree(details=False)`: the WIRING view, `detail:"full"` only —
 *  children in creation order, like python's dict walk. GATED PER NODE, as the
 *  roster is: the view carries every node's `description` and grant maps, so a
 *  caller allowed to reflect ONE agent read its whole subtree's — a depth-2
 *  agent its siblings', through its own parent (audit 2026-10-03, KV-3). A node
 *  `may` refuses keeps its `id` and `children` and nothing else. */
function treeNode(agent: Agent, may: (a: Agent) => boolean): Payload {
  const children = (): Payload[] => [...agent.children.values()].map((c) => treeNode(c, may));
  if (!may(agent)) return { id: agent.id, children: children() };
  const obj: Payload = {
    id: agent.id,
    parent_id: agent.parentId,
    handler_module: agent.handlerModule,
    display_name: displayName(agent),
  };
  if (agent.description !== null) obj["description"] = agent.description;
  for (const k of POSTURE_KEYS) {
    const v = agent.meta[k];
    if (v !== undefined) obj[k] = v;
  }
  obj["children"] = children();
  return obj;
}

/** Flat id index: self first, then every descendant (DFS, creation order). */
function descendantIds(agent: Agent): string[] {
  const out: string[] = [agent.id];
  for (const child of agent.children.values()) out.push(...descendantIds(child));
  return out;
}

/** python `repr()` of a str, for the messages that quote a caller's key. */
function pyRepr(s: string): string {
  if (s.includes("'") && !s.includes('"')) return `"${s}"`;
  return `'${s.replace(/\\/g, "\\\\").replace(/'/g, "\\'")}'`;
}

/** python `_readme_view` over THIS kernel's doc tree — which is empty: a record
 *  carries one readme string, never a keyed `docs/` tree. So `index` is `[]`
 *  and every key is refused the way python refuses a key its tree lacks. */
function readmeView(sel: string): Payload {
  if (sel === "index") return { index: [] };
  const safe = sel.replace(/^\/+|\/+$/g, "").replace(/\\/g, "/");
  if (safe === "" || safe.split("/").some((p) => p === "" || p === "." || p === "..")) {
    return { error: `bad key ${pyRepr(sel)}`, keys: [] };
  }
  return { error: `no doc ${pyRepr(sel)}`, keys: [] };
}
