// Part-3 bridge: a frontend view bundle's readme (its "client of a host
// capability" self-description) is surfaced via reflect readme=true, so an
// LLM can read it over the bridge and weave the frontend↔host pairing.

import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";

test("reflect readme=true surfaces a view bundle's registered readme", async () => {
  const kernel = new Kernel();
  kernel.registerBundle("ai_view.ts", () => null);
  kernel.setBundleReadme(
    "ai_view.ts",
    "HTML chat CLIENT for a host LLM backend. Fronts any agent answering send/history/interrupt; bound by backend_id.",
  );
  kernel.setRoot(new Agent({ id: "canvas", handlerModule: "canvas.ts" }));
  kernel.register(new Agent({ id: "chat", handlerModule: "ai_view.ts", parentId: "canvas" }));

  const reply = (await kernel.send("chat", { type: "reflect", readme: true })) as Record<
    string,
    unknown
  >;
  assert.equal(typeof reply["readme"], "string", "bundle readme surfaced");
  assert.match(reply["readme"] as string, /client/i, "declares its client role");
  assert.match(reply["readme"] as string, /backend_id/, "names the binding");
});

test("per-record meta.readme overrides the bundle readme", async () => {
  const kernel = new Kernel();
  kernel.registerBundle("ai_view.ts", () => null);
  kernel.setBundleReadme("ai_view.ts", "BUNDLE-LEVEL");
  kernel.setRoot(new Agent({ id: "canvas", handlerModule: "canvas.ts" }));
  kernel.register(
    new Agent({
      id: "chat",
      handlerModule: "ai_view.ts",
      parentId: "canvas",
      meta: { readme: "PER-RECORD" },
    }),
  );

  const reply = (await kernel.send("chat", { type: "reflect", readme: true })) as Record<
    string,
    unknown
  >;
  assert.equal(reply["readme"], "PER-RECORD", "per-record meta wins");
});

test("an agent with no readme reflects readme:null", async () => {
  const kernel = new Kernel();
  kernel.registerBundle("ai_view.ts", () => null);
  kernel.setRoot(new Agent({ id: "canvas", handlerModule: "canvas.ts" }));
  kernel.register(new Agent({ id: "chat", handlerModule: "ai_view.ts", parentId: "canvas" }));

  const reply = (await kernel.send("chat", { type: "reflect", readme: true })) as Record<
    string,
    unknown
  >;
  assert.equal(reply["readme"], null, "no bundle readme -> null");
});
