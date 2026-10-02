// pyjson: python's text forms. Every expected string below was printed by
// python 3.11 (`repr`, `json.dumps`, `int`) — python is canonical.

import { test } from "node:test";
import assert from "node:assert/strict";
import { pyDumps, pyRepr } from "../src/kernel/pyjson.ts";

// python `repr(float)` / `json.dumps(float)`: shortest round-trip digits, the
// exponent form exactly when the decimal exponent is < -4 or >= 16.
test("a float reads as python's repr: exponent below -4, two exponent digits", () => {
  const cases: [number, string][] = [
    [0.00001, "1e-05"],
    [1.5e-5, "1.5e-05"],
    [-0.00001, "-1e-05"],
    [0.0001, "0.0001"],
    [0.00012345, "0.00012345"],
    [1e-7, "1e-07"],
    [1.234e-300, "1.234e-300"],
    [5e-324, "5e-324"],
    [0.1, "0.1"],
    [123.456, "123.456"],
    [1e15 + 0.5, "1000000000000000.5"],
  ];
  for (const [n, want] of cases) {
    assert.equal(pyRepr(n), want, `repr(${n})`);
    assert.equal(pyDumps(n), want, `json.dumps(${n})`);
  }
  assert.equal(pyDumps({ n: [0.00001, 2.5] }), '{"n": [1e-05, 2.5]}');
  assert.equal(pyRepr([0.00001]), "[1e-05]");
});

// python `json.dumps(ensure_ascii=True)` escapes `[^\ -~]`: DEL (U+007F) too.
test("ensure_ascii escapes DEL as python does; ensure_ascii off leaves it", () => {
  assert.equal(pyDumps("\x7f\x80\x1f\b"), '"\\u007f\\u0080\\u001f\\b"');
  assert.equal(pyDumps({ "\x7f": 1 }), '{"\\u007f": 1}');
  assert.equal(pyDumps("\x7f", { ensureAscii: false }), '"\x7f"');
  assert.equal(pyDumps("~"), '"~"');
});
