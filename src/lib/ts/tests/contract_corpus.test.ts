// THE AGENTIC CONTRACT CORPUS, replayed against the ts kernel.
//
// `contract/` states what this substrate claims as DATA — `requirements.yaml`
// (the registry), `agentic/configs` (deployable kernel shapes) and
// `agentic/specs` (ordered steps). Python, rust and swift each run it with their
// own runner; this is the ts one, a port of python's reference
// (`src/lib/python/tests/agentic/{_engine,_core}.py`). The files are READ at
// test time and never edited here.
//
// The six per-runtime primitives, in ts:
//   load_spec  — `src/kernel/yaml.ts` (the kernel's own strict subset reader)
//                + python's strict key checks, so a typo fails at load.
//   hydrate    — the ts kernel has NO manifest loader (its tree is built by its
//                bundles and persisted through the host loader), so the runner
//                plays static_loader's operator part: each loader becomes a child
//                of the root, each manifest entry a child of its loader, with its
//                declared meta (the kernel stamps and merges the baseline grants).
//   reboot     — the ts persistence path for real: a `ProxyLoader` persists every
//                record to a host-side store over the bridge, a NEW kernel reads
//                them back with `loadTree` and `load`s them.
//   dehydrate  — one root reflect (`detail: full`, `tree: all`), flattened.
//   reach      — a send made INSIDE the sender's own dispatch (`sendAs`: the
//                operator calls the sender, whose handler makes the send), which
//                the call gate sees as that agent; `operator` is a bare send
//                (ungated).
//   denied / denial_side — `reason: unauthorized` / its `side`.
//
// STAND-IN BUNDLES. The configs name host bundles (`file_bridge`, `text_file`,
// `exec_bridge`) and python's test bundle (`lifecycle_probe`), none of which a
// browser kernel ships. The runner registers stand-ins so the roster exists:
// thin EFFECT RECORDERS shaped like the real replies (a root-keyed in-memory
// file space; a text editor that writes THROUGH its `target_id` leg with a real,
// gated nested send; a `slow` verb that stays in flight). They carry no policy:
// the io seal, `readonly` and rule validation are NOT implemented, so no step
// that asserts one runs here.
//
// SKIPPED BY NAME, never silently (the README's per-runtime exemption rule):
//   (a) TREE CRUD — `create_agent` / `update_agent` / `delete_agent` belong to
//       the host root; this kernel answers none (the C21 driver skips the same).
//   (b) HOST IO BUNDLE POLICY — the io_bridge seal, `readonly`, what a leg's
//       `root` serves: properties of bundles this kernel does not ship; a
//       stand-in answering them would be measuring itself.
//   (c) static_loader — the hydration verdict channels and the manifest
//       re-applied over hydrated records at boot.
//   (d) a step whose premise is a skipped step.
// Everything not named must pass. The final test prints the coverage matrix and
// pins which requirements are applicable, partial and not applicable.

import { test } from "node:test";
import assert from "node:assert/strict";
import { isDeepStrictEqual } from "node:util";
import { readFileSync, readdirSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { Kernel } from "../src/kernel/kernel.ts";
import type { Bridge } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import type { Json, Payload } from "../src/kernel/json.ts";
import type { Item, Outcome } from "../src/kernel/outcome.ts";
import { pyRepr } from "../src/kernel/pyjson.ts";
import { MISSING_ARG, NOT_FOUND, UNHANDLED, refusal } from "../src/kernel/refusal.ts";
import { ProxyLoader } from "../src/bundles/loader/proxy_loader.ts";
import { parseYaml } from "../src/kernel/yaml.ts";
import { sendAs } from "./kit.ts";

const CONTRACT = fileURLToPath(new URL("../../../../contract/", import.meta.url));

/** This runtime's root id — `$root` in the corpus means exactly this. */
const ROOT_ID = "kernel_state";

// ─── the registry ──────────────────────────────────────────────────────────

interface Requirement {
  id: string;
  level: string;
  title: string;
}

const registry = parseYaml(readFileSync(CONTRACT + "requirements.yaml", "utf8"), "requirements.yaml") as {
  core: string[];
  requirements: Requirement[];
};
const REQUIREMENTS = new Map<string, Requirement>();
for (const r of registry.requirements) {
  if (REQUIREMENTS.has(r.id)) throw new Error(`requirements.yaml: duplicate id ${r.id}`);
  if (!["MUST", "SHOULD", "MAY"].includes(r.level)) throw new Error(`requirements.yaml: ${r.id} level ${r.level}`);
  REQUIREMENTS.set(r.id, r);
}
const CORE = new Set(registry.core);

/** What ran for each requirement: passing checks, and every skip with its why. */
const COVERAGE = new Map<string, { pass: number; skipped: string[] }>();
for (const id of REQUIREMENTS.keys()) COVERAGE.set(id, { pass: 0, skipped: [] });

/** Skipped steps that tag no requirement — reported, never silent. */
const UNTAGGED_SKIPS: string[] = [];

class StepFailure extends Error {}

function check(rid: string, cond: unknown, detail: string): void {
  if (!COVERAGE.has(rid)) throw new Error(`unknown requirement ${rid}`);
  if (!cond) throw new StepFailure(`[${rid}] ${detail}`);
  COVERAGE.get(rid)!.pass += 1;
}

function skip(rid: string, what: string, why: string): void {
  if (!COVERAGE.has(rid)) throw new Error(`unknown requirement ${rid}`);
  COVERAGE.get(rid)!.skipped.push(`${what} — ${why}`);
}

// ─── load_spec: strict, both files ─────────────────────────────────────────

const CONFIG_KEYS = new Set(["loaders"]);
const SPEC_KEYS = new Set(["config", "name", "description", "requirements", "expect"]);
const LOADER_KEYS = new Set(["id", "file", "manifest", "manifest_text", "missing_manifest", "meta"]);
const EXPECT_KEYS = new Set(["hydration", "error_contains", "reboot", "steps", "closed_roster_sender"]);
const STEP_VERBS = new Set([
  "reach", "operator", "grant", "revoke", "reboot", "assert_tree",
  "start", "await_task", "collect", "reconcile_grants", "sweep_undeclared",
]);
const STEP_META = new Set(["expect", "because", "name"]);
// `mentions` / `absent`: a string ANYWHERE / NOWHERE in the reply (bytes read as
// utf-8) (python `_engine._flat_text`).
const MATCHERS = new Set(["allow", "side", "reason", "status", "contains", "not_contains", "error_contains", "mentions", "absent"]);
const HYDRATIONS = new Set(["ok", "refused_at_write", "boot_error"]);
const MINTED_ID = /^[A-Za-z0-9_.]+_[0-9a-f]{6}$/;

type Obj = { [k: string]: unknown };

interface Step {
  n: number; // 1-based position in the spec
  kind: string;
  args: Obj;
  expect: Obj;
  because: string;
  name: string;
}

interface LoaderSpec {
  id: string;
  file: string;
  manifest: Obj | null;
  meta: Obj;
}

interface Config {
  name: string;
  requirements: string[];
  loaders: LoaderSpec[];
  hydration: string;
  errorContains: string;
  reboot: boolean;
  steps: Step[];
  closedRosterSender: string | null;
}

function isObj(v: unknown): v is Obj {
  return v !== null && typeof v === "object" && !Array.isArray(v);
}

function strict(where: string, got: unknown, allowed: Set<string>): Obj {
  if (!isObj(got)) throw new Error(`${where}: must be a mapping`);
  const extra = Object.keys(got).filter((k) => !allowed.has(k));
  if (extra.length > 0) throw new Error(`${where}: unknown key(s) ${JSON.stringify(extra.sort())}`);
  return got;
}

function substRoot(v: unknown): unknown {
  if (v === "$root") return ROOT_ID;
  if (Array.isArray(v)) return v.map(substRoot);
  if (isObj(v)) return Object.fromEntries(Object.entries(v).map(([k, x]) => [k === "$root" ? ROOT_ID : k, substRoot(x)]));
  return v;
}

function parseSteps(raw: unknown, where: string): Step[] {
  if (raw === null || raw === undefined) return [];
  if (!Array.isArray(raw)) throw new Error(`${where}: expect.steps must be a list`);
  return raw.map((item, i) => {
    const at = `${where}: expect.steps[${i}]`;
    if (!isObj(item)) throw new Error(`${at}: must be a mapping`);
    const verbs = Object.keys(item).filter((k) => STEP_VERBS.has(k));
    const extra = Object.keys(item).filter((k) => !STEP_VERBS.has(k) && !STEP_META.has(k));
    if (extra.length > 0) throw new Error(`${at}: unknown key(s) ${JSON.stringify(extra)}`);
    if (verbs.length !== 1) throw new Error(`${at}: needs exactly ONE verb, got ${JSON.stringify(verbs)}`);
    const kind = verbs[0]!;
    const args = (item[kind] ?? {}) as Obj;
    if (!isObj(args)) throw new Error(`${at}: \`${kind}:\` must be a mapping`);
    const exp = (item["expect"] ?? {}) as Obj;
    if (!isObj(exp)) throw new Error(`${at}: \`expect:\` must be a mapping`);
    const bad = Object.keys(exp).filter((k) => !MATCHERS.has(k));
    if (bad.length > 0) throw new Error(`${at}.expect: unknown matcher(s) ${JSON.stringify(bad)}`);
    if (exp["allow"] === true && "side" in exp && exp["side"] !== null) throw new Error(`${at}: \`side:\` only means something on a denial`);
    if (kind === "reach" || kind === "start") {
      for (const req of ["from", "to", "send"]) if (!(req in args)) throw new Error(`${at}: \`${kind}:\` needs \`${req}:\``);
    }
    if (kind === "operator" && !("to" in args)) throw new Error(`${at}: \`operator:\` needs \`to:\``);
    if (kind === "grant" || kind === "revoke") {
      for (const req of ["agent", "field", "to"]) if (!(req in args)) throw new Error(`${at}: \`${kind}:\` needs \`${req}:\``);
      if (args["field"] !== "can_call" && args["field"] !== "callable_by") throw new Error(`${at}: \`field:\` is can_call or callable_by`);
    }
    if ((kind === "await_task" || kind === "collect") && !("name" in args)) throw new Error(`${at}: \`${kind}:\` needs \`name:\``);
    const because = typeof item["because"] === "string" ? item["because"] : "";
    if (because !== "" && !REQUIREMENTS.has(because)) throw new Error(`${at}: because ${because} is not a requirement`);
    return { n: i + 1, kind, args, expect: exp, because, name: typeof item["name"] === "string" ? item["name"] : "" };
  });
}

function loadConfig(specFile: string): Config {
  const specPath = CONTRACT + "agentic/specs/" + specFile;
  const spec = strict(specPath, parseYaml(readFileSync(specPath, "utf8"), specPath) ?? {}, SPEC_KEYS);
  if (typeof spec["config"] !== "string") throw new Error(`${specPath}: \`config:\` is required`);
  const cfgPath = CONTRACT + "agentic/configs/" + spec["config"];
  const cfg = strict(cfgPath, parseYaml(readFileSync(cfgPath, "utf8"), cfgPath) ?? {}, CONFIG_KEYS);
  const raw = substRoot({ ...cfg, ...spec }) as Obj;
  const reqs = (raw["requirements"] ?? []) as string[];
  for (const rid of reqs) {
    if (!REQUIREMENTS.has(rid)) throw new Error(`${specPath}: unknown requirement ${rid}`);
    if (CORE.has(rid)) throw new Error(`${specPath}: \`requirements:\` names core ${rid}`);
  }
  const exp = strict(`${specPath}: expect`, raw["expect"] ?? {}, EXPECT_KEYS);
  const hydration = typeof exp["hydration"] === "string" ? exp["hydration"] : "ok";
  if (!HYDRATIONS.has(hydration)) throw new Error(`${specPath}: hydration ${hydration}`);
  if (hydration !== "ok" && !exp["error_contains"]) throw new Error(`${specPath}: a refusal must be pinned (error_contains)`);
  const loadersRaw = raw["loaders"];
  if (!Array.isArray(loadersRaw) || loadersRaw.length === 0) throw new Error(`${cfgPath}: loaders must be a non-empty list`);
  const seen = new Set<string>();
  const loaders = loadersRaw.map((l, i) => {
    const item = strict(`${cfgPath}: loaders[${i}]`, l, LOADER_KEYS);
    const id = item["id"];
    if (typeof id !== "string" || id === "") throw new Error(`${cfgPath}: loaders[${i}] needs an id`);
    if (seen.has(id)) throw new Error(`${cfgPath}: duplicate loader ${id}`);
    seen.add(id);
    const picked = ["manifest", "manifest_text", "missing_manifest"].filter((k) => k in item);
    if (picked.length !== 1) throw new Error(`${cfgPath}: loaders[${i}] needs exactly one manifest form`);
    return {
      id,
      file: typeof item["file"] === "string" ? item["file"] : `${id}.yaml`,
      manifest: isObj(item["manifest"]) ? item["manifest"] : null,
      meta: isObj(item["meta"]) ? item["meta"] : {},
    };
  });
  return {
    name: typeof raw["name"] === "string" ? raw["name"] : specFile.replace(/\.yaml$/, ""),
    requirements: reqs,
    loaders,
    hydration,
    errorContains: typeof exp["error_contains"] === "string" ? exp["error_contains"] : "",
    reboot: exp["reboot"] !== false,
    steps: parseSteps(exp["steps"], specPath),
    closedRosterSender: typeof exp["closed_roster_sender"] === "string" ? exp["closed_roster_sender"] : null,
  };
}

function declared(ln: LoaderSpec): Obj[] {
  const agents = ln.manifest?.["agents"];
  return Array.isArray(agents) ? agents.filter(isObj) : [];
}

function rosterIds(cfg: Config): string[] {
  return cfg.loaders.flatMap((ln) => declared(ln).map((a) => a["id"]).filter((x): x is string => typeof x === "string"));
}

// ─── the stand-in bundles and the "disk" they share ────────────────────────

/** One in-memory file space for a whole scenario — it survives a reboot, like a
 *  disk does. Paths are `<root>/<path>` normalized; `.` is the cwd. */
class Disk {
  readonly files = new Map<string, Uint8Array>();
  readonly dirs = new Set<string>();

  static norm(root: string, path: string): string {
    return [...root.split("/"), ...path.split("/")].filter((p) => p !== "" && p !== ".").join("/");
  }

  list(dir: string): { name: string; is_dir: boolean }[] {
    const prefix = dir === "" ? "" : dir + "/";
    const names = new Map<string, boolean>();
    for (const p of [...this.files.keys(), ...this.dirs]) {
      if (!p.startsWith(prefix) || p === dir) continue;
      const rest = p.slice(prefix.length);
      const head = rest.split("/")[0]!;
      names.set(head, rest.includes("/") || this.dirs.has(prefix + head));
    }
    return [...names].sort(([a], [b]) => a.localeCompare(b)).map(([name, is_dir]) => ({ name, is_dir }));
  }
}

const enc = new TextEncoder();
const dec = new TextDecoder();

function registerStandIns(k: Kernel, disk: Disk): void {
  const unknownVerb = (bundle: string, verb: string): Json =>
    refusal(UNHANDLED, `${bundle} (stand-in): unknown verb ${JSON.stringify(verb)}`, { extra: { verb } });

  // file_bridge: a root-keyed file space. NO seal, NO readonly, NO rule check.
  k.registerBundle("file_bridge.tools", (id, p) => {
    const root = String(k.get(id)?.meta["root"] ?? ".");
    const verb = String(p["type"] ?? "");
    const path = Disk.norm(root, String(p["path"] ?? ""));
    switch (verb) {
      case "boot":
      case "shutdown":
        return null;
      case "list":
        return { path: String(p["path"] ?? ""), files: disk.list(path) };
      case "mkdir":
        disk.dirs.add(path);
        return { created: true, path: String(p["path"] ?? "") };
      case "write_stream": {
        const bytes = p["bytes"] as unknown;
        const data = bytes instanceof Uint8Array ? bytes : new Uint8Array();
        const prev = p["truncate"] === true ? new Uint8Array() : disk.files.get(path) ?? new Uint8Array();
        const next = new Uint8Array(prev.length + data.length);
        next.set(prev);
        next.set(data, prev.length);
        disk.files.set(path, next);
        return { written: true, path: String(p["path"] ?? ""), bytes: data.length };
      }
      case "read_stream": {
        const f = disk.files.get(path);
        if (f === undefined) return refusal(NOT_FOUND, `no such file: ${String(p["path"])}`);
        return { path: String(p["path"] ?? ""), bytes: f as unknown as Json, eof: true };
      }
      default:
        return unknownVerb("file_bridge", verb);
    }
  });

  // text_file: owns no disk — every byte is a GATED send to its `target_id` leg.
  k.registerBundle("text_file.tools", async (id, p, kk) => {
    const verb = String(p["type"] ?? "");
    if (verb === "boot" || verb === "shutdown") return null;
    const target = k.get(id)?.meta["target_id"];
    if (typeof target !== "string" || target === "") {
      return refusal(MISSING_ARG, "text_file (stand-in): no file surface wired — set `target_id` to a file_bridge", { extra: { arg: "target_id" } });
    }
    const path = String(p["path"] ?? "");
    if (verb === "write" || verb === "append") {
      const bytes = p["bytes"] instanceof Uint8Array ? (p["bytes"] as unknown as Uint8Array) : enc.encode(String(p["text"] ?? ""));
      const out = (await kk.send([{ target, payload: { type: "write_stream", path, bytes: bytes as unknown as Json, truncate: verb === "write" } }]))[0]!;
      const r = out.result as Payload;
      return isObj(r) && r["written"] === true ? { path, written: true } : (r as Json);
    }
    if (verb === "read") {
      const out = (await kk.send([{ target, payload: { type: "read_stream", path } }]))[0]!;
      const r = out.result as Payload;
      if (!isObj(r) || !(r["bytes"] instanceof Uint8Array)) return r as Json;
      const lines = dec.decode(r["bytes"] as unknown as Uint8Array).split("\n").filter((_, i, a) => i < a.length - 1 || a[i] !== "");
      return { path, lines: lines.map((text, i) => ({ n: i + 1, text })) };
    }
    return unknownVerb("text_file", verb);
  });

  k.registerBundle("exec_bridge.tools", (_id, p) => {
    const verb = String(p["type"] ?? "");
    if (verb === "boot" || verb === "shutdown") return null;
    if (verb === "status") return { jobs: [] };
    return unknownVerb("exec_bridge", verb);
  });

  // python's `lifecycle_probe` test bundle: `slow` stays in flight until killed.
  k.registerBundle("lifecycle_probe.tools", (_id, p) => {
    const verb = String(p["type"] ?? "");
    if (verb === "boot" || verb === "shutdown") return null;
    if (verb === "slow") return new Promise<Json>(() => undefined);
    return unknownVerb("lifecycle_probe", verb);
  });

  // The loaders' own agents: the runner hydrates FOR them (no manifest loader here).
  k.registerBundle("static_loader.tools", (_id, p) => {
    const verb = String(p["type"] ?? "");
    if (verb === "boot" || verb === "shutdown") return null;
    return unknownVerb("static_loader", verb);
  });
}

// ─── hydrate / reboot / dehydrate / reach ──────────────────────────────────

/** The host side of a reboot: the records a ProxyLoader persists over the bridge. */
class HostStore implements Bridge {
  readonly records = new Map<string, Payload>();
  static readonly ANCHOR = "host_session";

  forward(target: string, payload: Payload): Promise<Json> {
    if (target !== HostStore.ANCHOR) return Promise.resolve(refusal("unknown_agent", `host: no agent '${target}'`));
    const t = payload["type"];
    if (t === "persist_record") {
      const rec = payload["record"] as Payload;
      this.records.set(String(rec["id"]), structuredClone(rec));
      return Promise.resolve({ ok: true });
    }
    if (t === "forget_record") {
      this.records.delete(String(payload["id"]));
      return Promise.resolve({ ok: true });
    }
    if (t === "load_tree") {
      return Promise.resolve({
        version: 1,
        records: [{ id: HostStore.ANCHOR, handler_module: "kernel_state.tools" }, ...[...this.records.values()].map((r) => structuredClone(r))],
      } as unknown as Json);
    }
    return Promise.resolve(refusal(UNHANDLED, `host: ${String(t)}`));
  }
  forwardBinary(target: string, payload: Record<string, unknown>): Promise<Json> {
    return this.forward(target, payload as Payload);
  }
  emitRemote(): void {}
  watchRemote(): void {}
  unwatchRemote(): void {}
  onLifecycle(): () => void {
    return () => {};
  }
  subscribeState(): () => void {
    return () => {};
  }
}

interface Hydration {
  cfg: Config;
  k: Kernel;
  disk: Disk;
}

function hydrate(cfg: Config): Hydration {
  const disk = new Disk();
  // python `_write_manifests`: each loader's manifest lands in the cwd.
  for (const ln of cfg.loaders) if (ln.manifest !== null) disk.files.set(Disk.norm(".", ln.file), enc.encode(JSON.stringify(ln.manifest)));
  const k = new Kernel();
  registerStandIns(k, disk);
  k.setRoot(new Agent({ id: ROOT_ID }));
  for (const ln of cfg.loaders) {
    k.register(new Agent({ id: ln.id, parentId: ROOT_ID, handlerModule: "static_loader.tools", meta: { manifest: ln.file, ...(ln.meta as Payload) } }));
    for (const entry of declared(ln)) {
      const { id, handler_module, ...meta } = entry;
      k.register(new Agent({ id: String(id), parentId: ln.id, handlerModule: String(handler_module), meta: structuredClone(meta) as Payload }));
    }
  }
  return { cfg, k, disk };
}

async function reboot(h: Hydration): Promise<Hydration> {
  const host = new HostStore();
  h.k.bridge = host;
  const out = new ProxyLoader(h.k, HostStore.ANCHOR, { debounceMs: 1e9 });
  out.start(); // enqueues EVERY live record
  await out.stop(); // … and the final flush writes them
  h.k.bridge = null;
  const k2 = new Kernel();
  registerStandIns(k2, h.disk);
  k2.bridge = host;
  const back = new ProxyLoader(k2, HostStore.ANCHOR, { debounceMs: 1e9 });
  const records = await back.loadTree();
  k2.bridge = null; // a standalone kernel again: an unknown target is unknown, not forwarded
  k2.load(records);
  return { ...h, k: k2 };
}

interface Dehydrated {
  ids: string[];
  agents: Map<string, Obj>;
  children: Map<string, string[]>;
  tree: Obj;
}

async function dehydrate(k: Kernel): Promise<Dehydrated> {
  // python's dehydrate: `detail: "full"` — the WIRING tree (handler_module,
  // parent_id, posture); the default distilled roster says what agents are FOR.
  const r = (await k.send([{ target: "kernel", payload: { type: "reflect", detail: "full", tree: "all" } }]))[0]!.result as Obj;
  const tree = (r["tree"] ?? {}) as Obj;
  const agents = new Map<string, Obj>();
  const children = new Map<string, string[]>();
  const walk = (node: Obj): void => {
    const kids = (node["children"] ?? []) as Obj[];
    const { children: _c, ...rest } = node;
    agents.set(String(node["id"]), rest);
    children.set(String(node["id"]), kids.map((c) => String(c["id"])));
    for (const c of kids) walk(c);
  };
  if (isObj(tree) && "id" in tree) walk(tree);
  return { ids: [...agents.keys()].sort(), agents, children, tree };
}

function decodeBytesFields(p: Obj): Payload {
  if (!("bytes_utf8" in p)) return p as Payload;
  const { bytes_utf8, ...rest } = p;
  return { ...(rest as Payload), bytes: enc.encode(String(bytes_utf8)) as unknown as Json };
}

async function reachOutcome(k: Kernel, sender: string, target: string, payload: Obj): Promise<Outcome> {
  return (await sendAs(k, sender, [{ target, payload: decodeBytesFields(payload) }]))[0]!;
}

async function reach(k: Kernel, sender: string, target: string, payload: Obj): Promise<Obj> {
  const r = (await reachOutcome(k, sender, target, payload)).result;
  return isObj(r) ? r : {};
}

const denied = (r: Obj): boolean => r["reason"] === "unauthorized";
const sideOf = (r: Obj): unknown => r["side"] ?? null;

// ─── matchers ──────────────────────────────────────────────────────────────

function dig(obj: unknown, path: string): unknown[] {
  let cur: unknown[] = [obj];
  for (const part of path.split(".")) {
    const next: unknown[] = [];
    for (const c of cur) {
      if (part === "*") {
        if (Array.isArray(c)) next.push(...c);
      } else if (isObj(c) && part in c) next.push(c[part]);
    }
    cur = next;
  }
  return cur;
}

function resolveRefs(v: unknown, bound: Map<string, Obj>): unknown {
  if (typeof v === "string" && v.startsWith("$")) {
    const [name, ...rest] = v.slice(1).split(".");
    if (!bound.has(name!)) throw new Error(`$${v.slice(1)}: nothing bound under ${name}`);
    if (rest.length === 0) return bound.get(name!);
    const got = dig(bound.get(name!), rest.join("."));
    if (got.length === 0) throw new Error(`${v}: not present`);
    return got[0];
  }
  if (Array.isArray(v)) return v.map((x) => resolveRefs(x, bound));
  if (isObj(v)) return Object.fromEntries(Object.entries(v).map(([k, x]) => [k, resolveRefs(x, bound)]));
  return v;
}

function label(s: Step): string {
  if (s.name !== "") return s.name;
  if (s.kind === "reach" || s.kind === "start") {
    return `${String(s.args["from"])} → ${String(s.args["to"])} ${JSON.stringify((s.args["send"] as Obj)["type"])}`;
  }
  return `${s.kind} ${JSON.stringify(s.args)}`;
}

/** Every string, number and byte string in `x`, joined — bytes read as utf-8
 *  (python `_engine._flat_text`: keys and values, `str()` of a scalar). */
function flatText(x: unknown): string {
  if (x instanceof Uint8Array) return dec.decode(x);
  if (Array.isArray(x)) return x.map(flatText).join("\n");
  if (isObj(x)) return Object.entries(x).map(([k, v]) => `${k}\n${flatText(v)}`).join("\n");
  if (x === null || x === undefined) return "";
  if (typeof x === "boolean") return x ? "True" : "False";
  if (typeof x === "number") return pyRepr(x);
  return String(x);
}

function match(s: Step, exp: Obj, reply: Obj, outcome: Outcome | null): void {
  const fail = (why: string): never => {
    throw new StepFailure(`#${s.n} ${label(s)}: ${why}\n  reply: ${JSON.stringify(reply, (_k, v) => (v instanceof Uint8Array ? `<bytes:${v.length}>` : v))}`);
  };
  if ("allow" in exp) {
    if (exp["allow"] === true && denied(reply)) fail("expected the edge to be OPEN, it was denied");
    if (exp["allow"] === false && !denied(reply)) fail("expected a DENIAL, the call went through");
  }
  if ("side" in exp) {
    const got = sideOf(reply);
    if (exp["side"] === null) {
      if (got !== null) fail(`expected NO side (an io seal), got ${String(got)}`);
    } else if (got !== exp["side"]) fail(`expected the ${String(exp["side"])} half to refuse, got ${String(got)}`);
  }
  if ("reason" in exp && reply["reason"] !== exp["reason"]) fail(`expected reason ${String(exp["reason"])}, got ${String(reply["reason"])}`);
  if ("error_contains" in exp && !String(reply["error"] ?? "").includes(String(exp["error_contains"]))) {
    fail(`error does not contain ${String(exp["error_contains"])}`);
  }
  if ("status" in exp && outcome?.status !== exp["status"]) fail(`expected status ${String(exp["status"])}, got ${String(outcome?.status)}`);
  for (const [path, want] of Object.entries((exp["contains"] ?? {}) as Obj)) {
    if (path.split(".").at(-1) === "bytes") {
      const got = dig(reply, path).map((v) => (v instanceof Uint8Array ? dec.decode(v) : v));
      if (!got.some((g) => typeof g === "string" && g.includes(String(want)))) fail(`${path} does not contain ${JSON.stringify(want)}`);
      continue;
    }
    if (!dig(reply, path).some((g) => isDeepStrictEqual(g, want))) fail(`${path} does not contain ${JSON.stringify(want)} (got ${JSON.stringify(dig(reply, path))})`);
  }
  if ("mentions" in exp || "absent" in exp) {
    const flat = flatText(reply);
    if ("mentions" in exp && !flat.includes(String(exp["mentions"]))) fail(`the reply never mentions ${JSON.stringify(exp["mentions"])}`);
    if ("absent" in exp && flat.includes(String(exp["absent"]))) fail(`the reply carries ${JSON.stringify(exp["absent"])} and must not`);
  }
  for (const [path, unwanted] of Object.entries((exp["not_contains"] ?? {}) as Obj)) {
    const got = dig(reply, path);
    if (got.length === 0) fail(`not_contains ${path} resolved to nothing — absence here proves nothing`);
    if (got.some((g) => isDeepStrictEqual(g, unwanted))) fail(`${path} contains ${JSON.stringify(unwanted)} and must not`);
  }
}

// ─── the exemptions — BY NAME, with the reason ─────────────────────────────

const CRUD = "(a) tree CRUD is the host root's — this kernel answers no create_agent / update_agent / delete_agent (the C21 driver skips the same cases)";
const IO = "(b) host io-bundle policy — the io_bridge seal, `readonly`, what a leg's `root` serves — this kernel ships no io legs, and a stand-in answering it would be measuring itself";
const LOADER = "(c) static_loader — the manifest loader is a host bundle; this kernel has none, its records persist through the host loader as written";
const dep = (n: number): string => `(d) its premise is step #${n}, which is skipped`;

/** Whole specs this kernel cannot run, and why. */
const SKIP_SPECS: { [spec: string]: string } = {
  engine_smoke_bad_entry: `${LOADER}: the boot_error verdict is static_loader's (a file_bridge rule refused at the child's write), so PC-5 is its whole contract`,
  engine_smoke_unreadable_manifest: `${LOADER}: the refused_at_write verdict is static_loader's validate_meta reading the manifest path, so PC-5 is its whole contract`,
  exec_shell: "(c) exec_bridge — the process edge is a host bundle; this kernel spawns no processes, so every EXS requirement is the hosts'",
  credential_store: "credential_store is a host bundle (py/rs/sw) — it keeps values in a 0600 file, the OS keychain or the environment, none of which a browser kernel has; CRED-1..5 are the hosts'",
};

/** Steps skipped by spec + 1-based number. `name` pins WHICH step was meant: if
 *  the corpus moves, the exemption fails loudly instead of skipping the wrong one. */
const SKIP_STEPS: { [spec: string]: { [n: number]: { name: string; why: string } } } = {
  two_sided_gate: {
    28: { name: "but the runtime-only edge is NOT — the manifest never declared it", why: `${LOADER}: re-applying the manifest over the hydrated record at boot is static_loader's; here a runtime grant is a persisted record and survives a reboot by design` },
  },
  lifecycle: {
    15: { name: "the operator deletes it — the grant is deliberately left standing", why: CRUD },
    16: { name: "so the refusal is about NON-EXISTENCE, not about authority", why: dep(15) },
  },
  namespacing: {
    1: { name: "a wing reaches its own memory, and the write actually lands", why: `${IO} (what text_file writes, through its leg's root)` },
    7: { name: "a note written in one wing's memory is readable in that wing", why: `${IO} (text_file content through its leg's root)` },
    8: { name: "and is ABSENT from the other wing's memory — same path, different owner", why: `${IO} (text_file content through its leg's root)` },
    10: { name: "", why: `${IO} (what a leg's root serves)` },
    11: { name: "the neighbour's file of the same name is untouched", why: `${IO} (what a leg's root serves)` },
    15: { name: "", why: `${IO} (text_file content after a reboot)` },
    16: { name: "", why: `${IO} (text_file content after a reboot)` },
  },
  owned_subtree_delete: {
    9: { name: "deleting the loader answers for the loader it was asked about", why: CRUD },
    10: { name: "the whole subtree is gone — a member is a NON-AGENT now, not a denial", why: dep(9) },
    11: { name: "", why: dep(9) },
    14: { name: "the tree agrees — nothing of the doomed roster is left standing", why: dep(9) },
    15: { name: "the lock refuses the cascade and NAMES what blocked it", why: CRUD },
    16: { name: "and nothing in that subtree was taken on the way to the lock", why: dep(15) },
    17: { name: "", why: dep(15) },
    18: { name: "clearing the lock the refusal named lets the same cascade through", why: CRUD },
    19: { name: "", why: CRUD },
  },
  sealed_by_default: {
    2: { name: "a sealed leg refuses the same caller the same directory", why: IO },
    3: { name: "an explicit deny_ingress refuses exactly like an absent rule", why: IO },
    4: { name: "the process edge of the io family seals by the same default", why: IO },
    7: { name: "the seal refuses the OPERATOR, because it gates the edge not the caller", why: IO },
    8: { name: "and a write over the same sealed edge is refused the same way", why: IO },
    9: { name: "the refusal was not a quiet no-op — the byte never reached the disk", why: dep(8) },
    10: { name: "an explicit deny_ingress is the SAME seal, said out loud", why: IO },
    11: { name: "an OPEN leg answers that same operator — the seal is the only difference", why: IO },
    12: { name: "a readonly leg READS", why: IO },
    13: { name: "and refuses to write, without the call gate being involved at all", why: IO },
    14: { name: "which the disk confirms", why: dep(13) },
    15: { name: "a file written through one leg is invisible to a leg rooted elsewhere", why: IO },
    16: { name: "the vault leg sees its own file", why: IO },
    17: { name: "and the data leg cannot — different roots, no shared view", why: IO },
    18: { name: "a sealed leg still DESCRIBES itself — reflect is how you learn to open it", why: IO },
    19: { name: "opening the seal takes an OPERATOR write, and the same call then lands", why: CRUD },
    20: { name: "", why: dep(19) },
    21: { name: "a near-miss rule name is refused AT THE WRITE, leaving the leg as it was", why: CRUD },
    22: { name: "so the leg it named is still sealed", why: IO },
  },
};

// ─── core: PC-1..PC-5 ──────────────────────────────────────────────────────

async function coreBefore(h: Hydration): Promise<void> {
  const cfg = h.cfg;
  skip("PC-5", cfg.name, `${LOADER}: the hydration verdict and its two refusal channels are static_loader's`);

  // PC-1 — the running tree matches the declared shape, both directions.
  const d = await dehydrate(h.k);
  for (const ln of cfg.loaders) {
    check("PC-1", d.agents.has(ln.id), `loader ${ln.id} is not in the running tree: ${d.ids}`);
    for (const spec of declared(ln)) {
      const aid = spec["id"];
      check("PC-1", typeof aid === "string" && aid !== "", `${ln.id}: an id-less entry duplicates on every boot`);
      const node = d.agents.get(String(aid));
      check("PC-1", node !== undefined, `${String(aid)} declared by ${ln.id} but not running`);
      check("PC-1", node!["handler_module"] === spec["handler_module"], `${String(aid)}: declared ${String(spec["handler_module"])}, running ${String(node!["handler_module"])}`);
      check("PC-1", node!["parent_id"] === ln.id, `${String(aid)}: parent is ${String(node!["parent_id"])}, expected ${ln.id}`);
    }
    const want = new Set(declared(ln).map((a) => String(a["id"])));
    const got = new Set(d.children.get(ln.id) ?? []);
    check("PC-1", isDeepStrictEqual(got, want), `${ln.id} owns ${[...got].sort()}, declared ${[...want].sort()}`);
  }

  // PC-2 — the reach steps ARE the declared edges; a spec with none says nothing.
  check("PC-2", cfg.steps.some((s) => s.kind === "reach"), `${cfg.name}: declares no reach step`);

  // PC-3 — the roster is closed after boot.
  const sender = cfg.closedRosterSender ?? rosterIds(cfg)[0] ?? null;
  if (sender === null) return;
  check("PC-3", h.k.get(sender) !== undefined, `closure probe ${sender} is not a running agent`);
  const before = [...h.k.agents.keys()].sort();
  const create = { type: "create_agent", parent_id: ROOT_ID, handler_module: "file_bridge.tools", id: "smuggled" };
  const r = await reach(h.k, sender, ROOT_ID, create);
  check("PC-3", denied(r), `${sender} could create_agent on the root: ${JSON.stringify(r)}`);
  check("PC-3", sideOf(r) === "egress", `expected an egress refusal, got ${JSON.stringify(r)}`);
  check("PC-3", isDeepStrictEqual([...h.k.agents.keys()].sort(), before), "a refused create changed the roster");
  // an agent editing its OWN can_call: refused at egress — it holds no
  // update_agent on the root
  const self = await reach(h.k, sender, ROOT_ID, { type: "update_agent", id: sender, can_call: { [ROOT_ID]: ["create_agent"] } });
  check("PC-3", denied(self), `${sender} raised its own authority: ${JSON.stringify(self)}`);
  check("PC-3", sideOf(self) === "egress", `expected an egress refusal, got ${JSON.stringify(self)}`);
  const r2 = await reach(h.k, sender, ROOT_ID, { type: "create_agent", handler_module: "file_bridge.tools", id: "smuggled" });
  check("PC-3", denied(r2), `the roster opened after all: ${JSON.stringify(r2)}`);
}

async function coreAfter(h: Hydration): Promise<Hydration> {
  if (!h.cfg.reboot) return h;
  const before = [...h.k.agents.keys()].sort();
  const h2 = await reboot(h);
  const after = [...h2.k.agents.keys()].sort();
  check("PC-4", isDeepStrictEqual(after, before), `roster changed across a reboot: ${before} → ${after}`);
  check(
    "PC-4",
    isDeepStrictEqual(after.filter((i) => MINTED_ID.test(i)), before.filter((i) => MINTED_ID.test(i))),
    "auto-minted ids appeared across the reboot",
  );
  const dBefore = await dehydrate(h.k);
  const dAfter = await dehydrate(h2.k);
  check("PC-4", isDeepStrictEqual(canon(dAfter.tree), canon(dBefore.tree)), "the tree did not come back identical");
  skip("PC-4", `${h.cfg.name}: each loader re-boots and REAPPLIES its roster (booted / created: [] / reapplied)`, LOADER);
  return h2;
}

/** Children in id order: a rebuild may hand them back sorted, a hydration in
 *  manifest order — an ordering difference, not a roster difference. */
function canon(node: unknown): unknown {
  if (Array.isArray(node)) return node.map(canon);
  if (!isObj(node)) return node;
  const out = Object.fromEntries(Object.entries(node).map(([k, v]) => [k, canon(v)]));
  if (Array.isArray(out["children"])) {
    out["children"] = [...(out["children"] as Obj[])].sort((a, b) => String(a["id"]).localeCompare(String(b["id"])));
  }
  return out;
}

// ─── the step executor ─────────────────────────────────────────────────────

function specsById(cfg: Config): Map<string, Obj> {
  return new Map(cfg.loaders.flatMap((ln) => declared(ln).map((a) => [String(a["id"]), a] as [string, Obj])));
}

function declaredVerbs(spec: Obj, field: string, other: string): Set<string> {
  const grant = ((spec[field] ?? {}) as Obj)[other];
  if (grant === undefined || grant === null) return new Set();
  if (!Array.isArray(grant)) throw new Error(`reconcile_grants: ${field}[${other}] is ${JSON.stringify(grant)}; needs an explicit verb list`);
  return new Set(grant as string[]);
}

async function runSteps(h0: Hydration): Promise<void> {
  let h = h0;
  const cfg = h.cfg;
  const skips = SKIP_STEPS[cfg.name] ?? {};
  const bound = new Map<string, Obj>();
  const started = new Map<string, Promise<Outcome>>();
  let treeBefore: Dehydrated | null = null;

  for (const [idx, s] of cfg.steps.entries()) {
    const ex = skips[s.n];
    if (ex !== undefined) {
      assert.equal(s.name, ex.name, `${cfg.name} #${s.n}: the exemption names a step the corpus no longer has there — re-examine it`);
      const what = `${cfg.name} #${s.n} ${label(s)}`;
      if (s.because !== "") skip(s.because, what, ex.why);
      else UNTAGGED_SKIPS.push(`${what} — ${ex.why}`);
      continue;
    }
    const args = resolveRefs(s.args, bound) as Obj;
    let reply: Obj = {};
    let outcome: Outcome | null = null;
    switch (s.kind) {
      case "reach":
        reply = await reach(h.k, String(args["from"]), String(args["to"]), args["send"] as Obj);
        break;
      case "operator": {
        const item: Item = { target: String(args["to"]), payload: decodeBytesFields((args["send"] ?? {}) as Obj) };
        // the batch item's own fields, beside the payload
        if ("id" in args) item.id = args["id"] as string | number;
        outcome = (await h.k.send([item]))[0]!;
        reply = isObj(outcome.result) ? outcome.result : {};
        break;
      }
      case "grant":
      case "revoke": {
        // sugar over the operator's write, MERGED — a bare write would drop the
        // `$self` baseline the kernel stamped (update REPLACES a field)
        const agent = h.k.get(String(args["agent"]));
        assert.ok(agent !== undefined, `#${s.n}: no agent ${String(args["agent"])}`);
        const field = String(args["field"]);
        const current = { ...((agent.meta[field] ?? {}) as Payload) };
        if (s.kind === "revoke") delete current[String(args["to"])];
        else current[String(args["to"])] = ((args["verbs"] as Json) ?? ["*"]) as Json;
        h.k.updateMeta(agent.id, { [field]: current });
        reply = { updated: true };
        break;
      }
      case "reboot":
        treeBefore = await dehydrate(h.k);
        h = await reboot(h);
        reply = { rebooted: true };
        break;
      case "assert_tree": {
        const got = await dehydrate(h.k);
        if (args["same_as"] === "before_reboot") {
          if (treeBefore === null) throw new Error(`#${s.n}: same_as before_reboot with no preceding reboot`);
          if (!isDeepStrictEqual(canon(got.tree), canon(treeBefore.tree))) {
            throw new StepFailure(`#${s.n} ${label(s)}: the roster did not come back identical`);
          }
        }
        reply = { ids: got.ids, tree: got.tree };
        break;
      }
      case "start":
        started.set(String(args["name"]), reachOutcome(h.k, String(args["from"]), String(args["to"]), args["send"] as Obj));
        reply = { started: true };
        break;
      case "await_task": {
        const deadline = Date.now() + Number(args["timeout"] ?? 5) * 1000;
        let row: Obj | null = null;
        while (Date.now() < deadline && row === null) {
          const ps = (await h.k.send([{ target: "kernel", payload: { type: "ps" } }]))[0]!.result as Obj;
          row = ((ps["tasks"] ?? []) as Obj[]).find((t) => t["agent"] === args["agent"]) ?? null;
          if (row === null) await new Promise((r) => setTimeout(r, 5));
        }
        if (row === null) throw new StepFailure(`#${s.n} ${label(s)}: no task for ${String(args["agent"])} appeared in ps`);
        bound.set(String(args["name"]), row);
        reply = row;
        break;
      }
      case "collect": {
        const name = String(args["name"]);
        const p = started.get(name);
        if (p === undefined) throw new Error(`#${s.n}: nothing started under ${name}`);
        started.delete(name);
        outcome = await p;
        reply = isObj(outcome.result) ? outcome.result : {};
        bound.set(name, { ...(bound.get(name) ?? {}), ...(outcome as unknown as Obj) });
        break;
      }
      case "reconcile_grants":
      case "sweep_undeclared": {
        const table = (args["payloads"] ?? {}) as Obj;
        const payloadFor = (verb: string): Obj => ({ type: verb, ...((table[verb] ?? {}) as Obj) });
        if (s.kind === "reconcile_grants") await sweepReconcile(h, s, payloadFor);
        else await sweepUndeclared(h, s, payloadFor, cfg.steps.slice(0, idx));
        reply = { swept: true };
        break;
      }
      default:
        throw new Error(`#${s.n}: unknown step verb ${s.kind}`);
    }
    match(s, resolveRefs(s.expect, bound) as Obj, reply, outcome);
    if (s.because !== "") check(s.because, true, label(s));
  }
  assert.equal(started.size, 0, `started and never collected: ${[...started.keys()]}`);
}

async function sweepReconcile(h: Hydration, s: Step, payloadFor: (v: string) => Obj): Promise<void> {
  const specs = specsById(h.cfg);
  let probed = 0;
  for (const [sender, sSpec] of specs) {
    for (const [target, tSpec] of specs) {
      if (sender === target) continue;
      const egress = declaredVerbs(sSpec, "can_call", target);
      const ingress = declaredVerbs(tSpec, "callable_by", sender);
      for (const verb of [...new Set([...egress, ...ingress])].sort()) {
        probed += 1;
        const r = await reach(h.k, sender, target, payloadFor(verb));
        const both = egress.has(verb) && ingress.has(verb);
        if (denied(r) === both) {
          throw new StepFailure(`#${s.n}: ${sender} -${verb}-> ${target}: expected ${both ? "reachable" : "refused"}, got ${JSON.stringify(r)}`);
        }
        if (both) continue;
        const want = egress.has(verb) ? "ingress" : "egress";
        if (sideOf(r) !== want) throw new StepFailure(`#${s.n}: ${sender} -${verb}-> ${target}: the half that omitted it is ${want}, the denial named ${String(sideOf(r))}`);
      }
    }
  }
  const floor = Number(s.args["min_probes"] ?? 1);
  if (probed < floor) throw new StepFailure(`#${s.n}: only ${probed} declared triples to walk (min ${floor})`);
}

async function sweepUndeclared(h: Hydration, s: Step, payloadFor: (v: string) => Obj, prior: Step[]): Promise<void> {
  const d = await dehydrate(h.k);
  const universe = [ROOT_ID, ...h.cfg.loaders.map((l) => l.id), ...rosterIds(h.cfg)];
  const missing = universe.filter((u) => !d.ids.includes(u));
  if (missing.length > 0) throw new StepFailure(`#${s.n}: sweep universe is not running: ${missing}`);
  // the baseline is `{$self: "*"}` alone: every other open edge is declared
  const structural = new Set(universe.map((a) => `${a}|${a}`));
  for (const verb of ((s.args["verbs"] ?? ["reflect"]) as string[])) {
    const declaredOpen = new Set(
      prior
        .filter((p) => p.kind === "reach" && p.expect["allow"] === true && (p.args["send"] as Obj)["type"] === verb)
        .map((p) => `${String(p.args["from"])}|${String(p.args["to"])}`),
    );
    const want = new Set([...structural, ...declaredOpen]);
    const got = new Set<string>();
    const sides = new Set<unknown>();
    for (const sender of universe) {
      for (const target of universe) {
        const r = await reach(h.k, sender, target, payloadFor(verb));
        if (denied(r)) sides.add(sideOf(r));
        else got.add(`${sender}|${target}`);
      }
    }
    const extra = [...got].filter((x) => !want.has(x)).sort();
    const lost = [...want].filter((x) => !got.has(x)).sort();
    if (extra.length > 0 || lost.length > 0) throw new StepFailure(`#${s.n}: verb ${verb}: undeclared edges ${extra}, declared-and-missing ${lost}`);
    const bad = [...sides].filter((x) => x !== "egress" && x !== "ingress");
    if (bad.length > 0) throw new StepFailure(`#${s.n}: verb ${verb}: a refusal did not name a call-gate half: ${JSON.stringify(bad)}`);
  }
}

// ─── the suite ─────────────────────────────────────────────────────────────

const SPECS = readdirSync(CONTRACT + "agentic/specs").filter((f) => f.endsWith(".yaml")).sort();

test("the corpus loads strictly — every spec, its config, every step", () => {
  assert.ok(SPECS.length > 0, "no specs under contract/agentic/specs");
  for (const f of SPECS) loadConfig(f);
  // the strict parser refuses what it does not know, naming where
  assert.throws(() => parseSteps([{ reach: { from: "a", to: "b", send: {} }, expekt: {} }], "probe"), /unknown key/);
  assert.throws(() => parseSteps([{ reach: { from: "a", to: "b", send: {} }, expect: { alow: true } }], "probe"), /unknown matcher/);
  assert.throws(() => parseSteps([{ reach: {}, operator: {} }], "probe"), /exactly ONE verb/);
});

test("every requirement a spec claims is actually pinned by one of its steps", () => {
  for (const f of SPECS) {
    const cfg = loadConfig(f);
    const tagged = new Set(cfg.steps.map((s) => s.because).filter((b) => b !== ""));
    const missing = cfg.requirements.filter((r) => !tagged.has(r));
    assert.deepEqual(missing, [], `${f}: claims ${missing} but no step tags them`);
  }
});

test("every exemption names a spec and a step the corpus has", () => {
  const names = new Set(SPECS.map((f) => loadConfig(f).name));
  for (const spec of [...Object.keys(SKIP_SPECS), ...Object.keys(SKIP_STEPS)]) assert.ok(names.has(spec), `exemption for unknown spec ${spec}`);
  for (const [spec, steps] of Object.entries(SKIP_STEPS)) {
    const cfg = loadConfig(`${spec}.yaml`);
    for (const n of Object.keys(steps)) assert.ok(Number(n) <= cfg.steps.length, `${spec} #${n} does not exist`);
  }
});

for (const f of SPECS) {
  const cfg = loadConfig(f);
  test(`scenario ${cfg.name}`, async (t) => {
    const whole = SKIP_SPECS[cfg.name];
    if (whole !== undefined) {
      skip("PC-5", cfg.name, whole);
      // …and every requirement the spec itself names, so a skipped spec's own
      // requirements read "not applicable", never "evidence elsewhere".
      for (const rid of cfg.requirements) skip(rid, cfg.name, whole);
      t.diagnostic(`skipped: ${whole}`);
      return;
    }
    assert.equal(cfg.hydration, "ok", `${cfg.name}: a non-ok hydration must be exempted by name`);
    let h = hydrate(cfg);
    await coreBefore(h);
    h = await coreAfter(h);
    await runSteps(h);
  });
}

// What this kernel is expected to prove, decided up front: a requirement moving
// between these sets is a change someone has to look at, not a silent drift.
const EXPECTED = {
  covered: ["PC-1", "PC-2", "PC-3", "C14", "C16", "TSG-1", "TSG-2", "TSG-3", "TSG-4", "TSG-5", "DOC-1", "DOC-2", "DOC-3", "DOC-4", "DOC-5", "FLT-1", "FLT-2", "FLT-3"],
  // C18: only its PRECONDITIONS run here (the roster is live, the neighbour's
  // edge works) — the cascade itself is `delete_agent`, which this kernel lacks.
  partial: ["PC-4", "C15", "C18", "C20", "TSG-6"],
  notApplicable: ["PC-5", "C17", "C23", "EXS-1", "EXS-2", "EXS-3", "EXS-4", "EXS-5", "CRED-1", "CRED-2", "CRED-3", "CRED-4", "CRED-5"],
  // Registry rows whose evidence is NOT in the data corpus: the registry itself
  // points at host test files (C1, C24) or at conformance/cases.json (C21, which
  // `c21_conformance.test.ts` replays against this kernel).
  elsewhere: { C1: "python tests/conformance/test_c1_suspend_resume + tests/kernel/test_inquiry_plumbed — no corpus data to replay", C21: "contract/conformance/cases.json — replayed by c21_conformance.test.ts", C24: "python tests/conformance + test_stream_semantics — no corpus data to replay" } as { [k: string]: string },
};

test("coverage matrix — N of M requirements, and every exclusion with its reason", (t) => {
  const covered: string[] = [];
  const partial: string[] = [];
  const na: string[] = [];
  const elsewhere: string[] = [];
  for (const [rid, c] of COVERAGE) {
    if (c.pass === 0 && c.skipped.length === 0) elsewhere.push(rid);
    else if (c.pass === 0) na.push(rid);
    else if (c.skipped.length > 0) partial.push(rid);
    else covered.push(rid);
  }
  const lines = [`requirements: ${REQUIREMENTS.size}; applicable and fully covered: ${covered.length}; partially: ${partial.length}; not applicable: ${na.length}; evidence outside the agentic corpus: ${elsewhere.length}`];
  for (const rid of [...partial, ...na]) {
    lines.push(`${rid} (${na.includes(rid) ? "not applicable" : "partial"}, ${COVERAGE.get(rid)!.pass} checks ran):`);
    for (const why of COVERAGE.get(rid)!.skipped) lines.push(`    skipped ${why}`);
  }
  for (const rid of elsewhere) lines.push(`${rid}: ${EXPECTED.elsewhere[rid] ?? "?"}`);
  for (const why of UNTAGGED_SKIPS) lines.push(`(untagged step) skipped ${why}`);
  for (const l of lines) t.diagnostic(l);
  assert.deepEqual(covered.sort(), [...EXPECTED.covered].sort(), "fully covered");
  assert.deepEqual(partial.sort(), [...EXPECTED.partial].sort(), "partially covered");
  assert.deepEqual(na.sort(), [...EXPECTED.notApplicable].sort(), "not applicable");
  assert.deepEqual(elsewhere.sort(), Object.keys(EXPECTED.elsewhere).sort(), "evidence elsewhere");
});
