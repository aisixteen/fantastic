// Integration-test harness: boot a real python `fantastic` host (web + a
// nested web_ws surface) in a throwaway temp dir, and tear it down. Used by
// *.itest.ts. NOT a *.test.ts file, so the unit run never imports it.
//
// web_ws must be nested UNDER the web agent on disk (create by targeting the
// web agent, not kernel_state) — the tree comes from disk placement, and web mounts
// only its own children's get_routes. See bridge.ts's federation note.

import { spawn, spawnSync } from "node:child_process";
import type { ChildProcess } from "node:child_process";
import {
  readdirSync,
  mkdtempSync,
  mkdirSync,
  rmSync,
  statSync,
  readFileSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = fileURLToPath(new URL("../../", import.meta.url));
const FANTASTIC = join(repoRoot, "src", "lib", "python", ".venv", "bin", "fantastic_kernel");

// Target selection — mirrors the python integration harness. `local` (default)
// runs the built venv binary; `container` runs the universal image (python
// runtime). The SAME tests run either way: seeding one-shots run INSIDE the
// container (a rootless container's uid can't write a host-seeded .fantastic/),
// the daemon publishes -p 127.0.0.1:port:port (its `/` is the dynamic agent
// index), and the canvas page is extracted into the (bind-mounted) workdir so
// the `canvas_page` file_bridge agent can serve it.
// e2e is host/browser → container only (no container↔container), so -p suffices.
const TARGET = (process.env.FANTASTIC_TARGET ?? "local").trim().toLowerCase();
const IMAGE = process.env.FANTASTIC_IMAGE ?? "fantastic:latest";
const CONTAINER_BIN = "/opt/fantastic/venv/bin/fantastic_kernel"; // python in the image
// The interpreter the ONE executor (`exec_bridge`) is allowed to spawn. It has NO
// default — the operator names it — so the tests name the venv python that runs the
// kernel itself (local) or the image's venv python (container).
const PY_INTERP =
  TARGET === "container"
    ? "/opt/fantastic/venv/bin/python"
    : join(repoRoot, "src", "lib", "python", ".venv", "bin", "python");

function resolveEngine(): string {
  for (const e of ["podman", "docker"]) {
    if (spawnSync(e, ["--version"], { stdio: "ignore" }).status === 0) return e;
  }
  throw new Error("FANTASTIC_TARGET=container but no podman/docker found");
}
const ENGINE = TARGET === "container" ? resolveEngine() : "";

/** Daemon `run` args for container mode — shared by bootHost + restartHost so a
 *  restart re-creates an identical container (same name, port, mounts). */
function containerRunArgs(name: string, port: number, tmp: string): string[] {
  const args = [
    "run",
    "-d",
    "--name",
    name,
    "-p",
    `127.0.0.1:${port}:${port}`,
    "-v",
    `${tmp}:/work`,
    "-e",
    "FANTASTIC_RUNTIME=python",
    "-e",
    `FANTASTIC_PORT=${port}`,
  ];
  // Forward LLM keys for the (opt-in, paid) live-LLM tests.
  const env = envFromDotenv();
  // Forward ONLY the selected backend's secret (no vLLM key into an ollama run,
  // none at all with LLM_BACKEND unset) — same no-leak guarantee as daemonEnv(),
  // for the container target.
  const forward =
    LLM_BACKEND === "vllm"
      ? ["VLLM_API_KEY", "OLLAMA_HOST"]
      : LLM_BACKEND === "ollama"
        ? ["OLLAMA_HOST"]
        : [];
  for (const k of forward) {
    if (env[k]) args.push("-e", `${k}=${env[k]}`);
  }
  args.push(IMAGE);
  return args;
}

/** Parse the repo `.env` into a {KEY: value} map (so the spawned daemon — which
 *  runs in a tmp cwd and can't see the repo `.env` — gets e.g. VLLM_API_KEY in
 *  its environment, where the `creds` store reads it). Stdlib-only, mirrors the
 *  python `_load_dotenv` rules. */
function envFromDotenv(): Record<string, string> {
  const out: Record<string, string> = {};
  try {
    for (const raw of readFileSync(join(repoRoot, ".env"), "utf8").split("\n")) {
      const line = raw.trim().replace(/^export\s+/, "");
      if (!line || line.startsWith("#") || !line.includes("=")) continue;
      const eq = line.indexOf("=");
      const k = line.slice(0, eq).trim();
      let v = line.slice(eq + 1).trim();
      if (v.length >= 2 && v[0] === v[v.length - 1] && (v[0] === '"' || v[0] === "'")) {
        v = v.slice(1, -1);
      }
      if (k) out[k] = v;
    }
  } catch {
    /* no .env — fine */
  }
  return out;
}

/** Read one key from the repo `.env` (or process env) — test-side gating for
 *  live-LLM tests, e.g. `dotenvKey("VLLM_API_KEY")`. */
export function dotenvKey(name: string): string | undefined {
  return process.env[name] ?? envFromDotenv()[name];
}

export interface Host {
  origin: string;
  port: number;
  /** http origin (same port as the WS). */
  httpOrigin: string;
  /** The canvas page, when `serveDist` was set: its one-tag mount page, served
   *  by the `canvas_page` file_bridge from the workdir. */
  pageUrl?: string;
  /** The interpreter the exec_bridge leg is allowed to run (`<it> -c <code>`),
   *  when `execBridge` (or `llm`) was set. */
  python?: string;
  webId: string;
  webWsId: string;
  /** id of the `web_loader` (the frontend store), if `bootHost` was asked to
   *  create one — reach it over WS by its id, `web_loader`. */
  webLoaderId?: string;
  /** id of an `exec_bridge` script-executor agent, if `execBridge` was set. */
  pyId?: string;
  /** id of an `ollama_backend` LLM agent, if `llm` was set. */
  ollamaId?: string;
  /** id of a `scheduler` agent, if `scheduler` was set. */
  schedulerId?: string;
  tmp: string;
  /** the daemon process (local target) — undefined in container mode. */
  proc?: ChildProcess;
  /** the container name (container target) — undefined in local mode. */
  container?: string;
}

export interface BootOptions {
  /** Also create a `web_loader` (a kernel_state rooted at .fantastic/web, id
   *  `web_loader`) under the web agent — the frontend's persistence store. */
  webLoader?: boolean;
  /** Also serve the CANVAS PAGE: the canvas app is packed
   *  (`src/tui/web/canvas/scripts/pack.sh` → `dist/canvas.zip`), its
   *  `bundle.min.js` extracted into the workdir beside a one-tag `index.html`,
   *  and served by a `file_bridge` agent (id `canvas_page`) under web —
   *  `host.pageUrl`. */
  serveDist?: boolean;
  /** Also create an `exec_bridge` agent (the background-script runner). */
  execBridge?: boolean;
  /** Also create an LLM backend agent (+ its history `file_bridge` agent). `bundle`
   *  picks the backend (default `ollama_backend.tools`; e.g. `vllm_backend.tools`).
   *  Each record field is passed only when set — the backend refuses a record
   *  missing one it requires (vllm: endpoint, model, max_tokens, kv_slots,
   *  kv_overflow). A KEYED bundle reads its key from a `creds` credential_store
   *  (env backend) named in its `credentials` — the repo `.env` is injected into
   *  the daemon env, so the STORE reads e.g. VLLM_API_KEY; no backend reads a key
   *  from the environment. */
  llm?: {
    bundle?: string;
    model?: string;
    endpoint?: string;
    num_ctx?: number;
    max_tokens?: number;
    kv_slots?: number;
    kv_overflow?: string;
    /** More reach for the AI: host id → verbs (`"*"` or a list). Written on
     *  BOTH halves — the AI's `can_call`, and each target's `callable_by`
     *  (the root included: `kernel_state`). */
    reach?: Record<string, unknown>;
  };
  /** Also create a `scheduler` agent (it persists its schedules through the
   *  root loader) — fire a chosen payload to a target after N seconds. */
  scheduler?: boolean;
}

/** The canvas app and its pack output. */
export const CANVAS_APP = join(repoRoot, "src", "tui", "web", "canvas");
export const CANVAS_ZIP = join(CANVAS_APP, "dist", "canvas.zip");

/** The canvas's one-tag mount page: the bundle inlines three, xterm and its CSS. */
export const CANVAS_INDEX =
  '<!doctype html><html><head><meta charset="utf-8"><title>fantastic · canvas</title></head>\n' +
  '<body><script type="module" src="/canvas_page/file/bundle.min.js"></script></body></html>\n';

/** A literal as a regex (the exec leg's `allowed` names one interpreter path). */
function escapeRegex(lit: string): string {
  return lit.replace(/[.*+?^$()|[\]\\{}]/g, (c) => `\\${c}`);
}

/** The newest mtime under `dir` (files only). */
function newest(dir: string): number {
  let t = 0;
  for (const e of readdirSync(dir, { withFileTypes: true })) {
    const p = join(dir, e.name);
    t = Math.max(t, e.isDirectory() ? newest(p) : statSync(p).mtimeMs);
  }
  return t;
}

/** Pack the canvas app (the vendored esbuild; no npm). `node --test` runs test
 *  FILES CONCURRENTLY and every one packs into the same `dist/canvas.zip`, so a
 *  cross-process LOCK (an atomic mkdir) serialises it, and a zip newer than every
 *  source it is built from is reused rather than rebuilt under another reader. */
let packed = false;
export function packCanvas(): void {
  if (packed) return;
  const lock = join(CANVAS_APP, "dist", ".pack.lock");
  mkdirSync(join(CANVAS_APP, "dist"), { recursive: true });
  const deadline = Date.now() + 120_000;
  for (;;) {
    try {
      mkdirSync(lock);
      break;
    } catch {
      if (Date.now() > deadline) throw new Error(`canvas pack: ${lock} held for 120 s`);
      spawnSync("sleep", ["0.2"]);
    }
  }
  try {
    const sources = Math.max(
      newest(join(CANVAS_APP, "src")),
      newest(join(CANVAS_APP, "scripts")),
      newest(join(repoRoot, "src", "lib", "ts", "src")),
    );
    let fresh = false;
    try {
      fresh = statSync(CANVAS_ZIP).mtimeMs > sources;
    } catch {
      /* no zip yet */
    }
    if (!fresh) {
      const r = spawnSync("sh", ["scripts/pack.sh"], { cwd: CANVAS_APP, encoding: "utf8" });
      if (r.status !== 0) throw new Error(`canvas pack failed: ${r.stderr || r.stdout}`);
    }
    packed = true;
  } finally {
    rmSync(lock, { recursive: true, force: true });
  }
}

/** Write a fixture file beside the canvas page — served at
 *  `/canvas_page/file/<name>` for THIS host (the workdir copy, in both targets:
 *  a container's /work is the same dir). */
export function writeServedDist(host: Host, name: string, content: string): void {
  writeFileSync(join(host.tmp, "canvas_page", name), content);
}

/** A record the page's loader holds: `agent.yaml` under `.fantastic/web/…`.
 *  JSON is YAML, so a fixture writes it as JSON; the kernels read either. */
export function writeRecord(dir: string, rec: Record<string, unknown>): void {
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, "agent.yaml"), JSON.stringify(rec));
}

/** One top-level scalar of a kernel-written `agent.yaml` (block YAML), or
 *  undefined — enough for a test's field checks, with no YAML dependency. */
export function recordField(file: string, key: string): string | undefined {
  const m = readFileSync(file, "utf8").match(new RegExp(`^${key}:\\s*(.*)$`, "m"));
  if (m === null) return undefined;
  const v = (m[1] as string).trim();
  return v.replace(/^(['"])(.*)\1$/, "$2");
}

// ─── LLM backend selection — ONE env switch (`LLM_BACKEND`: `vllm` | `ollama`,
//     NO default) so every live-LLM itest runs against either a vLLM server or a
//     local ollama model (parity). Unset, every live-LLM test SKIPS, naming the
//     variable (`LLM_SKIP`); any other value fails at import. The in-kernel AI
//     agent is created identically via `bootHost({ llm })`; only the bundle/record
//     differ, so the whole suite is reused verbatim, no fork. ───
const LLM_RAW = (process.env.LLM_BACKEND ?? "").trim().toLowerCase();
if (LLM_RAW !== "" && LLM_RAW !== "vllm" && LLM_RAW !== "ollama") {
  throw new Error(
    `LLM_BACKEND=${JSON.stringify(process.env.LLM_BACKEND)} is not vllm or ollama — set one, or unset it to skip the live-LLM tests`,
  );
}
export const LLM_BACKEND: "vllm" | "ollama" | undefined =
  LLM_RAW === "" ? undefined : (LLM_RAW as "vllm" | "ollama");

/** Why a live-LLM test skips: `LLM_BACKEND` unset (named), or the selected
 *  backend unreachable. */
export const LLM_SKIP =
  LLM_BACKEND === undefined
    ? "LLM_BACKEND is unset — set it to vllm or ollama to run the live-LLM tests"
    : `${LLM_BACKEND} backend unreachable (key/server/model)`;

/** The vLLM record fields that have NO default anywhere — the backend refuses a
 *  record without them, so an unset one skips the live tests (`llmReachable`). */
const VLLM_REQUIRED = ["VLLM_ENDPOINT", "FANTASTIC_AI_MODEL", "VLLM_KV_SLOTS", "VLLM_KV_OVERFLOW"];

/** The active LLM agent config for `bootHost({ llm })`, chosen by `LLM_BACKEND`;
 *  undefined while it is unset (every live-LLM test skips).
 *  `vllm` → vllm_backend: the server's `/v1` URL (`VLLM_ENDPOINT`), the
 *  served model (`FANTASTIC_AI_MODEL`) and the KV session table's size and policy
 *  (`VLLM_KV_SLOTS`, `VLLM_KV_OVERFLOW`) — all required, nothing guessed; a turn's
 *  `max_tokens` is the e2e suite's one budget (65536, `MAX_TOKENS` overrides).
 *  `ollama` → ollama_backend at the MODEL'S FULL context (gemma4:12b = 262144 /
 *  256K) — proven to fit RAM on the 32GB box (resident KV is usage-based, ~8GB at
 *  low fill), not an invented cap. Override the model/ctx via env for a
 *  different model. */
export const ACTIVE_LLM: BootOptions["llm"] =
  LLM_BACKEND === "ollama"
    ? {
        bundle: "ollama_backend.tools",
        model: process.env.OLLAMA_MODEL ?? "gemma4:12b",
        num_ctx: Number(process.env.OLLAMA_NUM_CTX ?? 262144),
        endpoint: process.env.OLLAMA_ENDPOINT, // unset → ollama_backend default :11434
      }
    : LLM_BACKEND === "vllm"
      ? {
          bundle: "vllm_backend.tools",
          model: process.env.FANTASTIC_AI_MODEL,
          endpoint: process.env.VLLM_ENDPOINT,
          max_tokens: Number(process.env.MAX_TOKENS ?? 65536),
          kv_slots: process.env.VLLM_KV_SLOTS ? Number(process.env.VLLM_KV_SLOTS) : undefined,
          kv_overflow: process.env.VLLM_KV_OVERFLOW,
        }
      : undefined;

/** The keyed bundles: the env var their key is read from — by the `creds` STORE.
 *  A bundle in OPTIONAL_KEY is wired to the store only when its variable is set:
 *  a vLLM server started without `--api-key` takes no key at all. */
const KEY_VAR: Record<string, string> = {
  "responses_backend.tools": "OPENAI_API_KEY",
  "vllm_backend.tools": "VLLM_API_KEY",
};
const OPTIONAL_KEY = new Set(["vllm_backend.tools"]);

/** The env var `bundle`'s key is read from, or undefined when it takes none. */
function keyVarFor(bundle: string): string | undefined {
  const v = KEY_VAR[bundle];
  if (v === undefined) return undefined;
  return OPTIONAL_KEY.has(bundle) && !dotenvKey(v) ? undefined : v;
}

/** Is the active backend reachable? (skip — never hang/fail — when not; the reason
 *  is `LLM_SKIP`.) `LLM_BACKEND` unset: no. ollama: the server answers `/api/tags`
 *  AND has the model pulled. vllm: every required variable is set AND the server's
 *  `/models` lists the model (with the key as a Bearer header when `VLLM_API_KEY`
 *  is set). */
export async function llmReachable(): Promise<boolean> {
  const llm = ACTIVE_LLM;
  if (llm === undefined) return false;
  if (LLM_BACKEND === "ollama") {
    const raw = process.env.OLLAMA_HOST ?? "127.0.0.1:11434";
    const base = raw.startsWith("http") ? raw : `http://${raw}`;
    try {
      const r = await fetch(`${base}/api/tags`, { signal: AbortSignal.timeout(3000) });
      if (!r.ok) return false;
      const j = (await r.json()) as { models?: { name?: string }[] };
      const want = llm.model ?? "";
      return (j.models ?? []).some(
        (m) => m.name === want || (m.name ?? "").startsWith(`${want.split(":")[0]}:`),
      );
    } catch {
      return false;
    }
  }
  if (VLLM_REQUIRED.some((v) => !process.env[v])) return false;
  const key = dotenvKey("VLLM_API_KEY");
  try {
    const r = await fetch(`${(llm.endpoint as string).replace(/\/+$/, "")}/models`, {
      headers: key ? { Authorization: `Bearer ${key}` } : {},
      signal: AbortSignal.timeout(3000),
    });
    if (!r.ok) return false;
    const j = (await r.json()) as { data?: { id?: string }[] };
    return (j.data ?? []).some((m) => m.id === llm.model);
  } catch {
    return false;
  }
}

/** The daemon's env: process env + repo `.env`, but with the NON-selected
 *  backend's secret STRIPPED. Only an `LLM_BACKEND=vllm` run carries
 *  VLLM_API_KEY — in an ollama run (or with LLM_BACKEND unset) any stray keyed
 *  `vllm_backend` agent fails loudly instead of silently "leaking" the vLLM
 *  server into the comparison. */
function daemonEnv(): NodeJS.ProcessEnv {
  const env: NodeJS.ProcessEnv = { ...process.env, ...envFromDotenv() };
  if (LLM_BACKEND !== "vllm") {
    delete env.VLLM_API_KEY;
  }
  return env;
}

function fantasticAvailable(): boolean {
  if (TARGET === "container") {
    return spawnSync(ENGINE, ["image", "inspect", IMAGE], { stdio: "ignore" }).status === 0;
  }
  try {
    return statSync(FANTASTIC).isFile();
  } catch {
    return false;
  }
}

function runCli(tmp: string, args: string[]): string {
  // container: one-shot inside the image, bypassing the dispatch entrypoint,
  // against the bind-mounted /work (so records are written by the container's
  // own uid, same as the daemon). local: the historical subprocess path.
  const r =
    TARGET === "container"
      ? spawnSync(
          ENGINE,
          ["run", "--rm", "-v", `${tmp}:/work`, "-w", "/work", "--entrypoint", CONTAINER_BIN, IMAGE, ...args],
          { encoding: "utf8" },
        )
      : spawnSync(FANTASTIC, args, { cwd: tmp, encoding: "utf8" });
  if (r.status !== 0) {
    throw new Error(`fantastic ${args.join(" ")} failed: ${r.stderr || r.stdout}`);
  }
  return `${r.stdout}\n${r.stderr}`;
}

function extractId(out: string, prefix: string): string {
  const m = out.match(new RegExp(`"id":\\s*"(${prefix}_[0-9a-f]+)"`));
  if (m === null) throw new Error(`could not find ${prefix}_* id in: ${out}`);
  return m[1] as string;
}

async function waitForPort(port: number, ms: number): Promise<void> {
  const start = Date.now();
  while (Date.now() - start < ms) {
    try {
      const res = await fetch(`http://127.0.0.1:${port}/_assets/favicon.png`);
      if (res.ok) return;
    } catch {
      // not up yet
    }
    await new Promise((r) => setTimeout(r, 150));
  }
  throw new Error(`host did not come up on :${port} within ${ms}ms`);
}

/** Boot a host; throws (→ caller skips) if the python env isn't available.
 *  All agents are created via locked-out one-shots BEFORE the daemon spawns. */
export async function bootHost(port = 8911, opts: BootOptions = {}): Promise<Host> {
  if (!fantasticAvailable()) {
    throw new Error(`no fantastic bin at ${FANTASTIC}`);
  }
  // container mode bind-mounts the workdir, so it MUST live on a path the
  // podman/docker VM mounts. The OS tmpdir is often a harness-set TMPDIR (e.g.
  // /tmp/claude-501) that the VM can't see → use a repo-relative dir ($HOME is
  // mounted). local mode keeps the OS tmpdir.
  const tmpBase =
    TARGET === "container" ? join(repoRoot, "integration_tests", "py_ts", "tmp") : tmpdir();
  if (TARGET === "container") mkdirSync(tmpBase, { recursive: true });
  const tmp = mkdtempSync(join(tmpBase, "ftbridge-"));
  // Persistence provider FIRST. Under the no-fallback rule, kernel_state
  // auto-persists ONLY through a discovered file_bridge@.fantastic; without one the
  // one-shot seeds below stay in RAM and never reach disk, so the spawned daemon
  // boots empty. Seeding the store first makes the whole seeded tree durable.
  // Kernel verbs are answered by the ROOT and take an explicit `parent_id` (the
  // agent named is the operand, not the actor).
  runCli(tmp, [
    "kernel",
    "create_agent",
    "parent_id=kernel_state",
    "handler_module=file_bridge.tools",
    "id=store",
    "root=.fantastic",
    'allow=["."]',
    "ingress_rule=allow_all_ingress",
  ]);
  // THE MEMORY STORE: a started kernel (the daemon) refuses to run without one —
  // the root names it in `memory_store_id`. A ram_store serves the operator
  // (`kernel`) and lives as long as the daemon, which is all a test needs.
  runCli(tmp, [
    "kernel",
    "create_agent",
    "parent_id=kernel_state",
    "handler_module=ram_store.tools",
    "id=memory",
    "operator=true",
    'admins=["kernel"]',
  ]);
  runCli(tmp, ["kernel", "update_agent", "id=kernel_state", "memory_store_id=memory"]);
  const webOut = runCli(tmp, [
    "kernel",
    "create_agent",
    "parent_id=kernel_state",
    "handler_module=web.tools",
    `port=${port}`,
  ]);
  const webId = extractId(webOut, "web");
  // nest web_ws by TARGETING the web agent (disk placement = tree parent).
  // The WS verb surface is an io leg — it SEALS by default (deny-all flip), so a
  // bare web_ws refuses every forwarded call. Open it (allow_all): this is the
  // drivable test surface, mirroring the python `seed_web_ws` helper.
  const wsOut = runCli(tmp, [
    "kernel",
    "create_agent",
    `parent_id=${webId}`,
    "handler_module=web_ws.tools",
    "ingress_rule=allow_all_ingress",
  ]);
  const webWsId = extractId(wsOut, "web_ws");
  // optional frontend store: a SECOND kernel_state rooted at .fantastic/web,
  // reachable over WS by its id, `web_loader`. Created BEFORE spawn (the
  // one-shot CLI is locked out once the daemon owns the dir). A CHILD OF THE WS
  // LEG: the page's record verbs arrive as the leg, and the record-owner guard
  // admits only records inside the caller's own subtree. The page learns its
  // leg from this record's parent.
  let webLoaderId: string | undefined;
  if (opts.webLoader === true) {
    const wlOut = runCli(tmp, [
      "kernel",
      "create_agent",
      `parent_id=${webWsId}`,
      "handler_module=kernel_state.tools",
      "id=web_loader",
      "root=.fantastic/web",
      "watch=false",
    ]);
    if (!/"id":\s*"web_loader"/.test(wlOut)) throw new Error(`web_loader create failed: ${wlOut}`);
    webLoaderId = "web_loader";
  }

  // optional canvas page: packed, its bundle extracted into the workdir
  // (file_bridge clamps every root to the running dir; a container's /work is
  // this same dir) beside the one-tag page, served under web.
  let pageUrl: string | undefined;
  if (opts.serveDist === true) {
    packCanvas();
    const page = join(tmp, "canvas_page");
    mkdirSync(page, { recursive: true });
    const unz = spawnSync("unzip", ["-o", "-q", CANVAS_ZIP, "bundle.min.js", "-d", page], { encoding: "utf8" });
    if (unz.status !== 0) throw new Error(`unzip ${CANVAS_ZIP}: ${unz.stderr || unz.stdout}`);
    writeFileSync(join(page, "index.html"), CANVAS_INDEX);
    runCli(tmp, [
      "kernel",
      "create_agent",
      `parent_id=${webId}`,
      "handler_module=file_bridge.tools",
      "id=canvas_page",
      "root=canvas_page",
      'allow=["."]',
      "readonly=true",
      "ingress_rule=allow_all_ingress", // the fs edge seals by default
    ]);
    pageUrl = `http://127.0.0.1:${port}/canvas_page/file/index.html`;
  }

  // THE HOST AGENTS the page reaches. Every ws frame is dispatched AS the leg,
  // so each one is granted on BOTH halves: the leg's `can_call` names it, and
  // its `callable_by` names the leg (deny-all everywhere; no defaults).
  const reach: Record<string, unknown> = {};
  // who, besides the leg, the ROOT accepts (an AI given kernel reach)
  const rootCallers: string[] = [];

  // optional command runner: sealed, and gated by its `allowed` regexes — here
  // exactly `<the venv python> -c <code>`.
  let pyId: string | undefined;
  let python: string | undefined;
  if (opts.execBridge === true || opts.llm !== undefined) {
    python = PY_INTERP;
    const line = `${escapeRegex(python)} -c .*`;
    runCli(tmp, [
      "kernel",
      "create_agent",
      "parent_id=kernel_state",
      "handler_module=exec_bridge.tools",
      "id=exec",
      `allowed=${JSON.stringify([line])}`,
      // where a command runs is the record's; the host's own tmp dir
      `cwd=${tmp}`,
      "ingress_rule=allow_all_ingress",
      `callable_by=${JSON.stringify({ $self: "*", $parent: "*", $children: "*", [webWsId]: "*" })}`,
    ]);
    pyId = "exec";
    reach[pyId] = "*";
  }

  // optional LLM agent: its conversation lives in the kernel's memory store,
  // gated both ways (the agent may call the store, the store admits it).
  let ollamaId: string | undefined;
  if (opts.llm !== undefined) {
    const bundle = opts.llm.bundle ?? "ollama_backend.tools";
    const more = opts.llm.reach ?? {};
    // THE KEY, for a keyed bundle: an env-backed `creds` store, one ref bound to
    // the endpoint's host, readable by `llm` alone; the record names it.
    const keyVar = keyVarFor(bundle);
    if (keyVar !== undefined) {
      runCli(tmp, [
        "kernel",
        "create_agent",
        "parent_id=kernel_state",
        "handler_module=credential_store.tools",
        "id=creds",
        "backend=env",
        `refs=${JSON.stringify({ llm: { host: opts.llm.endpoint, env: { api_key: keyVar }, readers: ["llm"] } })}`,
        `callable_by=${JSON.stringify({ $self: "*", $parent: "*", $children: "*", llm: ["get"] })}`,
      ]);
    }
    const args = [
      "kernel",
      "create_agent",
      "parent_id=kernel_state",
      `handler_module=${bundle}`,
      "id=llm",
      "memory_store_id=memory",
      `can_call=${JSON.stringify({ $self: "*", $parent: "*", $children: "*", memory: "*", ...(keyVar !== undefined ? { creds: ["get"] } : {}), ...(pyId ? { [pyId]: "*" } : {}), ...more })}`,
      `callable_by=${JSON.stringify({ $self: "*", $parent: "*", $children: "*", [webWsId]: "*" })}`,
    ];
    if (keyVar !== undefined) args.push(`credentials=${JSON.stringify({ store: "creds", ref: "llm" })}`);
    if (opts.llm.model) args.push(`model=${opts.llm.model}`);
    if (opts.llm.endpoint) args.push(`endpoint=${opts.llm.endpoint}`);
    if (opts.llm.num_ctx) args.push(`num_ctx=${opts.llm.num_ctx}`);
    if (opts.llm.max_tokens) args.push(`max_tokens=${opts.llm.max_tokens}`);
    if (opts.llm.kv_slots) args.push(`kv_slots=${opts.llm.kv_slots}`);
    if (opts.llm.kv_overflow) args.push(`kv_overflow=${opts.llm.kv_overflow}`);
    runCli(tmp, args);
    runCli(tmp, [
      "kernel",
      "update_agent",
      "id=memory",
      `callable_by=${JSON.stringify({ $self: "*", $parent: "*", $children: "*", llm: "*" })}`,
    ]);
    ollamaId = "llm";
    reach[ollamaId] = "*";
    for (const target of Object.keys(more)) {
      if (target === "kernel_state") {
        rootCallers.push("llm");
        continue;
      }
      if (target === pyId) continue; // the exec leg's callable_by is written below
      runCli(tmp, [
        "kernel",
        "update_agent",
        `id=${target}`,
        `callable_by=${JSON.stringify({ $self: "*", $parent: "*", $children: "*", [webWsId]: "*", llm: "*" })}`,
      ]);
    }
  }

  // optional scheduler: it persists its schedules THROUGH the root loader (no
  // storage leg of its own) and fires a chosen payload to a target.
  let schedulerId: string | undefined;
  if (opts.scheduler === true) {
    runCli(tmp, [
      "kernel",
      "create_agent",
      "parent_id=kernel_state",
      "handler_module=scheduler.tools",
      "id=scheduler",
      `can_call=${JSON.stringify({ $self: "*", $parent: "*", $children: "*", ...(pyId ? { [pyId]: "*" } : {}) })}`,
      `callable_by=${JSON.stringify({ $self: "*", $parent: "*", $children: "*", [webWsId]: "*" })}`,
    ]);
    schedulerId = "scheduler";
    reach[schedulerId] = "*";
    if (pyId) {
      runCli(tmp, [
        "kernel",
        "update_agent",
        `id=${pyId}`,
        `callable_by=${JSON.stringify({ $self: "*", $parent: "*", $children: "*", [webWsId]: "*", scheduler: "*", ...(ollamaId ? { llm: "*" } : {}) })}`,
      ]);
    }
  } else if (pyId && ollamaId) {
    runCli(tmp, [
      "kernel",
      "update_agent",
      `id=${pyId}`,
      `callable_by=${JSON.stringify({ $self: "*", $parent: "*", $children: "*", [webWsId]: "*", llm: "*" })}`,
    ]);
  }

  // THE LEG'S RIGHTS, both halves: it reaches the root (privileged verbs named
  // — never under `*`), web, the memory store and every optional agent above;
  // the root accepts it.
  const BASE = { $self: "*", $parent: "*", $children: "*" };
  const ROOT_VERBS = ["*", "create_agent", "update_agent", "delete_agent"];
  runCli(tmp, [
    "kernel",
    "update_agent",
    `id=${webWsId}`,
    // its loader BY NAME: `load_tree`/`persist_record` are privileged, and the
    // structural `$children` never covers a privileged verb on egress
    `can_call=${JSON.stringify({ ...BASE, kernel_state: ROOT_VERBS, [webId]: "*", memory: "*", ...(webLoaderId ? { [webLoaderId]: "*" } : {}), ...reach })}`,
  ]);
  runCli(tmp, [
    "kernel",
    "update_agent",
    "id=kernel_state",
    `callable_by=${JSON.stringify({ ...BASE, [webWsId]: "*", ...Object.fromEntries(rootCallers.map((c) => [c, "*"])) })}`,
  ]);

  // Spawn the daemon. container: `podman/docker run -d` (publishes the port,
  // mounts the workdir + dist). local: the venv binary, with the repo `.env`
  // (e.g. VLLM_API_KEY) injected — it runs in `tmp`, so the cwd-relative
  // `_load_dotenv` wouldn't find it otherwise.
  let proc: ChildProcess | undefined;
  let container: string | undefined;
  if (TARGET === "container") {
    container = `fte2e-${port}-${process.pid}`;
    spawnSync(ENGINE, ["rm", "-f", container], { stdio: "ignore" });
    const r = spawnSync(
      ENGINE,
      containerRunArgs(container, port, tmp),
      { encoding: "utf8" },
    );
    if (r.status !== 0) {
      throw new Error(`container start failed (:${port}): ${r.stderr || r.stdout}`);
    }
  } else {
    proc = spawn(FANTASTIC, [], {
      cwd: tmp,
      stdio: "ignore",
      env: daemonEnv(),
    });
    proc.unref();
  }
  const host: Host = {
    origin: `ws://127.0.0.1:${port}`,
    httpOrigin: `http://127.0.0.1:${port}`,
    port,
    webId,
    webWsId,
    webLoaderId,
    pyId,
    ollamaId,
    schedulerId,
    tmp,
    proc,
    container,
    pageUrl,
    python,
  };
  try {
    await waitForPort(port, 30000);
  } catch (e) {
    teardownHost(host);
    throw e;
  }
  return host;
}

export function teardownHost(host: Host): void {
  try {
    if (host.container) {
      // never kill the container-internal pid — stop by name (tini → graceful).
      spawnSync(ENGINE, ["stop", "-t", "8", host.container], { stdio: "ignore" });
      spawnSync(ENGINE, ["rm", "-f", host.container], { stdio: "ignore" });
    } else {
      host.proc?.kill("SIGTERM");
    }
  } catch {
    /* already gone */
  }
  try {
    rmSync(host.tmp, { recursive: true, force: true });
  } catch {
    /* best effort */
  }
}

/** Restart the daemon IN PLACE: kill the running process, wait for its port to
 *  free, then re-spawn `fantastic` in the SAME tmp dir — the persisted `.fantastic/`
 *  rehydrates from disk. Mutates `host.proc`. Used to test save/load across a real
 *  daemon restart (the host kernel_state + the frontend store + the browser re-hydrate). */
export async function restartHost(host: Host): Promise<void> {
  try {
    if (host.container) {
      spawnSync(ENGINE, ["rm", "-f", host.container], { stdio: "ignore" });
    } else {
      host.proc?.kill("SIGTERM");
    }
  } catch {
    /* already gone */
  }
  // wait for the old daemon to release the port (fetch succeeds = still up)
  for (let i = 0; i < 50; i++) {
    try {
      await fetch(`http://127.0.0.1:${host.port}/`);
      await new Promise((r) => setTimeout(r, 200));
    } catch {
      break;
    }
  }
  if (host.container) {
    // re-create the SAME container (same name + mounts) — the workdir tmp on
    // the host persists, so the kernel rehydrates from disk exactly as local.
    const r = spawnSync(
      ENGINE,
      containerRunArgs(host.container, host.port, host.tmp),
      { encoding: "utf8" },
    );
    if (r.status !== 0) {
      throw new Error(`container restart failed: ${r.stderr || r.stdout}`);
    }
  } else {
    const proc = spawn(FANTASTIC, [], {
      cwd: host.tmp,
      stdio: "ignore",
      env: daemonEnv(),
    });
    proc.unref();
    (host as { proc: ChildProcess }).proc = proc;
  }
  await waitForPort(host.port, 30000);
}
