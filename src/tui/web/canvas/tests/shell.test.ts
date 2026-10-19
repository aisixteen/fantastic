import { test } from "node:test";
import assert from "node:assert/strict";
import { readShell } from "../src/bundles/shell.ts";

// THE NATIVE SHELL'S FLAG (`globalThis.__FANTASTIC_SHELL__`): phases 3, 4, 6 and 7 read it.

const g = globalThis as { __FANTASTIC_SHELL__?: unknown };
const VALID = { name: "tauri", os: "macos", webview: "wkwebview", backdrop: "native" } as const;

function withFlag<T>(flag: unknown, fn: () => T): T {
  g.__FANTASTIC_SHELL__ = flag;
  try {
    return fn();
  } finally {
    delete g.__FANTASTIC_SHELL__;
  }
}

test("readShell is null without a flag, reads a valid one, and names the bad field", async () => {
  delete g.__FANTASTIC_SHELL__;
  assert.equal(readShell(), null);

  const flag = Object.freeze({ ...VALID });
  assert.equal(withFlag(flag, readShell), flag);
  const pick = async (): Promise<string[]> => ["/a"];
  const withPick = Object.freeze({ ...VALID, os: "windows", webview: "webview2", backdrop: "web", pickFolder: pick });
  const read = withFlag(withPick, readShell);
  assert.equal(read?.pickFolder, pick);
  assert.deepEqual(await read!.pickFolder!(), ["/a"]);

  const bad: Array<[unknown, string]> = [
    [{ ...VALID, name: "" }, "name"],
    [{ ...VALID, name: 7 }, "name"],
    [{ ...VALID, os: "beos" }, "os"],
    [{ ...VALID, webview: "gecko" }, "webview"],
    [{ ...VALID, backdrop: "glass" }, "backdrop"],
    [{ ...VALID, pickFolder: "yes" }, "pickFolder"],
    ["tauri", "the flag"],
    [[VALID], "the flag"],
  ];
  for (const [flag, field] of bad) {
    assert.throws(
      () => withFlag(flag, readShell),
      (e: unknown) => e instanceof Error && e.message.startsWith(`__FANTASTIC_SHELL__: ${field} must be `),
      `${JSON.stringify(flag)} names ${field}`,
    );
  }
});
