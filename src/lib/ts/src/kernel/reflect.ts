import type { Agent } from "./agent.ts";
import { BAD_ARG, refusal } from "./refusal.ts";
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
// mode?, children}`. `detail:"full"` is the WIRING tree (handler_module +
// posture on every node). A reflect says what an agent IS, never how to call it
// (owner, 2026-10-05): verb docs are the `docs` kernel verb.

export interface BundleInfo {
  name: string;
  handler_module: string;
}

const ROOT_SENTENCE =
  "Fantastic kernel. Everything is reachable by sending messages to agents.";
const BARE_SENTENCE =
  "Bare agent (no handler_module) — answers substrate verbs only.";

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
 *  never builds its own), so a record's `sentence` is read here; its `verbs`
 *  are `docs`, never a reflect. */
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
  for (const [k, v] of Object.entries(agent.meta)) {
    if (!(k in obj)) obj[k] = v;
  }
  return obj;
}

/**
 * Compose any reflect reply with the universal flags (python
 * `_apply_reflect_flags`), keys in python's order: identity, `description`,
 * the gate fields, the root frame, `tree`, `next`, `bundles`, `bundles_next`,
 * `readme`. `show_subtrees` / `ignore_subtrees` cut the tree (python
 * `kernel/_docs.subtree_filter`).
 *   detail  = distilled (default) | full
 *   tree    = all (default) | ids — decided by KEY PRESENCE: absent is `all`,
 *             any other present value (`none`, null, …) omits it; `ids` pages.
 *   bundles = all | ids (default none) — the catalog, paged (`bundles_next`)
 *   readme  = true (the agent's readme, or null) | "index" | "<key>"
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
  if (tree === "all" || tree === "ids") {
    const filt = subtreeFilter(kernel, payload, target);
    if ("error" in filt) return filt;
    const { show, ignore } = filt as { show: Agent[]; ignore: Set<string> };
    const roots = show.length > 0 ? show : [target];
    if (tree === "all") {
      const nodes = roots.map((r) =>
        ignore.has(r.id) ? prunedStub(r) : full ? treeNode(r, ignore) : roster(kernel, r, ignore),
      );
      obj["tree"] = show.length > 0 ? nodes : (nodes[0] as Payload);
    } else {
      // The FLAT id index is the one tree shape that is a plain list, so it is the
      // one that pages; the nested tree is never sliced (a page of it would be a
      // forest with no root). A pruned branch stands as its stub.
      const flat: Json[] = roots.flatMap((r) => descendantEntries(r, ignore));
      const p = page(flat, payload, (x) =>
        typeof x === "string" ? x : String((x as Payload)["id"]),
      );
      obj["tree"] = p.page;
      if (p.next !== null) obj["next"] = p.next;
    }
  }

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

/** python `_roster`: `{id, sentence?, description?, mode?, children}` — the tree
 *  carrying what each agent is FOR. A reflect that passed the gate returns the
 *  tree whole. */
function roster(kernel: Kernel, a: Agent, ignore: Set<string> = new Set()): Payload {
  const raw = reflectIdentity(a, kernel.rootId);
  const node: Payload = { id: a.id };
  if (raw["sentence"]) node["sentence"] = raw["sentence"];
  if (a.description !== null) node["description"] = a.description;
  for (const k of PURPOSE_KEYS) {
    const v = raw[k];
    if (v !== undefined && v !== null) node[k] = v;
  }
  node["children"] = [...a.children.values()].map((c) =>
    ignore.has(c.id) ? prunedStub(c) : roster(kernel, c, ignore),
  );
  return node;
}

/** python `_tree(details=False)`: the WIRING view, `detail:"full"` only —
 *  children in creation order, like python's dict walk. Whole, as the roster. */
function treeNode(agent: Agent, ignore: Set<string> = new Set()): Payload {
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
  obj["children"] = [...agent.children.values()].map((c) =>
    ignore.has(c.id) ? prunedStub(c) : treeNode(c, ignore),
  );
  return obj;
}

/** Flat id index: self first, then every descendant (DFS, creation order). */
function descendantIds(agent: Agent): string[] {
  const out: string[] = [agent.id];
  for (const child of agent.children.values()) out.push(...descendantIds(child));
  return out;
}

/** `descendantIds` as reply entries: an id in `ignore` stands as its stub. */
function descendantEntries(agent: Agent, ignore: Set<string>): Json[] {
  if (ignore.has(agent.id)) return [prunedStub(agent)];
  const out: Json[] = [agent.id];
  for (const child of agent.children.values()) out.push(...descendantEntries(child, ignore));
  return out;
}

/** What an ignored branch leaves: its id and how many agents it holds (itself
 *  included). python `_docs.pruned_stub`. */
function prunedStub(agent: Agent): Payload {
  return { id: agent.id, pruned: descendantIds(agent).length };
}

/** Is `agent` `ancestor` or below it? The parent-chain walk the gate uses. */
function within(kernel: Kernel, agent: Agent, ancestor: Agent): boolean {
  let a: Agent | undefined = agent;
  const seen = new Set<string>();
  while (a !== undefined && !seen.has(a.id)) {
    if (a.id === ancestor.id) return true;
    seen.add(a.id);
    a = a.parentId === null ? undefined : kernel.agents.get(a.parentId);
  }
  return false;
}

/** python `_docs.subtree_filter`: validate `show_subtrees` / `ignore_subtrees`.
 *  Every id must name an agent in `target`'s subtree; a shown id under another
 *  shown id is not repeated. */
function subtreeFilter(
  kernel: Kernel,
  payload: Payload,
  target: Agent,
): { show: Agent[]; ignore: Set<string> } | Payload {
  const lists: { [arg: string]: string[] } = {};
  for (const arg of ["show_subtrees", "ignore_subtrees"]) {
    const v = payload[arg];
    if (v === undefined || v === null) {
      lists[arg] = [];
      continue;
    }
    if (!Array.isArray(v) || !v.every((x) => typeof x === "string" && x !== "")) {
      return refusal(BAD_ARG, `reflect: ${arg} must be a list of agent ids`, { extra: { arg } });
    }
    for (const x of v as string[]) {
      const a = kernel.agents.get(x);
      if (a === undefined || !within(kernel, a, target)) {
        return refusal(
          BAD_ARG,
          `reflect: ${arg} names ${pyRepr(x)} — not an agent in ${target.id}'s tree`,
          { hint: "reflect with tree='ids' lists the ids", extra: { arg } },
        );
      }
    }
    lists[arg] = v as string[];
  }
  const shown = lists["show_subtrees"] as string[];
  const show: Agent[] = [];
  for (const x of [...new Set(shown)]) {
    const a = kernel.agents.get(x) as Agent;
    const nested = shown.some((o) => o !== x && within(kernel, a, kernel.agents.get(o) as Agent));
    if (!nested) show.push(a);
  }
  return { show, ignore: new Set(lists["ignore_subtrees"]) };
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
