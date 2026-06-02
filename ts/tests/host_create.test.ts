// Part-3: the canvas creates host peers on the HOST ROOT without naming a
// host-specific id. `kernel.callHost("kernel", …)` must forward the LITERAL
// target over the bridge (the host resolves `kernel` to its own root —
// fs_loader/core), NOT resolve locally to the frontend canvas root.

import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel, type Bridge } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import type { Json, Payload } from "../src/kernel/json.ts";

function fakeBridge(seen: Array<{ target: string; payload: Payload }>): Bridge {
  return {
    forward: (target: string, payload: Payload): Promise<Json> => {
      seen.push({ target, payload });
      return Promise.resolve({ id: "tb_1" });
    },
    forwardBinary: () => Promise.resolve({}),
    emitRemote: () => {},
    watchRemote: () => {},
    unwatchRemote: () => {},
    onLifecycle: () => () => {},
    subscribeState: () => () => {},
  };
}

test("callHost forwards the LITERAL target to the host, not the local root", async () => {
  const kernel = new Kernel();
  kernel.setRoot(new Agent({ id: "canvas", handlerModule: "canvas.ts" }));
  const seen: Array<{ target: string; payload: Payload }> = [];
  kernel.bridge = fakeBridge(seen);

  const reply = await kernel.callHost("kernel", {
    type: "create_agent",
    handler_module: "terminal_backend.tools",
  });
  assert.equal(seen.length, 1);
  assert.equal(
    seen[0]?.target,
    "kernel",
    "host create forwards the literal 'kernel' (host resolves to its own root), not the local canvas",
  );
  assert.deepEqual(reply, { id: "tb_1" });
});

test("a plain send('kernel') still resolves LOCALLY to the canvas root", async () => {
  const kernel = new Kernel();
  kernel.setRoot(new Agent({ id: "canvas", handlerModule: "canvas.ts" }));
  kernel.bridge = fakeBridge([]);

  const local = (await kernel.send("kernel", { type: "reflect" })) as Record<string, unknown>;
  assert.equal(local["id"], "canvas", "send('kernel') resolves to the local frontend root");
});
