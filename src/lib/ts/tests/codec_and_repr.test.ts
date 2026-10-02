// The recursive walkers: the frame codec's binary path (find, clone, place), the
// secret masks' copy and scrub, and how reflect quotes a readme key. Every
// expectation whose text python also produces was measured on python
// (io_bridge/_codec.py, kernel/_secret.py, `_readme_view`) on 2026-10-01.

import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import type { Json, Payload } from "../src/kernel/json.ts";
import { encodeFrame, decodeFrame } from "../src/transport/frame.ts";
import type { Envelope } from "../src/transport/frame.ts";
import { check, shownPayload } from "../src/kernel/secret.ts";
import { pyDumps, pyRepr } from "../src/kernel/pyjson.ts";
import { str } from "../src/kernel/json.ts";

/** [head JSON text, body bytes] of a binary frame. */
function split(buf: ArrayBuffer): [string, number[]] {
  const n = new DataView(buf).getUint32(0, false);
  return [new TextDecoder().decode(new Uint8Array(buf, 4, n)), [...new Uint8Array(buf, 4 + n)]];
}

/** A binary frame as a peer would send it. */
function frame(head: unknown, body: number[] = [1, 2]): ArrayBuffer {
  const h = typeof head === "string" ? new TextEncoder().encode(head) : new TextEncoder().encode(JSON.stringify(head));
  const buf = new Uint8Array(4 + h.length + body.length);
  new DataView(buf.buffer).setUint32(0, h.length, false);
  buf.set(h, 4);
  buf.set(body, 4 + h.length);
  return buf.buffer;
}

// ── the frame codec ────────────────────────────────────────────────────

test("bytes inside a list: python's head and path, the body, and a round trip with no hole", () => {
  const env = { type: "x", payload: { chunks: ["a", new Uint8Array([9, 8])] } };
  const enc = encodeFrame(env);
  assert.equal(enc.binary, true);
  assert.deepEqual(split(enc.data as ArrayBuffer), [
    '{"type": "x", "payload": {"chunks": ["a", null]}, "_binary_path": ["payload", "chunks", 1]}',
    [9, 8],
  ]);
  const dec = decodeFrame(enc.data) as { payload: { chunks: unknown[] } };
  assert.equal(dec.payload.chunks.length, 2);
  assert.equal(dec.payload.chunks[0], "a");
  assert.deepEqual([...(dec.payload.chunks[1] as Uint8Array)], [9, 8]);
  // encoding never touches the caller's envelope
  assert.ok(env.payload.chunks[1] instanceof Uint8Array);
  assert.deepEqual(Object.keys(dec), ["type", "payload"], "the path marker is consumed");
});

test("the body is exactly the value's bytes: a bare ArrayBuffer, and a view with an offset", () => {
  const ab = new Uint8Array([5, 6, 7]).buffer;
  assert.deepEqual(split(encodeFrame({ type: "x", b: ab }).data as ArrayBuffer), ['{"type": "x", "b": null, "_binary_path": ["b"]}', [5, 6, 7]]);
  const view = new Uint8Array([0, 1, 2, 3]).subarray(1, 3);
  assert.deepEqual(split(encodeFrame({ type: "x", deep: [{ v: view }] }).data as ArrayBuffer), [
    '{"type": "x", "deep": [{"v": null}], "_binary_path": ["deep", 0, "v"]}',
    [1, 2],
  ]);
});

test("decoding a binary frame: a missing key on the path, a non-object head, the empty key, a bad header", () => {
  assert.throws(() => decodeFrame(frame({ a: {}, _binary_path: ["a", "b", "c"] })), { message: "bad _binary_path ['a', 'b', 'c']: no key 'b'" });
  assert.throws(() => decodeFrame(frame({ a: { b: 1 }, _binary_path: ["x", "b"] })), { message: "bad _binary_path ['x', 'b']: no key 'x'" });
  // a head that is not an object is returned as it is (python: no `_binary_path` to pop)
  assert.deepEqual(decodeFrame(frame("[1, 2]")), [1, 2] as unknown as Envelope);
  assert.equal(decodeFrame(frame("5")), 5 as unknown as Envelope);
  // the step "" is the key "" (python `obj[""] = body`)
  const e = decodeFrame(frame({ a: {}, _binary_path: [""] })) as Record<string, unknown>;
  assert.deepEqual(Object.keys(e), ["a", ""]);
  assert.deepEqual([...(e[""] as Uint8Array)], [1, 2]);
  // a header that is not strict JSON, or not UTF-8, is one Error with python's prefix
  for (const head of ['{"a": NaN}', "{bad"]) {
    assert.throws(() => decodeFrame(frame(head)), (err: unknown) => {
      assert.ok(err instanceof Error && err.message.startsWith("bad binary frame header: "), String(err));
      return true;
    });
  }
  assert.throws(() => decodeFrame(frame('{"a": 12345678901234567890}')), (err: unknown) => {
    assert.ok((err as Error).message.startsWith("bad binary frame header: integer 12345678901234567890 is outside int64"), String(err));
    return true;
  });
  const notUtf8 = new Uint8Array([0, 0, 0, 2, 0xff, 0xfe]).buffer;
  assert.throws(() => decodeFrame(notUtf8), (err: unknown) => {
    assert.ok((err as Error).message.startsWith("bad binary frame header: "), String(err));
    return true;
  });
});

// ── secret: the copy and the scrub ─────────────────────────────────────

test("secret masks: a dict's leaves are its values, bytes are shared and never scrubbed, a dead-end path names nothing", () => {
  const blob = new TextEncoder().encode("k-uu-k");
  const P = {
    type: "login",
    creds: { user: "uu", pass: "pp", n: 5, ok: true, none: null, f: 1.5 },
    s: "str",
    list: [1, "two"],
    blob,
  } as unknown as Payload;
  const [s, r] = check({ secret: ["payload.creds"] }, P);
  assert.equal(r, null);
  assert.deepEqual(s!.values, ["uu", "pp", "5", "1.5"]);
  const shown = shownPayload(P, s!);
  assert.equal(shown["creds"], "<secret>");
  assert.deepEqual(shown["list"], [1, "two"]);
  assert.equal(shown["blob"] as unknown, blob, "bytes are shared, as python's deepcopy shares them");
  assert.equal(new TextDecoder().decode(blob), "k-uu-k", "a secret value inside bytes is not scrubbed");
  assert.deepEqual((P["creds"] as Payload)["user"], "uu", "the real payload is untouched");
  assert.deepEqual(check({ secret: ["payload.list.1"] }, P)[0]!.values, ["two"]);
  assert.deepEqual(check({ secret: ["payload.blob"] }, P)[0]!.values, ["b'k-uu-k'"]);
  for (const p of ["payload.s.x", "payload.list.5", "payload.list.x"]) {
    assert.deepEqual(check({ secret: [p] }, P), [
      null,
      { error: `send: secret path '${p}' names nothing in the payload`, reason: "bad_arg", arg: "secret" },
    ]);
  }
});

// ── reflect: readme keys and the bundle catalog ────────────────────────

test("reflect quotes a readme key as python's repr does (plain, a quote, a backslash) and refuses an escaping one", async () => {
  const k = new Kernel();
  k.setRoot(new Agent({ id: "kernel_state" }));
  const readme = async (sel: Json): Promise<Json | undefined> => {
    const r = (await k.send([{ target: "kernel_state", payload: { type: "reflect", tree: "none", readme: sel } }]))[0]!.result as Payload;
    return "readme" in r ? r["readme"] : undefined;
  };
  assert.deepEqual(await readme("index"), { index: [] });
  assert.deepEqual(await readme("x"), { error: "no doc 'x'", keys: [] });
  assert.deepEqual(await readme("a'b"), { error: "no doc \"a'b\"", keys: [] });
  assert.deepEqual(await readme("a\\b"), { error: "no doc 'a\\\\b'", keys: [] });
  assert.deepEqual(await readme('it"s'), { error: "no doc 'it\"s'", keys: [] });
  assert.deepEqual(await readme("a/../b"), { error: "bad key 'a/../b'", keys: [] });
  assert.deepEqual(await readme("/"), { error: "bad key '/'", keys: [] });
  assert.equal(await readme(""), undefined, "an empty key asks for nothing");
});

test("reflect bundles=all pages the catalog by name, sorted, with bundles_next", async () => {
  const k = new Kernel();
  k.registerBundle("zeta.ts", () => null);
  k.registerBundle("alpha.tools", () => null);
  k.registerBundle("mid.js", () => null);
  k.setRoot(new Agent({ id: "kernel_state" }));
  const page = async (extra: Payload): Promise<Payload> =>
    (await k.send([{ target: "kernel", payload: { type: "reflect", tree: "none", bundles: "all", ...extra } }]))[0]!.result as Payload;
  const first = await page({ limit: 2 });
  assert.deepEqual(first["bundles"], [
    { name: "alpha", handler_module: "alpha.tools" },
    { name: "mid", handler_module: "mid.js" },
  ]);
  assert.equal(first["bundles_next"], "mid");
  const rest = await page({ limit: 2, after: "mid" });
  assert.deepEqual(rest["bundles"], [{ name: "zeta", handler_module: "zeta.ts" }]);
  assert.equal("bundles_next" in rest, false);
  const all = await page({});
  assert.equal((all["bundles"] as Json[]).length, 3);
  assert.equal("bundles_next" in all, false);
});

// ── pyjson: the two walkers over one nested value ──────────────────────

test("pyRepr and pyDumps walk the same nested value as python's repr and json.dumps do", () => {
  const x = {
    a: [null, true, false, 1, -2, 1.5, 1e-5, "it's", 'q"', "\n\x00é \t\\", "\u{1f600}"],
    "k'ey": { n: {} },
    e: [],
    m: [[1, [2]], { z: null }],
  };
  assert.equal(
    pyRepr(x),
    "{'a': [None, True, False, 1, -2, 1.5, 1e-05, \"it's\", 'q\"', '\\n\\x00é\\u2028\\t\\\\', '\u{1f600}'], \"k'ey\": {'n': {}}, 'e': [], 'm': [[1, [2]], {'z': None}]}",
  );
  assert.equal(
    pyDumps(x),
    '{"a": [null, true, false, 1, -2, 1.5, 1e-05, "it\'s", "q\\"", "\\n\\u0000\\u00e9\\u2028\\t\\\\", "\\ud83d\\ude00"], "k\'ey": {"n": {}}, "e": [], "m": [[1, [2]], {"z": null}]}',
  );
  assert.equal(
    pyDumps(x, { ensureAscii: false }),
    '{"a": [null, true, false, 1, -2, 1.5, 1e-05, "it\'s", "q\\"", "\\n\\u0000é \\t\\\\", "\u{1f600}"], "k\'ey": {"n": {}}, "e": [], "m": [[1, [2]], {"z": null}]}',
  );
  assert.equal(pyRepr(["a"], { tuple: true }), "('a',)");
  assert.equal(pyRepr([], { tuple: true }), "()");
  assert.equal(pyRepr(["a", "b"], { tuple: true }), "('a', 'b')");
  // a key whose value is undefined is absent (python has no such value); in a list it is None
  assert.equal(pyRepr({ u: undefined, l: [undefined] }), "{'l': [None]}");
  assert.equal(pyDumps({ u: undefined, l: [undefined] }), '{"l": [null]}');
});

test("str reads a string field, or the fallback for anything else", () => {
  assert.equal(str({ type: "go" }, "type"), "go");
  assert.equal(str({ type: 5 }, "type"), "");
  assert.equal(str({}, "type", "none"), "none");
  assert.equal(str({ type: null }, "type", "none"), "none");
  assert.equal(str({ type: "" }, "type", "none"), "", "an empty string is a string");
});
