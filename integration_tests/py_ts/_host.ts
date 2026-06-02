// Integration-test harness: boot a real python `fantastic` host (web + a
// nested web_ws surface) in a throwaway temp dir, and tear it down. Used by
// *.itest.ts. NOT a *.test.ts file, so the unit run never imports it.
//
// web_ws must be nested UNDER the web agent on disk (create by targeting the
// web agent, not fs_loader) — the tree comes from disk placement, and web mounts
// only its own children's get_routes. See bridge.ts's federation note.

import { spawn, spawnSync } from "node:child_process";
import type { ChildProcess } from "node:child_process";
import { mkdtempSync, rmSync, statSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = fileURLToPath(new URL("../../", import.meta.url));
const FANTASTIC = join(repoRoot, "python", ".venv", "bin", "fantastic");

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
  proc: ChildProcess;
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
  llm?: { bundle?: string; model?: string; endpoint?: string };
  /** Also create a `scheduler` agent (+ a `file` agent `sched_files` for its
   *  schedules) — fire a chosen payload to a target after N seconds. */
  scheduler?: boolean;
}

/** Absolute path to the built frontend (`ts/dist`) — served when `serveDist`. */
export const DIST_DIR = join(repoRoot, "ts", "dist");

function fantasticAvailable(): boolean {
  try {
    return statSync(FANTASTIC).isFile();
  } catch {
    return false;
  }
}

function runCli(tmp: string, args: string[]): string {
  const r = spawnSync(FANTASTIC, args, { cwd: tmp, encoding: "utf8" });
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
  const tmp = mkdtempSync(join(tmpdir(), "ftbridge-"));
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
    runCli(tmp, [
      webId,
      "create_agent",
      "handler_module=file.tools",
      "id=ts_dist",
      `root=${DIST_DIR}`,
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

  // inject the repo `.env` (e.g. ANTHROPIC_KEY) into the daemon's environment —
  // it runs in `tmp`, so the python `_load_dotenv` (cwd-relative) won't find it.
  const proc = spawn(FANTASTIC, [], {
    cwd: tmp,
    stdio: "ignore",
    env: { ...process.env, ...envFromDotenv() },
  });
  proc.unref();
  try {
    await waitForPort(port, 15000);
  } catch (e) {
    teardownHost({ proc, tmp } as Host);
    throw e;
  }
  return {
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
  };
}

export function teardownHost(host: Host): void {
  try {
    host.proc?.kill("SIGTERM");
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
    host.proc.kill("SIGTERM");
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
  const proc = spawn(FANTASTIC, [], {
    cwd: host.tmp,
    stdio: "ignore",
    env: { ...process.env, ...envFromDotenv() },
  });
  proc.unref();
  (host as { proc: ChildProcess }).proc = proc;
  await waitForPort(host.port, 15000);
}
