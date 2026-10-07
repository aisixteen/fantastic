// FOLDER BUNDLES on the page kernel (owner, 2026-10-08): `<loader root>/bundles/*/
// bundle.yaml`, read through the host loader's `read_bundles` (raw text) and parsed
// HERE, extend a BASE the app registered (`registerBundle(…, {folderBundle})`).
// They load before the tree; the root's `reload_bundle` re-reads one and restarts
// its live instances. Python twin: py_agent/tests/test_py_agent.py.

import { test } from "node:test";
import assert from "node:assert/strict";
import { Agent, boot } from "../src/index.ts";
import type { FolderBase, Json, Kernel, Payload, WebSocketLike } from "../src/index.ts";
import { decodeFrame, encodeFrame } from "../src/transport/frame.ts";

/** A host loader on the control socket: `load_tree` + `read_bundles` over a
 *  mutable folder table (a test edits a folder, then reloads). */
class HostSocket implements WebSocketLike {
  binaryType = "blob";
  onopen: ((ev: unknown) => void) | null = null;
  onclose: ((ev: unknown) => void) | null = null;
  onerror: ((ev: unknown) => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  readonly url: string;
  private readonly records: Payload[];
  private readonly folders: Map<string, string | null>;
  constructor(url: string, records: Payload[], folders: Map<string, string | null>) {
    this.url = url;
    this.records = records;
    this.folders = folders;
    setTimeout(() => this.onopen?.(undefined), 0);
  }
  send(data: string | ArrayBuffer): void {
    const env = decodeFrame(data);
    if (env["type"] !== "call") return;
    const p = env["payload"] as Payload;
    let reply: Json = { ok: true };
    if (p["type"] === "load_tree") reply = { version: 1, records: this.records as Json };
    if (p["type"] === "read_bundles") {
      const row = (n: string): Payload => {
        const text = this.folders.get(n);
        const out: Payload = { name: n, folder: `web/bundles/${n}` };
        if (typeof text === "string") out["text"] = text;
        else out["error"] = `bundles/${n}: no bundle.yaml`;
        return out;
      };
      const name = p["name"];
      if (typeof name === "string") {
        reply = this.folders.has(name)
          ? { bundles: [row(name)] }
          : {
              error: `read_bundles: no folder web/bundles/${name}`,
              reason: "not_found",
              hint: "a bundle is a folder under web/bundles/ with a bundle.yaml",
            };
      } else {
        reply = { bundles: [...this.folders.keys()].sort().map(row) };
      }
    }
    setTimeout(() => this.onmessage?.({ data: encodeFrame({ type: "reply", id: env["id"], data: reply }).data }), 0);
  }
  close(): void {}
}

/** The BASE a folder extends — the canvas's html agent, in miniature: each
 *  instance renders the folder's `html`, stamped with the load it runs. */
let loads = 0;
const log: string[] = [];
const htmlBase: FolderBase = (name, folder, spec) => {
  const html = spec["html"];
  if (typeof html !== "string") return "html: required — the page this agent renders";
  const load = ++loads;
  return {
    handler: (id, p) => {
      log.push(`${id}:${String(p["type"])}${p["reason"] !== undefined ? `:${String(p["reason"])}` : ""}@${load}`);
      if (p["type"] === "render") return { html, folder, load, bundle: name };
      return null;
    },
    verbs: spec["verbs"] as { [verb: string]: string },
    readme: `${name}: an html agent`,
  };
};

const CLOCK = "base: html_agent\nsentence: A clock.\nhtml: <b>tick</b>\nverbs:\n  render: 'No args. Returns {html}.'\n";

async function page(
  folders: { [name: string]: string | null },
  records: Payload[] = [],
): Promise<{ kernel: Kernel; table: Map<string, string | null> }> {
  const table = new Map(Object.entries(folders));
  const { kernel } = await boot({
    origin: "ws://host",
    loader: "web_loader",
    makeSocket: (url) => new HostSocket(url, records, table),
    register: (k) => {
      k.registerBundle("html_agent.ts", () => null, {}, { folderBundle: htmlBase });
      k.registerBundle("app_root.ts", () => null);
    },
    seed: (k) => k.setRoot(new Agent({ id: "app", handlerModule: "app_root.ts" })),
  });
  return { kernel, table };
}

async function call(k: Kernel, target: string, payload: Payload): Promise<Payload> {
  return (await k.send([{ target, payload }]))[0]!.result as Payload;
}

test("a folder extending a base loads BEFORE the tree: its persisted instance runs, docs read bundle.yaml", async () => {
  const { kernel } = await page({ clock: CLOCK }, [
    { id: "web_loader", handler_module: "kernel_state.tools" },
    { id: "app", handler_module: "app_root.ts", parent_id: "web_loader" },
    { id: "c1", handler_module: "clock", parent_id: "app" },
  ]);
  const r = await call(kernel, "c1", { type: "render" });
  assert.deepEqual(r, { html: "<b>tick</b>", folder: "web/bundles/clock", load: loads, bundle: "clock" });
  const cat = (await call(kernel, "kernel", { type: "reflect", tree: "none", bundles: "all" }))["bundles"] as Payload[];
  assert.deepEqual(
    cat.find((b) => b["name"] === "clock"),
    { name: "clock", handler_module: "clock", folder: "web/bundles/clock", base: "html_agent", sentence: "A clock." },
  );
  assert.equal(cat.filter((b) => b["handler_module"] === "clock").length, 1, "listed once, as its folder row");
  const docs = await call(kernel, "kernel", { type: "docs", of: "clock", tier: "detailed" });
  assert.deepEqual(docs["verbs"], { render: "No args. Returns {html}." });
  const rf = await call(kernel, "c1", { type: "reflect", readme: true });
  assert.equal(rf["readme"], "clock: an html agent");
});

for (const [name, text, fragment] of [
  ["nobase", "sentence: x\n", "base: required — the bundled agent it extends (here: html_agent)"],
  ["badbase", "base: py_agent\nsentence: x\n", "base: 'py_agent' is no bundled agent a folder can extend in this kernel — known: html_agent"],
  ["nosentence", "base: html_agent\n", "sentence: required"],
  ["nohtml", "base: html_agent\nsentence: x\n", "html: required"],
  ["notamap", "- a\n- b\n", "bundle.yaml is not a mapping"],
  ["broken", "base: 'html_agent\n", "bundle.yaml does not parse: web/bundles/broken/bundle.yaml:1:"],
  ["noyaml", null, "no bundle.yaml"],
  ["app_root", CLOCK, "'app_root' is a built-in bundle — a folder may not shadow it"],
  ["bad.name", CLOCK, "a bundle folder is named with letters, digits, _ and -"],
] as const) {
  test(`a folder that cannot load is listed with its error, never run: ${name}`, async () => {
    const { kernel } = await page({ [name]: text }, [
      { id: "web_loader" },
      { id: "app", handler_module: "app_root.ts", parent_id: "web_loader" },
      { id: "x1", handler_module: name, parent_id: "app" },
    ]);
    const cat = (await call(kernel, "kernel", { type: "reflect", tree: "none", bundles: "all" }))["bundles"] as Payload[];
    const row = cat.find((b) => b["name"] === name && "folder" in b);
    assert.ok(row !== undefined && String(row["error"]).includes(fragment), JSON.stringify(row));
    assert.ok(String(row["error"]).startsWith(`bundles/${name}: `), JSON.stringify(row));
    // a record naming it weak-loads (skipped), as any bundle this runtime cannot run
    assert.equal(kernel.get("x1"), undefined);
    if (name !== "app_root") {
      const d = await call(kernel, "kernel", { type: "docs", of: name });
      assert.equal(d["reason"], "bad_arg", JSON.stringify(d));
    }
  });
}

test("reload_bundle restarts live instances under the fresh load — shutdown under the old, boot under the new", async () => {
  const { kernel, table } = await page({ clock: CLOCK });
  kernel.register(new Agent({ id: "c2", handlerModule: "clock", parentId: "app" }));
  kernel.register(new Agent({ id: "c1", handlerModule: "clock", parentId: "app" }));
  const before = (await call(kernel, "c1", { type: "render" }))["load"] as number;
  table.set("clock", CLOCK.replace("<b>tick</b>", "<b>tock</b>"));
  log.length = 0;
  const r = await call(kernel, "kernel", { type: "reload_bundle", name: "clock" });
  assert.deepEqual(r, { reloaded: "clock", instances: ["c1", "c2"] });
  const after = loads;
  assert.deepEqual(log, [
    `c1:shutdown:reloaded@${before}`,
    `c1:boot@${after}`,
    `c2:shutdown:reloaded@${before}`,
    `c2:boot@${after}`,
  ]);
  assert.deepEqual(await call(kernel, "c2", { type: "render" }), {
    html: "<b>tock</b>",
    folder: "web/bundles/clock",
    load: after,
    bundle: "clock",
  });
});

test("reload_bundle loads a NEW folder; a failed reload keeps the previous load running", async () => {
  const { kernel, table } = await page({});
  table.set("fresh", CLOCK);
  assert.deepEqual(await call(kernel, "kernel", { type: "reload_bundle", name: "fresh" }), {
    reloaded: "fresh",
    instances: [],
  });
  kernel.register(new Agent({ id: "f1", handlerModule: "fresh", parentId: "app" }));
  const first = await call(kernel, "f1", { type: "render" });
  table.set("fresh", "base: html_agent\n"); // sentence gone
  const r = await call(kernel, "kernel", { type: "reload_bundle", name: "fresh" });
  assert.equal(r["reason"], "invalid_meta");
  assert.ok(String(r["error"]).includes("sentence: required"), JSON.stringify(r));
  assert.deepEqual(await call(kernel, "f1", { type: "render" }), first, "the live instance runs the previous load");
  const cat = (await call(kernel, "kernel", { type: "reflect", tree: "none", bundles: "all" }))["bundles"] as Payload[];
  const row = cat.find((b) => b["name"] === "fresh");
  assert.equal(row?.["base"], "html_agent");
  assert.equal(row?.["error"], undefined);
});

test("reload_bundle refuses: no name, no folder, a bad name, a non-root target", async () => {
  const { kernel } = await page({ clock: CLOCK });
  kernel.register(new Agent({ id: "c1", handlerModule: "clock", parentId: "app" }));
  assert.equal((await call(kernel, "kernel", { type: "reload_bundle" }))["reason"], "missing_arg");
  const gone = await call(kernel, "kernel", { type: "reload_bundle", name: "nope" });
  assert.equal(gone["reason"], "not_found");
  assert.equal(gone["error"], "reload_bundle: read_bundles: no folder web/bundles/nope");
  assert.equal((await call(kernel, "kernel", { type: "reload_bundle", name: "../x" }))["reason"], "not_found");
  assert.equal((await call(kernel, "c1", { type: "reload_bundle", name: "clock" }))["reason"], "not_here");
});

test("a kernel with no loader wired reads no folders: reload_bundle says so", async () => {
  const { Kernel: K } = await import("../src/index.ts");
  const k = new K();
  k.setRoot(new Agent({ id: "app" }));
  const r = (await k.send([{ target: "kernel", payload: { type: "reload_bundle", name: "clock" } }]))[0]!.result as Payload;
  assert.equal(r["reason"], "not_connected", JSON.stringify(r));
});
