import { test } from "node:test";
import assert from "node:assert/strict";
import { WatchHub, readAll, readBytes } from "../src/bundles/files/file_bridge.ts";
import type { Change } from "../src/bundles/files/file_bridge.ts";
import { FakeBridge, settle } from "./fake_bridge.ts";

test("readAll pages a long file, keeps a trailing newline and the last line's absence of one", async () => {
  const b = new FakeBridge();
  const long = Array.from({ length: 4500 }, (_, i) => `line ${i}`).join("\n");
  b.put("long.txt", long + "\n");
  b.put("nonl.txt", "a\nb");
  b.put("empty.txt", "");
  b.put("blank.txt", "\n");
  b.put("crlf.txt", "a\r\nb\r\n");
  const r = await readAll(b, "long.txt");
  assert.ok(r.ok && r.text === long + "\n");
  assert.ok(b.calls.filter((c) => c["type"] === "read").length >= 3, "paged");
  for (const [name, text] of [["nonl.txt", "a\nb"], ["empty.txt", ""], ["blank.txt", "\n"], ["crlf.txt", "a\r\nb\r\n"]] as const) {
    const x = await readAll(b, name);
    assert.ok(x.ok && x.text === text, name);
  }
});

test("readAll: a refusal is returned as it is; a file that changes under the read is read again, then said", async () => {
  const b = new FakeBridge();
  const nf = await readAll(b, "nope.txt");
  assert.ok(!nf.ok && nf.refusal["reason"] === "not_found");

  b.put("f.txt", Array.from({ length: 4001 }, (_, i) => `l${i}`).join("\n"));
  let reads = 0;
  const was = b.call.bind(b);
  b.call = async (p) => {
    const r = await was(p);
    if (p["type"] === "read" && ++reads % 2 === 1) b.files.set("f.txt", `${b.files.get("f.txt")}\nmore`); // changes after every first page
    return r;
  };
  const x = await readAll(b, "f.txt");
  assert.ok(!x.ok);
  assert.equal(x.refusal["reason"], "changing");
  assert.match(x.refusal.error, /kept changing/);
});

test("readBytes pages read_stream into one array, and refuses a file over the cap", async () => {
  const b = new FakeBridge();
  b.put("a.bin", "x".repeat(2_500_000));
  const r = await readBytes(b, "a.bin", 10_000_000);
  assert.ok(r.ok && r.bytes.length === 2_500_000);
  const big = await readBytes(b, "a.bin", 1000);
  assert.ok(!big.ok && big.refusal["reason"] === "too_large");
});

test("the hub: one watch for two holders, events reach the holders of the path or its folder, coalesced per tick", async () => {
  const b = new FakeBridge();
  b.put("src/one.txt", "1");
  const hub = new WatchHub(b, b.subscribe);
  const dir: Change[] = [];
  const file: Change[] = [];
  const other: Change[] = [];
  await hub.hold("src", "tree", (c) => dir.push(c));
  await hub.hold("src/one.txt", "prev", (c) => file.push(c));
  await hub.hold("src", "prev", (c) => other.push(c));
  assert.equal(b.calls.filter((c) => c["type"] === "watch_path" && c["path"] === "src").length, 1, "the second holder of src did not watch again");
  b.put("src/one.txt", "2");
  b.put("src/one.txt", "3");
  b.put("src/two.txt", "x");
  await settle();
  assert.deepEqual(file.map((c) => c.path), ["src/one.txt"], "two writes in a tick are one change");
  assert.deepEqual(dir.map((c) => c.path).sort(), ["src/one.txt", "src/two.txt"]);
  assert.equal(other.length, 2);
  hub.drop("src", "tree");
  assert.ok(b.watched.has("src"), "prev still holds src");
  hub.drop("src", "prev");
  hub.dropAll("prev");
  await settle();
  assert.deepEqual([...b.watched], []);
  assert.equal(b.subscribed, false);
});

test("the hub: a refused watch is thrown to every holder waiting on it, and holds nothing", async () => {
  const b = new FakeBridge();
  const hub = new WatchHub(b, b.subscribe);
  const was = b.call.bind(b);
  b.call = async (p) => (p["type"] === "watch_path" ? { error: "watch_path: denied", reason: "denied" } : was(p));
  const a = hub.hold("src", "a", () => {});
  const c = hub.hold("src", "b", () => {});
  await assert.rejects(a, /denied/);
  await assert.rejects(c, /denied/);
  assert.deepEqual(hub.held(), []);
  assert.equal(b.calls.filter((x) => x["type"] === "unwatch_path").length, 0, "nothing was armed, so nothing is unwatched");
});
