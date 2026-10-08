// `watch_path` — python `file_bridge/tests/test_file_watch.py` and rust
// `fantastic-file/src/tests_watch.rs`, case for case: against the real disk, a
// direct write (an editor), an atomic save (a temp file renamed over), a write
// through the bridge, a delete, a directory's entries; refusals; `unwatch_path`
// ends it.

import FantasticFile
import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections
import Testing

@Suite("file_bridge watch_path")
struct FileWatchTests {

    /// A kernel whose running dir is a fresh temp dir, with an OPEN file_bridge
    /// `fb` rooted at `served/` exposing `allow`. Returns (kernel, served dir, the
    /// temp dir to remove).
    private func boot(allow: JSON) async -> (Kernel, URL, URL) {
        let fm = FileManager.default
        let tmp = URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("fantastic-watch-\(UUID().uuidString)")
        let served = tmp.appendingPathComponent("served")
        try? fm.createDirectory(at: served, withIntermediateDirectories: true)
        let registry = BundleRegistry()
        registry.register("file_bridge.tools", FileBundle())
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(
            id: AgentId("kernel_state"), handlerModule: nil, parentId: nil,
            rootPath: tmp.appendingPathComponent(".fantastic"))
        kernel.register(root)
        kernel.setRoot(root)
        let r = await kernel.send([
            Item(
                "kernel_state",
                .object([
                    "type": "create_agent", "parent_id": "kernel_state",
                    "handler_module": "file_bridge.tools", "id": "fb", "root": "served",
                    "ingress_rule": "allow_all_ingress", "allow": allow,
                ]))
        ])[0].result
        #expect(r["id"].asString == "fb", "\(r)")
        return (kernel, served, tmp)
    }

    private func call(_ k: Kernel, _ payload: OrderedDictionary<String, JSON>) async -> JSON {
        await k.send([Item(AgentId("fb"), .object(payload))])[0].result
    }

    private func inbox(_ k: Kernel) -> Inbox {
        let w = AgentId("_t_watch")
        let q = k.ensureInbox(w)
        #expect(k.watch(src: AgentId("fb"), watcher: w) == nil)
        return q
    }

    /// The next `changed` for `path` within `secs` (other events skipped).
    private func nextChanged(_ q: Inbox, _ path: String, _ secs: Double) async -> JSON? {
        let deadline = Date().addingTimeInterval(secs)
        while Date() < deadline {
            if let item = q.tryTake() {
                if item.payload["type"] == "changed" && item.payload["path"].asString == path {
                    return item.payload
                }
                continue
            }
            try? await Task.sleep(nanoseconds: 20_000_000)
        }
        return nil
    }

    private func write(_ url: URL, _ text: String) {
        try? Data(text.utf8).write(to: url)
    }

    private func settle() async {
        try? await Task.sleep(nanoseconds: 300_000_000)
    }

    @Test func aDirectWriteAnAtomicSaveAndABridgeWriteAreSeen() async throws {
        let (k, served, tmp) = await boot(allow: ["."])
        defer { try? FileManager.default.removeItem(at: tmp) }
        let prompt = served.appendingPathComponent("prompt.md")
        write(prompt, "one\n")
        let q = inbox(k)
        let r = await call(k, ["type": "watch_path", "path": "prompt.md"])
        #expect(r == ["watching": "prompt.md", "paths": ["prompt.md"]], "\(r)")
        await settle()
        write(prompt, "two\n")
        let ev = await nextChanged(q, "prompt.md", 5)
        #expect(
            ["modified", "added"].contains(ev?["kind"].asString ?? ""),
            "an editor's save: \(String(describing: ev))")
        let tmpFile = served.appendingPathComponent(".prompt.md.tmp")
        write(tmpFile, "three\n")
        _ = rename(tmpFile.path, prompt.path)
        #expect(await nextChanged(q, "prompt.md", 5) != nil, "an atomic save")
        let w = await call(k, ["type": "write", "path": "prompt.md", "bytes": "four\n"])
        #expect(!w["version"].isNull, "\(w)")
        #expect(await nextChanged(q, "prompt.md", 5) != nil, "a write through the bridge")
        try FileManager.default.removeItem(at: prompt)
        let gone = await nextChanged(q, "prompt.md", 5)
        #expect(gone?["kind"] == "deleted", "a delete: \(String(describing: gone))")
    }

    @Test func aDirectoryReportsItsEntries() async throws {
        let (k, served, tmp) = await boot(allow: ["."])
        defer { try? FileManager.default.removeItem(at: tmp) }
        try FileManager.default.createDirectory(
            at: served.appendingPathComponent("notes"), withIntermediateDirectories: true)
        let q = inbox(k)
        #expect(await call(k, ["type": "watch_path", "path": "notes"])["watching"] == "notes")
        await settle()
        write(served.appendingPathComponent("notes/a.md"), "x")
        #expect(await nextChanged(q, "notes/a.md", 5) != nil, "a new entry")
        await settle()
        write(served.appendingPathComponent("notes/a.md"), "xy")
        #expect(await nextChanged(q, "notes/a.md", 5)?["kind"] == "modified", "an entry's edit")
    }

    @Test func onlyTheWatchedPathIsReportedAndUnwatchEndsIt() async throws {
        let (k, served, tmp) = await boot(allow: ["."])
        defer { try? FileManager.default.removeItem(at: tmp) }
        write(served.appendingPathComponent("prompt.md"), "one\n")
        let q = inbox(k)
        _ = await call(k, ["type": "watch_path", "path": "prompt.md"])
        await settle()
        write(served.appendingPathComponent("other.md"), "noise")
        #expect(await nextChanged(q, "other.md", 1) == nil)
        let r = await call(k, ["type": "unwatch_path", "path": "prompt.md"])
        #expect(r == ["watching": false, "paths": []], "\(r)")
        await settle()
        write(served.appendingPathComponent("prompt.md"), "two\n")
        #expect(await nextChanged(q, "prompt.md", 1) == nil)
    }

    @Test func whatIsNotExposedOrNotThereIsRefused() async throws {
        let (k, served, tmp) = await boot(allow: ["docs"])
        defer { try? FileManager.default.removeItem(at: tmp) }
        write(served.appendingPathComponent("secret.md"), "no")
        for extra: OrderedDictionary<String, JSON> in [
            ["path": "secret.md"], ["path": "docs/missing/deep.md"], [:],
        ] {
            var payload: OrderedDictionary<String, JSON> = ["type": "watch_path"]
            for (key, v) in extra { payload[key] = v }
            let r = await call(k, payload)
            #expect(r["reason"] == "not_found", "\(payload): \(r)")
        }
    }

    @Test func deletingTheBridgeEndsItsWatchTask() async throws {
        let (k, served, tmp) = await boot(allow: ["."])
        defer { try? FileManager.default.removeItem(at: tmp) }
        write(served.appendingPathComponent("prompt.md"), "one\n")
        _ = await call(k, ["type": "watch_path", "path": "prompt.md"])
        func watching() async -> Bool {
            let ps = await k.send([Item(AgentId("kernel"), .object(["type": "ps"]))])[0].result
            return (ps["tasks"].asArray ?? []).contains { $0["verb"] == "watch_path" }
        }
        #expect(await watching())
        _ = await k.send([Item("kernel_state", .object(["type": "delete_agent", "id": "fb"]))])
        var ended = false
        for _ in 0..<100 where !ended {
            ended = !(await watching())
            if !ended { try? await Task.sleep(nanoseconds: 20_000_000) }
        }
        #expect(ended, "the watch task outlived its bridge")
    }
}
