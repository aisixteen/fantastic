import { test } from "node:test";
import assert from "node:assert/strict";
import { plan } from "../src/bundles/home_view/pages.ts";
import type { HomeRow, Status } from "../src/bundles/home_view/home_model.ts";

function row(id: string, status: Status, port = 0): HomeRow {
  return {
    id, guid: id, name: id, path: `/f/${id}`, last_seen: 1, status,
    canvas: status === "running" ? { url: `http://127.0.0.1:${port}/`, port } : null,
    pid: status === "stopped" ? null : 10, actions: [], log: null,
  };
}

test("plan keeps a running page at the same url, creates a new one, drops a stopped, closed or moved one", () => {
  const held = new Map([
    ["a", "http://127.0.0.1:1001/"], // still running there: kept
    ["b", "http://127.0.0.1:1002/"], // stopped: dropped
    ["c", "http://127.0.0.1:1003/"], // its page closed: dropped
    ["d", "http://127.0.0.1:1004/"], // moved to another port: dropped and made again
  ]);
  const p = plan(held, {
    pages: ["a", "b", "d", "e", "f"],
    rows: [row("a", "running", 1001), row("b", "stopped"), row("c", "running", 1003), row("d", "running", 1999), row("e", "running", 1005), row("f", "starting")],
  });
  assert.deepEqual(p.keep, ["a"]);
  assert.deepEqual(p.drop, ["b", "c", "d"]);
  assert.deepEqual(p.create, [
    ["d", "http://127.0.0.1:1999/"],
    ["e", "http://127.0.0.1:1005/"],
  ]);
  // nothing held, nothing open: nothing to do; a page with no row makes no frame
  assert.deepEqual(plan(new Map(), { pages: [], rows: [row("a", "running", 1)] }), { keep: [], create: [], drop: [] });
  assert.deepEqual(plan(new Map(), { pages: ["gone"], rows: [] }), { keep: [], create: [], drop: [] });
});
