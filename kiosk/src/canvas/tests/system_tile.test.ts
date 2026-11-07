import { test } from "node:test";
import assert from "node:assert/strict";
import { systemTileSize } from "../system_tile.ts";

test("system_tile_size_is_640x480_for_network_and_380x420_otherwise", () => {
  assert.deepEqual(systemTileSize("network"), { width: 640, height: 480 });
  for (const id of ["power", "audio", "displays", "notifications", "lock", "anything"]) {
    assert.deepEqual(systemTileSize(id), { width: 380, height: 420 }, id);
  }
});
