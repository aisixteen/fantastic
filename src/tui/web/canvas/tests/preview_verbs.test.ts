import { test } from "node:test";
import assert from "node:assert/strict";
import { Agent } from "../src/lib.ts";
import { controllers } from "../src/bundles/files/files_controller.ts";
import { previews } from "../src/bundles/preview/preview_controller.ts";
import { settle, versionOf } from "./fake_bridge.ts";
import type { Dict } from "./rig.ts";
import { project, rig } from "./rig.ts";

// THE VERBS of the previewer and the editor, driven through a Kernel as an AI
// drives them, over an in-memory file bridge.

test("a pinned preview shows the file, holds it live, and follows a change on disk", async () => {
  const r = rig();
  project(r.bridge);
  const p = await r.mountPreview("prev_pin", { pinned: "src/one.txt", follow: false });
  assert.equal(p.mode, "text");
  assert.equal(p.editor.text(), "one");
  assert.ok(r.bridge.watched.has("src/one.txt"));
  r.bridge.put("src/one.txt", "two");
  await settle();
  assert.equal(p.editor.text(), "two");
  r.bridge.remove("src/one.txt");
  await settle();
  assert.equal(p.content.k, "deleted");
  r.bridge.put("src/one.txt", "back");
  await settle();
  assert.equal(p.editor.text(), "back", "the hold was kept, so it shows again");
  assert.equal(p.content.k, "text");
});

test("follow takes the selection; pin freezes it; unpin follows again; open pins", async () => {
  const r = rig();
  project(r.bridge);
  const f = await r.mountPreview("prev_follow", {});
  assert.equal(f.content.k, "none");
  await r.select({ path: "docs/a.md", agent: null });
  assert.equal(f.mode, "markdown");
  assert.equal(f.editor.text(), "# Hello\n");
  assert.deepEqual(await r.call("prev_follow", { type: "pin" }), { pinned: "docs/a.md" });
  await r.select({ path: "src/one.txt", agent: null });
  assert.equal(f.path, "docs/a.md", "pinned: the selection does not move it");
  assert.deepEqual(await r.call("prev_follow", { type: "unpin" }), { following: true });
  assert.equal(f.path, "src/one.txt");
  assert.deepEqual(await r.call("prev_follow", { type: "open", path: "top.txt" }), { opened: "top.txt", mode: "text" });
  await r.select({ path: "docs/a.md", agent: null });
  assert.equal(f.path, "top.txt");
  assert.equal(r.bridge.watched.has("src/one.txt"), false, "the old file is no longer held");
});

test("the verbs refuse: bad_arg, outside_root, not_found, nothing_shown, not_an_agent, not_editing, not_mounted", async () => {
  const r = rig();
  project(r.bridge);
  await r.mountPreview("pv");
  assert.equal((await r.call("pv", { type: "open" }))["reason"], "bad_arg");
  assert.equal((await r.call("pv", { type: "open", path: "/etc/passwd" }))["reason"], "outside_root");
  assert.equal((await r.call("pv", { type: "open", path: "../x" }))["reason"], "outside_root");
  assert.equal((await r.call("pv", { type: "open", path: "nope.txt" }))["reason"], "not_found");
  await r.call("pv", { type: "unpin" });
  assert.equal((await r.call("pv", { type: "pin" }))["reason"], "nothing_shown");
  assert.equal((await r.call("pv", { type: "edit" }))["reason"], "nothing_shown");
  assert.equal((await r.call("pv", { type: "mode", mode: "moon" }))["reason"], "bad_arg");
  assert.equal((await r.call("pv", { type: "mode", mode: "agent" }))["reason"], "not_an_agent");
  assert.equal((await r.call("pv", { type: "save" }))["reason"], "not_editing");
  assert.equal((await r.call("pv", { type: "revert" }))["reason"], "nothing_to_revert");
  assert.equal((await r.call("pv", { type: "nope" }))["reason"], "unhandled");
  r.k.register(new Agent({ id: "pv_ghost", parentId: "canvas", handlerModule: "preview_view.ts" }));
  assert.equal((await r.call("pv_ghost", { type: "unpin" }))["reason"], "not_mounted");
});

test("what cannot be edited says why: binary, image, large, folder", async () => {
  const r = rig();
  project(r.bridge);
  r.bridge.put("blob.dat", "\u0000\u0001");
  r.bridge.binary.add("blob.dat");
  r.bridge.put("pic.png", "notreallyapng");
  r.bridge.put("big.log", "x".repeat(1024 * 1024 + 1));
  const p = await r.mountPreview("pv");
  const edit = async (path: string): Promise<Dict> => {
    await r.call("pv", { type: "open", path });
    return r.call("pv", { type: "edit" });
  };
  const b = await edit("blob.dat");
  assert.equal(p.mode, "binary", "the bridge's bad_arg on path is a binary file");
  assert.equal(b["reason"], "not_editable");
  assert.match(String(b["why"]), /binary/);
  assert.equal(p.content.k, "binary");
  assert.match(String((await edit("pic.png"))["why"]), /image/);
  assert.equal(p.content.k, "image");
  assert.match(String((await edit("big.log"))["why"]), /1 MiB/);
  assert.equal(p.content.k, "large");
  await p.page(1);
  assert.equal(p.content.k === "large" ? p.content.page : -1, 1, "a large file pages");
  assert.match(String((await edit("src"))["why"]), /folder/);
  assert.equal(p.content.k, "dir");
});

test("edit, type, save: the disk has the new text; another preview of the file updates; the echo is no conflict", async () => {
  const r = rig();
  project(r.bridge);
  const pin = await r.mountPreview("prev_pin", { pinned: "src/one.txt", follow: false });
  const other = await r.mountPreview("prev_follow", {});
  await r.select({ path: "src/one.txt", agent: null });
  const e = await r.call("prev_pin", { type: "edit" });
  assert.equal(e["editing"], "src/one.txt");
  pin.edit("one two");
  assert.equal(pin.dirty, true);
  const s = await r.call("prev_pin", { type: "save" });
  assert.equal(s["saved"], "src/one.txt");
  assert.equal(s["version"], versionOf("one two"));
  assert.equal(r.bridge.files.get("src/one.txt"), "one two");
  await settle();
  assert.equal(other.editor.text(), "one two", "the other preview followed the write");
  assert.equal(pin.editor.state.k, "viewing", "the echo of our own write is no conflict");
  assert.equal(pin.dirty, false);
});

test("a change on disk under unsaved edits is a loud conflict: nothing overwritten; compare, keep mine, reload", async () => {
  const r = rig();
  project(r.bridge);
  const p = await r.mountPreview("prev_pin", { pinned: "src/one.txt", follow: false });
  await r.call("prev_pin", { type: "edit" });
  p.edit("one four");
  r.bridge.put("src/one.txt", "disk wins");
  await settle();
  assert.equal(p.editor.state.k, "conflict");
  assert.equal(r.bridge.files.get("src/one.txt"), "disk wins", "nothing overwritten");
  const s = await r.call("prev_pin", { type: "save" });
  assert.equal(s["reason"], "conflict");
  assert.equal(s["expected"], versionOf("one"));
  assert.equal(s["actual"], versionOf("disk wins"));
  assert.equal(r.bridge.files.get("src/one.txt"), "disk wins");
  p.compare();
  assert.equal(p.editor.state.k, "comparing");
  // the disk changes again while the conflict is open: keep mine is against what was seen, never blind
  r.bridge.put("src/one.txt", "third writer");
  await p.keepMine();
  assert.equal(p.editor.state.k, "conflict", "refused again: a fresh conflict");
  assert.equal(r.bridge.files.get("src/one.txt"), "third writer");
  await p.keepMine();
  assert.equal(r.bridge.files.get("src/one.txt"), "one four", "keep mine, against the version just seen");
  assert.equal(p.editor.state.k, "viewing");

  p.edit("mine again");
  r.bridge.put("src/one.txt", "disk again");
  await settle();
  await p.reload();
  assert.equal(p.editor.text(), "disk again");
  assert.equal(p.dirty, false);
});

test("revert: edits drop; a save is undone through the bridge; then nothing to revert", async () => {
  const r = rig();
  project(r.bridge);
  const p = await r.mountPreview("pv", { pinned: "src/one.txt", follow: false });
  await r.call("pv", { type: "edit" });
  p.edit("changed");
  assert.equal((await r.call("pv", { type: "revert" }))["reverted"], "edits");
  assert.equal(p.editor.text(), "one");
  p.edit("saved text");
  await r.call("pv", { type: "save" });
  assert.equal(r.bridge.files.get("src/one.txt"), "saved text");
  const b = await r.call("pv", { type: "revert" });
  assert.equal(b["reverted"], "save");
  assert.equal(r.bridge.files.get("src/one.txt"), "one");
  await settle();
  assert.equal(p.editor.text(), "one");
  assert.equal((await r.call("pv", { type: "revert" }))["reason"], "nothing_to_revert");
});

test("a lost undo slot (the host restarted) is the bridge's refusal, passed on", async () => {
  const r = rig();
  project(r.bridge);
  const p = await r.mountPreview("pv", { pinned: "src/one.txt", follow: false });
  await r.call("pv", { type: "edit" });
  p.edit("x");
  await r.call("pv", { type: "save" });
  const was = r.bridge.call.bind(r.bridge);
  r.bridge.call = async (q) => (q["type"] === "undo" ? { error: "undo: nothing to undo", reason: "not_found" } : was(q));
  const x = await r.call("pv", { type: "revert" });
  assert.equal(x["reason"], "not_found");
  assert.match(String(x["error"]), /nothing to undo/);
});

test("an agent folder: file mode shows its agent.yaml, agent mode the agent; mode refuses elsewhere", async () => {
  const r = rig();
  project(r.bridge);
  const folder = ".fantastic/agents/user/agents/files";
  r.index.set(folder, { id: "files", handler: "file_bridge.tools", displayName: "", hasParent: true, hasMemory: false });
  const p = await r.mountPreview("prev_follow", {});
  await r.select({ path: folder, agent: "files" });
  assert.equal(p.path, `${folder}/agent.yaml`);
  assert.match(p.editor.text(), /file_bridge/);
  assert.deepEqual(await r.call("prev_follow", { type: "mode", mode: "agent" }), { mode: "agent" });
  assert.deepEqual(p.content, { k: "agent", agent: "files" });
  assert.equal(p.canEdit(), false);
  assert.deepEqual(await r.call("prev_follow", { type: "mode", mode: "file" }), { mode: "file" });
  assert.equal(p.content.k, "text");
  await r.select({ path: null, agent: "page_only" });
  assert.deepEqual(p.content, { k: "agent", agent: "page_only" }, "an agent with no folder is shown as the agent");
});

test("tree and previews on one folder: closing any keeps the others live; closing all leaves no hold", async () => {
  const r = rig();
  project(r.bridge);
  const tree = await r.mountTree("tree_1");
  await r.call("tree_1", { type: "expand", path: "src" });
  const a = await r.mountPreview("pa", { pinned: "src/one.txt", follow: false });
  const b = await r.mountPreview("pb", { pinned: "src/one.txt", follow: false });
  const dirp = await r.mountPreview("pd", { pinned: "src", follow: false });
  assert.equal(dirp.mode, "dir");
  a.dispose();
  previews.delete("pa");
  assert.ok(r.bridge.watched.has("src/one.txt"), "pb still holds the file");
  r.bridge.put("src/one.txt", "still live");
  await settle();
  assert.equal(b.editor.text(), "still live");
  b.dispose();
  previews.delete("pb");
  assert.ok(!r.bridge.watched.has("src/one.txt"));
  assert.ok(r.bridge.watched.has("src"), "the tree and the folder preview hold it");
  r.bridge.put("src/added.txt", "n");
  await settle();
  assert.ok(dirp.content.k === "dir" && dirp.content.entries.some((e) => e.name === "added.txt"));
  dirp.dispose();
  previews.delete("pd");
  assert.ok(r.bridge.watched.has("src"), "the tree still holds src");
  tree.dispose();
  controllers.delete("tree_1");
  await settle();
  assert.deepEqual([...r.bridge.watched], [], "no watch leaked");
});

test("a watch refused is said on the preview, and the file still shows", async () => {
  const r = rig();
  project(r.bridge);
  const was = r.bridge.call.bind(r.bridge);
  r.bridge.call = async (q) => (q["type"] === "watch_path" ? { error: "watch_path: denied", reason: "denied" } : was(q));
  const p = await r.mountPreview("pv", { pinned: "src/one.txt", follow: false });
  assert.match(p.notice ?? "", /not live: .*denied/);
  assert.equal(p.editor.text(), "one");
  assert.equal(p.isLive(), false);
});
