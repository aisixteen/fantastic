// file_bridge's view agents — the `file` and `folder` views over one bridge
// (`FantasticFile/Views.swift`). Mirrors py `file_bridge/tests/test_views.py` and
// `test_views_units.py`, and rust `fantastic-file/tests_views.rs`.

@testable import FantasticFile
import FantasticJSON
@testable import FantasticKernel
import Foundation
import OrderedCollections
import Testing

@Suite("file_bridge view agents (open_view, scoping, relay)")
struct FileViewsTests {

    /// An open file_bridge `fs` over a fresh cwd-relative dir, with its own root agent.
    private func bridge(allow: [String] = ["."]) async -> (Kernel, URL) {
        let registry = BundleRegistry()
        registry.register("file_bridge.tools", FileBundle())
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(
            id: AgentId("kernel_state"), handlerModule: nil, parentId: nil,
            rootPath: URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
                .appendingPathComponent(".fantastic"))
        kernel.register(root)
        kernel.setRoot(root)
        let rel = "fantastic-fileviews-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
            .appendingPathComponent(rel)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let meta: OrderedDictionary<String, JSON> = [
            "type": "create_agent", "parent_id": "kernel_state",
            "handler_module": "file_bridge.tools", "id": "fs", "root": .string(rel),
            "allow": .array(allow.map { .string($0) }), "ingress_rule": "allow_all_ingress",
        ]
        let r = await kernel.send([Item("kernel_state", .object(meta))])[0].result
        #expect(r["id"].asString == "fs", "\(r)")
        return (kernel, dir)
    }

    private func call(_ k: Kernel, _ target: String, _ payload: OrderedDictionary<String, JSON>)
        async -> JSON
    {
        await k.send([Item(AgentId(target), .object(payload))])[0].result
    }

    private func open(_ k: Kernel, _ kind: String, _ path: String) async -> JSON {
        await call(k, "fs", ["type": "open_view", "kind": .string(kind), "path": .string(path)])
    }

    private func write(_ dir: URL, _ path: String, _ text: String) {
        let url = dir.appendingPathComponent(path)
        try? FileManager.default.createDirectory(
            at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        try? Data(text.utf8).write(to: url)
    }

    @Test func openViewMakesOneViewPerPath() async {
        let (k, dir) = await bridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        write(dir, "notes.md", "hi\n")
        let first = await open(k, "file", "notes.md")
        #expect(first["created"].asBool == true, "\(first)")
        #expect(first["kind"].asString == "file")
        let vid = first["id"].asString ?? ""
        // One agent per path: a second open reuses the view.
        let again = await open(k, "file", "notes.md")
        #expect(again["id"].asString == vid)
        #expect(again["created"].asBool == false)

        // The view is the bridge's child, a file_bridge record carrying its `view`.
        let rec = k.agent(AgentId(vid))
        #expect(rec?.metaValue(forKey: "view")?.asString == "file")
        #expect(rec?.metaValue(forKey: "path")?.asString == "notes.md")
        #expect(rec?.parentId?.value == "fs")
        // Its grant on the bridge is its own verbs only, and the bridge's `callable_by`
        // names it for exactly those, plus the scope token that holds it to its path.
        let grant = ["read", "write", "watch_path", "unwatch_path"]
        #expect(rec?.metaValue(forKey: "can_call")?["fs"].asArray?.compactMap(\.asString) == grant)
        let callable = k.agent(AgentId("fs"))?.metaValue(forKey: "callable_by")?[vid]
        #expect(
            callable?.asArray?.compactMap(\.asString)
                == ["file:notes.md", "read", "unwatch_path", "watch_path", "write"])
    }

    @Test func aNewViewAdmitsTheCallersOfItsBridgeForTheVerbsItAnswers() async {
        let (k, dir) = await bridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        write(dir, "notes.md", "hi\n")
        // the bridge's callers reach the view, for the verbs the view answers only
        k.update(AgentId("fs"), ["callable_by": .object([
            "ws": .string("*"), "user": .array([.string("read"), .string("list")]),
        ])])
        let vid = (await open(k, "file", "notes.md"))["id"].asString ?? ""
        let cb = k.agent(AgentId(vid))?.metaValue(forKey: "callable_by")
        #expect(cb?["ws"].asArray?.compactMap(\.asString) == ["read", "reflect", "watch", "write"])
        #expect(cb?["user"].asArray?.compactMap(\.asString) == ["read"])
        #expect(cb?["$self"].asArray == nil, "the bridge's own self is no caller of its view")
    }

    @Test func aFolderViewIsOnePerFolder() async {
        let (k, dir) = await bridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        write(dir, "docs/a.txt", "a")
        let first = await open(k, "folder", "docs")
        #expect(first["created"].asBool == true, "\(first)")
        let again = await open(k, "folder", "docs")
        #expect(again["id"].asString == first["id"].asString)
        #expect(again["created"].asBool == false)
    }

    @Test func openViewRefusesBadArgumentsAndWhatTheBridgeDoesNotExpose() async {
        let (k, dir) = await bridge(allow: ["docs"])
        defer { try? FileManager.default.removeItem(at: dir) }
        write(dir, "docs/a.txt", "a")
        write(dir, "secret.txt", "s")
        #expect(await open(k, "dir", "docs")["reason"].asString == "bad_arg")
        #expect(await open(k, "file", "/etc/passwd")["reason"].asString == "bad_arg")
        #expect(await open(k, "file", ".")["reason"].asString == "bad_arg")
        // Outside `allow` is the same answer whether or not the file exists.
        #expect(await open(k, "file", "secret.txt")["reason"].asString == "not_found")
        #expect(await open(k, "file", "docs/a.txt")["created"].asBool == true)
    }

    @Test func aFileViewReadsAndWritesItsOwnPathAndRefusesAnother() async {
        let (k, dir) = await bridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        write(dir, "notes.md", "hi\n")
        write(dir, "other.md", "o\n")
        let vid = (await open(k, "file", "notes.md"))["id"].asString ?? ""
        let read = await call(k, vid, ["type": "read"])
        #expect(read["lines"][0]["text"].asString == "hi", "\(read)")
        let wrote = await call(k, vid, ["type": "write", "bytes": "new\n"])
        #expect(wrote["size"].asInt == 4, "\(wrote)")
        let back = await call(k, vid, ["type": "read"])
        #expect(back["lines"][0]["text"].asString == "new")
        // Another path is refused by the view, before the bridge is asked.
        let other = await call(k, vid, ["type": "read", "path": "other.md"])
        #expect(other["reason"].asString == "unauthorized", "\(other)")
    }

    @Test func aFolderViewListsInsideItselfAndRefusesEscape() async {
        let (k, dir) = await bridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        write(dir, "docs/a.txt", "a")
        write(dir, "docs/sub/b.txt", "b")
        write(dir, "outside.txt", "x")
        let vid = (await open(k, "folder", "docs"))["id"].asString ?? ""
        let listed = await call(k, vid, ["type": "list"])
        let names = (listed["files"].asArray ?? []).compactMap { $0["name"].asString }
        #expect(names.contains("a.txt") && names.contains("sub"), "\(listed)")
        #expect(!names.contains("outside.txt"))
        let escape = await call(k, vid, ["type": "list", "path": "../"])
        #expect(escape["reason"].asString == "unauthorized", "\(escape)")
    }

    @Test func revealShowsTheWayDownAndRefusesWhatIsNotThere() async {
        let (k, dir) = await bridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        write(dir, "docs/sub/deep.txt", "d")
        let vid = (await open(k, "folder", "docs"))["id"].asString ?? ""
        let shown = await call(k, vid, ["type": "reveal", "path": "sub/deep.txt"])
        #expect(shown["revealed"].asString == "sub/deep.txt", "\(shown)")
        #expect(shown["dirs"].asArray?.compactMap(\.asString) == ["sub"])
        let missing = await call(k, vid, ["type": "reveal", "path": "nope.txt"])
        #expect(missing["reason"].asString == "not_found")
    }

    @Test(.timeLimit(.minutes(1)))
    func aFileViewRelaysItsChangesToItsWatchers() async {
        let (k, dir) = await bridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        write(dir, "notes.md", "hi\n")
        let vid = (await open(k, "file", "notes.md"))["id"].asString ?? ""
        let inbox = k.ensureInbox(AgentId("t_view"))
        #expect(k.watch(src: AgentId(vid), watcher: AgentId("t_view")) == nil)
        let watched = await call(k, vid, ["type": "watch"])
        #expect(watched["relayed"].asBool == false, "\(watched)")
        write(dir, "notes.md", "changed\n")
        // The bridge's disk watch reports the write; the view relays it as its own event.
        while let item = await inbox.next() {
            if item.payload["type"].asString == "changed" {
                #expect(item.payload["path"].asString == "notes.md")
                break
            }
        }
        _ = await call(k, vid, ["type": "watch"])  // idempotent: still one relay
        k.removeInbox(AgentId("t_view"))
    }

    @Test func theViewRelayRuleKeepsOnlyItsOwnPath() {
        let file = viewRelayEvent(
            VIEW_FILE, own: "notes.md",
            ev: ["type": "changed", "path": "other.md", "kind": "modified"])
        #expect(file == nil)
        let mine = viewRelayEvent(
            VIEW_FILE, own: "notes.md",
            ev: ["type": "changed", "path": "notes.md", "kind": "deleted"])
        #expect(mine?["kind"].asString == "deleted")
        // A folder: its entries become added / changed / removed, named by their leaf.
        let gone = viewRelayEvent(
            VIEW_FOLDER, own: "docs", ev: ["type": "changed", "path": "docs/a.txt", "kind": "deleted"])
        #expect(gone?["type"].asString == "removed")
        #expect(gone?["name"].asString == "a.txt")
        let made = viewRelayEvent(
            VIEW_FOLDER, own: "docs", ev: ["type": "changed", "path": "docs/b.txt", "kind": "added"])
        #expect(made?["type"].asString == "added")
        // Something deeper than its entries, or the folder itself, is not the folder's.
        #expect(
            viewRelayEvent(
                VIEW_FOLDER, own: "docs", ev: ["type": "changed", "path": "docs/sub/x", "kind": "added"])
                == nil)
        #expect(
            viewRelayEvent(
                VIEW_FOLDER, own: "docs", ev: ["type": "changed", "path": "docs", "kind": "modified"])
                == nil)
    }

    @Test func theViewPathScopeIsItsOwnPathOrFolder() {
        #expect(viewBridgePath(VIEW_FILE, own: "notes.md", rel: "") == "notes.md")
        #expect(viewBridgePath(VIEW_FILE, own: "notes.md", rel: "other.md") == nil)
        #expect(viewBridgePath(VIEW_FOLDER, own: "docs", rel: "a/b") == "docs/a/b")
        #expect(viewBridgePath(VIEW_FOLDER, own: "docs", rel: "") == "docs")
        #expect(viewBridgePath(VIEW_FOLDER, own: "docs", rel: "../x") == nil)
        #expect(viewBridgePath(VIEW_FOLDER, own: ".", rel: "a") == "a")
    }

    /// python `test_docs_of_a_view_lists_only_the_views_own_verbs`: a view documents its
    /// own table, and the bridge keeps the whole one.
    @Test func docsOfAViewListTheViewsOwnVerbsOnly() async {
        let (k, dir) = await bridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        write(dir, "a.txt", "one\n")
        let f = await open(k, "file", "a.txt")["id"].asString ?? ""
        let fv = await call(k, "kernel", ["type": "docs", "of": .string(f)])
        let names = (fv["verbs"].asObject ?? [:]).keys.sorted()
        #expect(names == ["read", "reflect", "watch", "write"], "\(fv)")
        let whole = await call(k, "kernel", ["type": "docs", "of": "fs"])
        #expect((whole["verbs"].asObject ?? [:])["delete"] != nil)
    }

    /// python `test_read_reports_whether_the_file_ends_in_a_newline`.
    @Test func readReportsWhetherTheFileEndsInANewline() async {
        let (k, dir) = await bridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        write(dir, "with.txt", "a\nb\n")
        write(dir, "without.txt", "a\nb")
        write(dir, "empty.txt", "")
        for (name, want) in [("with.txt", true), ("without.txt", false), ("empty.txt", false)] {
            let r = await call(k, "fs", ["type": "read", "path": .string(name)])
            #expect(r["newline"].asBool == want, "\(name): \(r)")
        }
    }

    @Test func aChosenViewIdIsTakenOnlyWhenItIsFree() async {
        let (k, dir) = await bridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        write(dir, "a.md", "x\n")
        write(dir, "b.md", "y\n")
        func openWith(_ kind: String, _ path: String, _ id: JSON) async -> JSON {
            await call(
                k, "fs",
                ["type": "open_view", "kind": .string(kind), "path": .string(path), "id": id])
        }
        let made = await openWith("file", "a.md", .string("fv-a"))
        #expect(made["created"].asBool == true && made["id"].asString == "fv-a", "\(made)")
        let again = await openWith("file", "a.md", .string("fv-a"))
        #expect(again["created"].asBool == false && again["id"].asString == "fv-a", "\(again)")
        // another agent's id, a view of another path, a second id for one path: refused
        #expect(await openWith("file", "a.md", .string("fs"))["reason"].asString == "exists")
        #expect(await openWith("file", "b.md", .string("fv-a"))["reason"].asString == "exists")
        let other = await openWith("file", "a.md", .string("fv-other"))
        #expect(other["reason"].asString == "exists" && other["id"].asString == "fv-a", "\(other)")
        #expect(await openWith("file", "b.md", .string("x/y"))["reason"].asString == "bad_arg")
    }

    @Test func aViewCallerIsHeldToItsPathAtTheBridge() async {
        let (k, dir) = await bridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        write(dir, "a.md", "one\n")
        write(dir, "b.md", "two\n")
        write(dir, "dir/x.md", "x\n")
        let fv = AgentId((await open(k, "file", "a.md"))["id"].asString ?? "")
        let dv = AgentId((await open(k, "folder", "dir"))["id"].asString ?? "")
        func read(_ path: String) -> JSON { .object(["path": .string(path)]) }
        let refused = viewPathGate(k, bridge: "fs", origin: fv, verb: "read", payload: read("b.md"))
        #expect(refused?["reason"].asString == "unauthorized", "\(String(describing: refused))")
        #expect(refused?["error"].asString?.contains("outside this file view") == true)
        #expect(viewPathGate(k, bridge: "fs", origin: fv, verb: "read", payload: read("a.md")) == nil)
        let watch = viewPathGate(
            k, bridge: "fs", origin: fv, verb: "watch_path", payload: read("dir"))
        #expect(watch?["reason"].asString == "unauthorized")
        let folder = viewPathGate(k, bridge: "fs", origin: dv, verb: "read", payload: read("a.md"))
        #expect(folder?["error"].asString?.contains("outside this folder view") == true)
        #expect(viewPathGate(k, bridge: "fs", origin: dv, verb: "read", payload: read("dir/x.md")) == nil)
        // an operator, or a plain agent, is not path-scoped here
        #expect(viewPathGate(k, bridge: "fs", origin: nil, verb: "read", payload: read("b.md")) == nil)
        let bad = viewPathGate(k, bridge: "fs", origin: fv, verb: "read", payload: .object(["path": .bool(true)]))
        #expect(bad?["reason"].asString == "bad_arg")
    }

    @Test func aViewCallerIsHeldToItsPathOnTheBinaryChannel() async {
        let (k, dir) = await bridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        write(dir, "a.md", "one\n")
        write(dir, "b.md", "two\n")
        let fv = AgentId((await open(k, "file", "a.md"))["id"].asString ?? "")
        // the view calls its bridge AS ITSELF, a document riding the binary body
        func asView(_ header: OrderedDictionary<String, JSON>, _ body: Data) async -> (JSON, Data?) {
            let item = Item(AgentId("fs"), .object(header), binary: Blob("bytes", body))
            let out = await KernelTaskLocals.$sender.withValue(fv) { await k.send([item])[0] }
            return (out.result, out.body?.bytes)
        }
        let refused = await asView(["type": "write", "path": "b.md"], Data("x\n".utf8))
        #expect(refused.0["reason"].asString == "unauthorized", "\(refused.0)")
        #expect(refused.0["error"].asString?.contains("outside this file view") == true)
        #expect(refused.1 == nil)
        #expect(
            (try? String(contentsOf: dir.appendingPathComponent("b.md"), encoding: .utf8)) == "two\n")
        // its own path is still reached on the binary channel
        let inside = await asView(["type": "read", "path": "a.md"], Data())
        #expect(inside.0["lines"][0]["text"].asString == "one", "\(inside.0)")
    }

    @Test func theBridgeHoldsAViewToTheScopeInItsCallableBy() async {
        let (k, dir) = await bridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        write(dir, "dir/sub/y.md", "y\n")
        write(dir, "dir/x.md", "x\n")
        let dv = AgentId((await open(k, "folder", "dir"))["id"].asString ?? "")
        func read(_ path: String) -> JSON { .object(["path": .string(path)]) }
        #expect(viewPathGate(k, bridge: "fs", origin: dv, verb: "read", payload: read("dir/x.md")) == nil)
        // The bridge's entry narrows the view to `dir/sub`: `dir/x.md` is refused at the
        // bridge, although the view's own record still says `dir`.
        setCallableBy(k, dv, .array([
            .string("folder:dir/sub"), .string("list"), .string("watch_path"), .string("unwatch_path"),
        ]))
        let narrowed = viewPathGate(k, bridge: "fs", origin: dv, verb: "read", payload: read("dir/x.md"))
        #expect(narrowed?["reason"].asString == "unauthorized", "\(String(describing: narrowed))")
        #expect(
            viewPathGate(k, bridge: "fs", origin: dv, verb: "list", payload: read("dir/sub")) == nil)
        // An entry with no single scope token is a view refused in full.
        setCallableBy(k, dv, .array([.string("list"), .string("watch_path")]))
        let none = viewPathGate(k, bridge: "fs", origin: dv, verb: "list", payload: read("dir/sub"))
        #expect(none?["reason"].asString == "unauthorized", "\(String(describing: none))")
    }

    @Test func theBridgeRelaysAViewOnlyTheChangesInItsScope() async {
        let (k, dir) = await bridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        write(dir, "dir/x.md", "x\n")
        write(dir, "other.md", "o\n")
        let dv = AgentId((await open(k, "folder", "dir"))["id"].asString ?? "")
        // The view's relay inbox, and the holder of `dir` the bridge pushes to. The view
        // is not armed here, so this inbox has no other reader.
        let inbox = k.ensureInbox(viewRelayInbox(dv.value))
        let holders: [String: Set<String>] = ["dir": [dv.value]]
        viewRelayToViews(k, bridge: "fs", holders: holders, rel: "dir/a.md", kind: "added")
        let got = inbox.tryTake()?.payload
        #expect(got?["path"].asString == "dir/a.md" && got?["kind"].asString == "added", "\(String(describing: got))")
        viewRelayToViews(k, bridge: "fs", holders: holders, rel: "other.md", kind: "modified")
        #expect(inbox.tryTake() == nil, "outside the view's folder")
        // The entry narrows the view to `dir/sub`: `dir/a.md` is no longer relayed.
        setCallableBy(k, dv, .array([.string("folder:dir/sub"), .string("list")]))
        viewRelayToViews(k, bridge: "fs", holders: holders, rel: "dir/a.md", kind: "added")
        #expect(inbox.tryTake() == nil, "narrowed scope")
        // No single scope token: fail closed, nothing relayed.
        setCallableBy(k, dv, .array([.string("list")]))
        viewRelayToViews(k, bridge: "fs", holders: holders, rel: "dir/a.md", kind: "added")
        #expect(inbox.tryTake() == nil, "no scope token")
        // No relay inbox: the change is dropped, and the drop is reported.
        #expect(viewDeliver(k, view: "gone", payload: .object([:])) == false)
        k.removeInbox(viewRelayInbox(dv.value))
    }

    /// Replace one caller's entry in the bridge's `callable_by` (the scope a view is held to).
    private func setCallableBy(_ k: Kernel, _ caller: AgentId, _ entry: JSON) {
        var cb = k.agent(AgentId("fs"))?.metaValue(forKey: "callable_by")?.asObject ?? [:]
        cb[caller.value] = entry
        k.update(AgentId("fs"), ["callable_by": .object(cb)])
    }

    @Test func aRelayEventWithoutAKnownKindIsDropped() {
        let noKind: [JSON] = [
            .object(["path": .string("a.md")]),
            .object(["path": .string("a.md"), "kind": .string("moved")]),
        ]
        for ev in noKind {
            #expect(viewRelayEvent(VIEW_FILE, own: "a.md", ev: ev) == nil)
            #expect(viewRelayEvent(VIEW_FOLDER, own: "docs", ev: ev) == nil)
        }
        let changed = JSON.object(["path": .string("docs/n.md"), "kind": .string("modified")])
        #expect(viewRelayEvent(VIEW_FOLDER, own: "docs", ev: changed)?["type"].asString == "changed")
    }
}
