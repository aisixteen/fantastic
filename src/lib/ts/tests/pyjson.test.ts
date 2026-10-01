// pyjson: python's text forms. Every expected string below was printed by
// python 3.11 (`repr`, `json.dumps`, `int`) — python is canonical.

import { test } from "node:test";
import assert from "node:assert/strict";
import { pyDumps, pyInt, pyRepr } from "../src/kernel/pyjson.ts";

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

// python `int(s)` (base 10) — the grammar a `_binary_path` list step goes through.
test("pyInt reads a str as python int() does", () => {
  const ok: [string, bigint][] = [
    ["0", 0n],
    ["1_0", 10n],
    ["-0", 0n],
    [" -5 ", -5n],
    ["+7", 7n],
    ["007", 7n],
    ["0\u{a0}", 0n],
    ["\u{661}\u{662}", 12n],
    ["\u{1d7d9}", 1n],
    ["99999999999999999999", 99999999999999999999n],
  ];
  for (const [s, want] of ok) assert.equal(pyInt(s), want, pyRepr(s));
  const bad: [string, string][] = [
    ["_1", "'_1'"],
    ["1_", "'1_'"],
    ["1__0", "'1__0'"],
    ["+-1", "'+-1'"],
    ["", "''"],
    ["0x1", "'0x1'"],
    ["1e0", "'1e0'"],
    ["1.5", "'1.5'"],
    ["\x1c0", "'\\x1c0'"],
    ["\u{feff}0", "'\\ufeff0'"],
  ];
  for (const [s, quoted] of bad) {
    assert.throws(() => pyInt(s), { message: `invalid literal for int() with base 10: ${quoted}` });
  }
  // python cuts the quoted literal at 200 characters (`%.200R`)
  assert.throws(() => pyInt("x".repeat(300)), {
    message: `invalid literal for int() with base 10: '${"x".repeat(199)}`,
  });
});
