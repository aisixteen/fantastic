import { test } from "node:test";
import assert from "node:assert/strict";
import { file, ownCopiesReadNothing, run } from "./view_harness.ts";
import type { Obj } from "./view_harness.ts";

test("the mail view re-reads only on what its agent publishes, never on a copy of a call", async () => {
  const h = run(file("src/lib/rust/crates/bundles/fantastic-mail/src/mail_view.html"), { agent: "mail1" }, (_t, p) => {
    if (p["type"] === "reflect") return { id: "mail1", account: "me@x", mailboxes: ["INBOX", "Archive"], recent_sends: [] };
    if (p["type"] === "list") return { messages: [] };
    if (p["type"] === "inquiries") return { inquiries: [] };
    return {};
  });
  await h.settle();
  assert.ok(h.watches.has("mail1"), "the view watches its agent");
  assert.deepEqual(h.intervals, [], "no timer");
  await ownCopiesReadNothing(h);
  const reads = async (ev: Obj): Promise<string[]> => {
    const mark = h.sends.length;
    await h.publish("mail1", ev);
    return h.since(mark).map((s) => [String(s.p["type"]), s.p["mailbox"]].filter((x) => x !== undefined).join(" "));
  };
  // copies of calls: the view's own reads, anyone's mark or send
  for (const copy of [{ type: "reflect" }, { type: "list", mailbox: "INBOX", unseen: true, limit: 100 }, { type: "inquiries" }, { type: "mark", mailbox: "INBOX", uid: 2, seen: true }, { type: "send", to: ["a@b"], subject: "s", text: "t" }]) {
    assert.deepEqual(await reads(copy), [], `a copy read again: ${JSON.stringify(copy)}`);
  }
  // the agent's published changes
  assert.deepEqual(await reads({ type: "marked", mailbox: "Archive", uid: 2 }), ["list Archive"], "a mark: that box's count");
  assert.deepEqual(await reads({ type: "sent", message_id: "m1", to: ["a@b"] }), ["reflect"], "a sent mail: the recent sends");
  assert.deepEqual(await reads({ type: "inquiry_opened", re: 3, asker: null, holder: "mail1" }), ["inquiries"], "a send waits for a person");
  assert.deepEqual(await reads({ type: "inquiry_closed", re: 3, asker: "chat1", holder: "user" }), [], "another holder's question");
});
