// Integration-test harness: boot a real python `fantastic` host (web + a
// nested web_ws surface) in a throwaway temp dir, and tear it down. Used by
// *.itest.ts. NOT a *.test.ts file, so the unit run never imports it.
//
// web_ws must be nested UNDER the web agent on disk (create by targeting the
// web agent, not fs_loader) — the tree comes from disk placement, and web mounts
// only its own children's get_routes. See bridge.ts's federation note.

import { spawn, spawnSync } from "node:child_process";
import type { ChildProcess } from "node:child_process";
import { mkdtempSync, mkdirSync, rmSync, statSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = fileURLToPath(new URL("../../", import.meta.url));
const FANTASTIC = join(repoRoot, "python", ".venv", "bin", "fantastic");

// Target selection — mirrors the python integration harness. `local` (default)
// runs the built venv binary; `container` runs the universal image (python
// runtime). The SAME tests run either way: seeding one-shots run INSIDE the
// container (a rootless container's uid can't write a host-seeded .fantastic/),
// the daemon publishes -p 127.0.0.1:port:port with FANTASTIC_HEAD=off, and the
// frontend dist is bind-mounted in so the `ts_dist` file agent can serve it.
// e2e is host/browser → container only (no container↔container), so -p suffices.
const TARGET = (process.env.FANTASTIC_TARGET ?? "local").trim().toLowerCase();
const IMAGE = process.env.FANTASTIC_IMAGE ?? "fantastic:latest";
const CONTAINER_BIN = "/opt/fantastic/venv/bin/fantastic"; // python in the image
const CONTAINER_DIST = "/dist"; // where ts/dist is mounted in container mode

function resolveEngine(): string {
  for (const e of ["podman", "docker"]) {
    if (spawnSync(e, ["--version"], { stdio: "ignore" }).status === 0) return e;
  }
  throw new Error("FANTASTIC_TARGET=container but no podman/docker found");
}
const ENGINE = TARGET === "container" ? resolveEngine() : "";

/** Daemon `run` args for container mode — shared by bootHost + restartHost so a
 *  restart re-creates an identical container (same name, port, mounts). */
function containerRunArgs(name: string, port: number, tmp: string, serveDist: boolean): string[] {
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
    "-e",
    "FANTASTIC_HEAD=off",
  ];
  if (serveDist) args.push("-v", `${DIST_DIR}:${CONTAINER_DIST}:ro`);
  // Forward LLM keys for the (opt-in, paid) live-LLM tests.
  const env = envFromDotenv();
  // Forward ONLY the selected backend's secret (no anthropic key into an ollama
  // run) — same no-leak guarantee as daemonEnv(), for the container target.
  const forward =
    LLM_BACKEND === "ollama"
      ? ["OLLAMA_HOST"]
      : ["ANTHROPIC_KEY", "ANTHROPIC_API_KEY", "OLLAMA_HOST"];
  for (const k of forward) {
    if (env[k]) args.push("-e", `${k}=${env[k]}`);
  }
  args.push(IMAGE);
  return args;
}

/** Parse the repo `.env` into a {KEY: value} map (so the spawned daemon — which
 *  runs in a tmp cwd and can't see the repo `.env` — gets e.g. ANTHROPIC_KEY in
 *  its environment). Stdlib-only, mirrors the python `_load_dotenv` rules. */
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
 *  live-LLM tests, e.g. `dotenvKey("ANTHROPIC_KEY")`. */
export function dotenvKey(name: string): string | undefined {
  return process.env[name] ?? envFromDotenv()[name];
}

export interface Host {
  origin: string;
  port: number;
  /** http origin (same port as the WS) — serves the static frontend when
   *  `serveDist` was set: open `${httpOrigin}/ts_dist/file/<path>`. */
  httpOrigin: string;
  webId: string;
  webWsId: string;
  /** id of the `web_loader` (the frontend store), if `bootHost` was asked to
   *  create one — reach it over WS by its `web_loader` alias. */
  webLoaderId?: string;
  /** id of a `python_runtime` agent, if `pythonRuntime` was set. */
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
  /** whether the frontend dist was mounted/served (needed to re-create the
   *  container identically on restart). */
  serveDist?: boolean;
}

export interface BootOptions {
  /** Also create a `web_loader` (an fs_loader rooted at .fantastic/web, alias
   *  `web_loader`) under the web agent — the frontend's persistence store. */
  webLoader?: boolean;
  /** Also serve the built frontend (`ts/dist`) via a `file` agent (id
   *  `ts_dist`) under web — its ESM mounts at `/ts_dist/file/<path>`, on the
   *  same port as the WS. Requires `npm run build` to have run. */
  serveDist?: boolean;
  /** Also create a `python_runtime` agent (the background-script runner). */
  pythonRuntime?: boolean;
  /** Also create an LLM backend agent (+ its history `file` agent). `bundle`
   *  picks the backend (default `ollama_backend.tools`; e.g.
   *  `anthropic_backend.tools` for Claude). `model`/`endpoint` are passed only
   *  when set (each bundle defaults its own). The repo `.env` is injected into
   *  the daemon env, so a key like ANTHROPIC_KEY reaches the backend. */
  llm?: { bundle?: string; model?: string; endpoint?: string; num_ctx?: number };
  /** Also create a `scheduler` agent (+ a `file` agent `sched_files` for its
   *  schedules) — fire a chosen payload to a target after N seconds. */
  scheduler?: boolean;
}

/** Absolute path to the built frontend (`ts/dist`) — served when `serveDist`. */
export const DIST_DIR = join(repoRoot, "ts", "dist");

// ─── LLM backend selection — ONE env switch (`LLM_BACKEND`) so every live-LLM
//     itest runs against either Claude or a local ollama model (parity). The
//     in-kernel AI agent is created identically via `bootHost({ llm })`; only the
//     bundle/model differ, so the whole suite is reused verbatim, no fork. ───
export const LLM_BACKEND = (process.env.LLM_BACKEND ?? "claude").toLowerCase();

/** The active LLM agent config for `bootHost({ llm })`, chosen by `LLM_BACKEND`.
 *  `claude` (default) → anthropic_backend; `ollama` → ollama_backend at the
 *  MODEL'S FULL context (gemma4:12b = 262144 / 256K) — proven to fit RAM on the
 *  32GB box (resident KV is usage-based, ~8GB at low fill), not an invented cap.
 *  Override the model/ctx via env for a different model. */
export const ACTIVE_LLM: NonNullable<BootOptions["llm"]> =
  LLM_BACKEND === "ollama"
    ? {
        bundle: "ollama_backend.tools",
        model: process.env.OLLAMA_MODEL ?? "gemma4:12b",
        num_ctx: Number(process.env.OLLAMA_NUM_CTX ?? 262144),
        endpoint: process.env.OLLAMA_ENDPOINT, // unset → ollama_backend default :11434
      }
    : {
        bundle: "anthropic_backend.tools",
        model: process.env.ANTHROPIC_MODEL ?? "claude-opus-4-8",
      };

/** Is the active backend reachable? (skip — never hang/fail — when not.) ollama:
 *  the server answers `/api/tags` AND has the model pulled. Claude: a key in
 *  `.env` AND api.anthropic.com reachable. */
export async function llmReachable(): Promise<boolean> {
  if (LLM_BACKEND === "ollama") {
    const raw = process.env.OLLAMA_HOST ?? "127.0.0.1:11434";
    const base = raw.startsWith("http") ? raw : `http://${raw}`;
    try {
      const r = await fetch(`${base}/api/tags`, { signal: AbortSignal.timeout(3000) });
      if (!r.ok) return false;
      const j = (await r.json()) as { models?: { name?: string }[] };
      const want = ACTIVE_LLM.model ?? "";
      return (j.models ?? []).some(
        (m) => m.name === want || (m.name ?? "").startsWith(`${want.split(":")[0]}:`),
      );
    } catch {
      return false;
    }
  }
  if (!dotenvKey("ANTHROPIC_KEY") && !dotenvKey("ANTHROPIC_API_KEY")) return false;
  try {
    await fetch("https://api.anthropic.com/", { signal: AbortSignal.timeout(3000) });
    return true;
  } catch {
    return false;
  }
}

/** The daemon's env: process env + repo `.env`, but with the NON-selected
 *  backend's secret STRIPPED. An `LLM_BACKEND=ollama` run therefore carries NO
 *  ANTHROPIC_KEY — Claude cannot run, and any stray `anthropic_backend` agent
 *  fails loudly instead of silently "leaking" Claude into the ollama comparison. */
function daemonEnv(): NodeJS.ProcessEnv {
  const env: NodeJS.ProcessEnv = { ...process.env, ...envFromDotenv() };
  if (LLM_BACKEND === "ollama") {
    delete env.ANTHROPIC_KEY;
    delete env.ANTHROPIC_API_KEY;
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
      const res = await fetch(`http://127.0.0.1:${port}/`);
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
  const webOut = runCli(tmp, [
    "fs_loader",
    "create_agent",
    "handler_module=web.tools",
    `port=${port}`,
  ]);
  const webId = extractId(webOut, "web");
  // nest web_ws by TARGETING the web agent (disk placement = tree parent)
  const wsOut = runCli(tmp, [webId, "create_agent", "handler_module=web_ws.tools"]);
  const webWsId = extractId(wsOut, "web_ws");

  // optional frontend store: a SECOND fs_loader rooted at .fantastic/web,
  // reachable over WS by its `web_loader` alias. Created BEFORE spawn (the
  // one-shot CLI is locked out once the daemon owns the dir).
  let webLoaderId: string | undefined;
  if (opts.webLoader === true) {
    const wlOut = runCli(tmp, [
      webId,
      "create_agent",
      "handler_module=fs_loader.tools",
      "root=.fantastic/web",
      "watch=false",
      "alias=web_loader",
    ]);
    webLoaderId = extractId(wlOut, "fs_loader");
  }

  // optional static frontend: a `file` agent (fixed id `ts_dist`) rooted at the
  // build output. Its route mounts under web → `/ts_dist/file/<path>` serves
  // every ESM module on the SAME port as the WS surface.
  if (opts.serveDist === true) {
    // container mode serves the dist from its bind-mount path; local from the
    // host build dir. (The record only stores the path; the daemon — which has
    // the mount — does the reading.)
    const distRoot = TARGET === "container" ? CONTAINER_DIST : DIST_DIR;
    runCli(tmp, [
      webId,
      "create_agent",
      "handler_module=file.tools",
      "id=ts_dist",
      `root=${distRoot}`,
    ]);
  }

  // optional background-script runner (a HOST peer, not under web).
  let pyId: string | undefined;
  if (opts.pythonRuntime === true || opts.llm !== undefined) {
    const pyOut = runCli(tmp, [
      "fs_loader",
      "create_agent",
      "handler_module=python_runtime.tools",
    ]);
    pyId = extractId(pyOut, "python_runtime");
  }

  // optional LLM agent: a `file` agent for its chat-history sidecar + the
  // chosen backend bundle. model/endpoint passed only when set (each bundle
  // defaults its own).
  let ollamaId: string | undefined;
  if (opts.llm !== undefined) {
    const bundle = opts.llm.bundle ?? "ollama_backend.tools";
    runCli(tmp, [
      "fs_loader",
      "create_agent",
      "handler_module=file.tools",
      "id=llm_files",
      "root=.fantastic",
    ]);
    const args = [
      "fs_loader",
      "create_agent",
      `handler_module=${bundle}`,
      "file_agent_id=llm_files",
    ];
    if (opts.llm.model) args.push(`model=${opts.llm.model}`);
    if (opts.llm.endpoint) args.push(`endpoint=${opts.llm.endpoint}`);
    if (opts.llm.num_ctx) args.push(`num_ctx=${opts.llm.num_ctx}`);
    const out = runCli(tmp, args);
    ollamaId = extractId(out, bundle.replace(/\.tools$/, ""));
  }

  // optional scheduler: a `file` agent for its schedules + the scheduler agent.
  let schedulerId: string | undefined;
  if (opts.scheduler === true) {
    runCli(tmp, [
      "fs_loader",
      "create_agent",
      "handler_module=file.tools",
      "id=sched_files",
      "root=.fantastic",
    ]);
    const schedOut = runCli(tmp, [
      "fs_loader",
      "create_agent",
      "handler_module=scheduler.tools",
      "file_agent_id=sched_files",
    ]);
    schedulerId = extractId(schedOut, "scheduler");
  }

  // Spawn the daemon. container: `podman/docker run -d` (publishes the port,
  // mounts the workdir + dist). local: the venv binary, with the repo `.env`
  // (e.g. ANTHROPIC_KEY) injected — it runs in `tmp`, so the cwd-relative
  // `_load_dotenv` wouldn't find it otherwise.
  let proc: ChildProcess | undefined;
  let container: string | undefined;
  if (TARGET === "container") {
    container = `fte2e-${port}-${process.pid}`;
    spawnSync(ENGINE, ["rm", "-f", container], { stdio: "ignore" });
    const r = spawnSync(
      ENGINE,
      containerRunArgs(container, port, tmp, opts.serveDist === true),
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
    serveDist: opts.serveDist === true,
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
 *  daemon restart (the host fs_loader + the frontend store + the browser re-hydrate). */
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
      containerRunArgs(host.container, host.port, host.tmp, host.serveDist === true),
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
