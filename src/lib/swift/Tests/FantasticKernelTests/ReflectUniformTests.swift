// Unit tests for the uniform `reflect` surface — tree/bundles/readme
// flags and the `description` field. Mirrors Python's test_reflect_flags
// and Rust's reflect/tests.rs.

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

import FantasticKernelState

@testable import FantasticKernel

@Suite("Uniform reflect")
struct ReflectUniformTests {
    @Test func rootReflectIsUniform() async {
        let kernel = makeKernel()
        let r = await kernel.send([Item("kernel_state", ["type": "reflect"])])[0].result
        #expect(r["id"].asString == "kernel_state")
        #expect(r["sentence"].asString?.hasPrefix("Fantastic kernel") == true)
        #expect(r["parent_id"].isNull)
        // tree default = all.
        #expect(r["tree"]["id"].asString == "kernel_state")
        // bundles omitted by default.
        #expect(r["bundles"].asArray == nil)
        // kernel runtime identity — root only, lowercase enum.
        #expect(r["runtime"].asString == "swift")
    }

    @Test func rootReflectSurfacesGuidFromMeta() async {
        let kernel = makeKernel()
        // Bare root has no guid in meta → reflect surfaces an explicit null
        // (persistence-scoped field, always present on the root reflect).
        let bare = await kernel.send([Item("kernel_state", ["type": "reflect"])])[0].result
        #expect(bare["guid"].isNull)
        // Seed a 32-hex guid into root meta → reflect surfaces it verbatim.
        kernel.root?.updateMeta(["guid": .string("0123456789abcdef0123456789abcdef")])
        let r = await kernel.send([Item("kernel_state", ["type": "reflect"])])[0].result
        #expect(r["guid"].asString == "0123456789abcdef0123456789abcdef")
        // Non-root reflects never carry guid.
        _ = await kernel.send([Item("kernel_state", ["type": "create_agent", "parent_id": "kernel_state", "handler_module": "echo.tools", "id": "kid"])])[0].result
        let kid = await kernel.send([Item("kid", ["type": "reflect"])])[0].result
        #expect(kid["guid"] == nil || kid["guid"].isNull)
    }

    @Test func treeTiers() async {
        let kernel = makeKernel()
        _ = await kernel.send([Item("kernel_state", ["type": "create_agent", "parent_id": "kernel_state", "handler_module": "echo.tools", "id": "kid"])])[0].result
        // tree=ids → flat list, root first.
        let ids = await kernel.send([Item("kernel_state", ["type": "reflect", "tree": "ids"])])[0].result
        #expect(ids["tree"].asArray?.compactMap { $0.asString } == ["kernel_state", "kid"])
        // tree=none → omitted.
        let none = await kernel.send([Item("kernel_state", ["type": "reflect", "tree": "none"])])[0].result
        #expect(none["tree"] == nil || none["tree"].isNull)
    }

    @Test func bundlesTiers() async {
        let kernel = makeKernel()
        let all = await kernel.send([Item("kernel_state", ["type": "reflect", "bundles": "all"])])[0].result
        let arr = all["bundles"].asArray
        #expect(arr != nil && !(arr!.isEmpty))
        #expect(arr?.first?["name"].asString != nil)
        #expect(arr?.first?["handler_module"].asString != nil)
        let ids = await kernel.send([Item("kernel_state", ["type": "reflect", "bundles": "ids"])])[0].result
        #expect(ids["bundles"].asArray?.allSatisfy { $0.asString != nil } == true)
    }

    /// The catalog NAME is the entry-point name, never the module path:
    /// python's `_available_bundles` pages `ep.name` (`kernel_state`) with
    /// `ep.value` (`kernel_state.tools`) as `handler_module`.
    @Test func catalogNamesTheEntryPointNotTheModule() async {
        let kernel = makeLoaderKernel()
        let all = await kernel.send([Item("kernel_state", ["type": "reflect", "bundles": "all"])])[0].result
        let entry = all["bundles"].asArray?.first { $0["handler_module"].asString == "kernel_state.tools" }
        #expect(entry?["name"].asString == "kernel_state")
        let ids = await kernel.send([Item("kernel_state", ["type": "reflect", "bundles": "ids"])])[0].result
        let names = ids["bundles"].asArray?.compactMap { $0.asString } ?? []
        #expect(names.contains("kernel_state"))
        #expect(!names.contains("kernel_state.tools"))
    }

    @Test func descriptionSurfacesTopAndTree() async {
        let kernel = makeKernel()
        _ = await kernel.send([Item("kernel_state", [
                "type": "create_agent", "parent_id": "kernel_state", "handler_module": "echo.tools",
                "id": "kid", "description": "holds my notes",
            ])])[0].result
        // top-level on the child's own reflect.
        let own = await kernel.send([Item("kid", ["type": "reflect"])])[0].result
        #expect(own["description"].asString == "holds my notes")
        // and in the parent's tree=all node.
        let root = await kernel.send([Item("kernel_state", ["type": "reflect"])])[0].result
        let node = root["tree"]["children"].asArray?.first { $0["id"].asString == "kid" }
        #expect(node?["description"].asString == "holds my notes")
    }

    @Test func readmeFlagTiersFromTheBundle() async {
        let kernel = makeLoaderKernel()
        // default → no readme key.
        let plain = await kernel.send([Item("kernel_state", ["type": "reflect"])])[0].result
        #expect(plain["readme"] == nil || plain["readme"].isNull)
        // readme=true → the root bundle's readme (kernel_state's).
        let r = await kernel.send([Item("kernel_state", ["type": "reflect", "readme": true])])[0].result
        #expect(r["readme"].asString == RootReadme.text)
        #expect(r["readme"].asString?.hasPrefix("# This is a Fantastic kernel.") == true)
        // readme honored.
        let legacy = await kernel.send([Item("kernel_state", ["type": "reflect", "readme": true])])[0].result
        #expect(legacy["readme"].asString == RootReadme.text)
    }

    @Test func embeddedReadmeIsNonEmpty() {
        // The bundled resource must load (Bundle.module wired correctly).
        #expect(!RootReadme.text.isEmpty)
        #expect(RootReadme.text.contains("reflect"))
    }

    // ─── keyed / tiered doc tree ───────────────────────────────

    @Test func embeddedDocsLoad() {
        // The `docs/` resource tree must load (Bundle.module wired), each with a
        // `> ` descriptor line the index reads.
        #expect(RootDocs.all.count == RootDocs.keys(RootDocs.tree).count)
        #expect(RootDocs.all.contains { $0.key == "self-extension" })
        for (key, body) in RootDocs.all {
            #expect(!body.isEmpty, "empty doc \(key)")
        }
    }

    /// The authoring doc must teach the FILE+COMMAND execution model — a durable
    /// program is a FILE you write (the stream verbs, the document verbatim in
    /// `<bytes>`) plus a command you run on the general-purpose exec_bridge
    /// COMMAND runner. No `code` field.
    @Test func selfExtensionDocStatesTheFileCommandModel() throws {
        let body = try #require(RootDocs.all.first { $0.key == "self-extension" }?.body)
        #expect(body.contains("write_stream"), "\(body)")
        #expect(body.contains("<bytes>"), "\(body)")
        // `send_binary` is GONE: there is no such tool in the call grammar, and
        // the byte path is the verbatim `<bytes>` value.
        #expect(!body.contains("send_binary"), "\(body)")
        #expect(body.lowercased().contains("command runner"), "\(body)")
        #expect(body.contains("`code` field"), "\(body)")
        // The superseded script model must be gone, not merely supplemented.
        #expect(!body.contains("IS the tool"), "\(body)")
    }

    @Test func rootReflectCarriesReadmesFrame() async {
        let kernel = makeLoaderKernel()
        // `detail:'full'` carries the doc-tree FRAME (readmes + docs); the distilled
        // DEFAULT omits it — the always-injected AI/FM prompt stays in budget.
        // Mirrors python `test_readme_doctree.py` (full carries, default omits).
        let r = await kernel.send([Item("kernel_state", ["type": "reflect", "detail": "full"])])[0].result
        let readmes = r["readmes"].asArray
        #expect(readmes != nil && !(readmes!.isEmpty))
        #expect(readmes?.contains { $0["key"].asString == "self-extension" } == true)
        for e in readmes ?? [] {
            #expect(e["desc"].asString?.isEmpty == false)
            #expect((e["tokens"].asInt ?? 0) > 0)
            #expect(e["body"] == nil || e["body"].isNull)  // index carries no bodies
        }
        #expect(r["docs"].asString?.contains("keyed doc tree") == true)

        // The distilled default drops the inlined frame (readmes + docs).
        let d = await kernel.send([Item("kernel_state", ["type": "reflect"])])[0].result
        #expect(d["readmes"] == nil || d["readmes"].isNull)
        #expect(d["docs"] == nil || d["docs"].isNull)
    }

    @Test func readmeIndexKeyedViewAndErrors() async {
        let kernel = makeLoaderKernel()
        // readme=index → the token-annotated topic tree.
        let idx = await kernel.send([Item("kernel_state", ["type": "reflect", "readme": "index"])])[0].result
        #expect(
            idx["readme"]["index"].asArray?.contains { $0["key"].asString == "self-extension" }
                == true)
        // readme=<key> → the topic body + tokens.
        let doc = await kernel.send([Item("kernel_state", ["type": "reflect", "readme": "self-extension"])])[0].result
        #expect(doc["readme"]["key"].asString == "self-extension")
        #expect(doc["readme"]["body"].asString?.lowercased().contains("author") == true)
        #expect((doc["readme"]["tokens"].asInt ?? 0) > 0)
        // unknown key → {error, keys}.
        let miss = await kernel.send([Item("kernel_state", ["type": "reflect", "readme": "nope"])])[0].result
        #expect(miss["readme"]["error"].asString != nil)
        #expect(
            miss["readme"]["keys"].asArray?.contains { $0.asString == "self-extension" } == true)
        // a `..` escape is refused.
        let esc = await kernel.send([Item("kernel_state", ["type": "reflect", "readme": "../root_readme"])])[0].result
        #expect(esc["readme"]["error"].asString != nil)
    }

    /// The manual is python's `kernel_state/docs/` tree, byte for byte, nested
    /// dirs as tiers: the index lists `frontend` with `children`, a nested key
    /// opens by `frontend/routing`, and the error texts are python's `{sel!r}`.
    @Test func readmeTreeIsNestedLikePythons() async {
        let kernel = makeLoaderKernel()
        let idx = await kernel.send([Item("kernel_state", ["type": "reflect", "readme": "index"])])[0].result
        let entries = idx["readme"]["index"].asArray ?? []
        // Python walks `sorted(d.iterdir())`: alphabetical, every `*.md`.
        #expect(
            entries.map { $0["key"].asString ?? "" } == [
                "agents", "daemon", "frontend", "memory", "orchestration", "persistence", "reflect",
                "self-extension", "streams", "system-verbs", "terminal", "transports",
            ], "\(entries.map { $0["key"] })")
        let frontend = entries.first { $0["key"].asString == "frontend" }
        #expect(
            frontend?["children"].asArray?.map { $0["key"].asString ?? "" }
                == ["frontend/html-agent", "frontend/routing"], "\(String(describing: frontend))")
        for e in entries where e["key"].asString != "frontend" {
            #expect(e["children"].isNull, "only a sibling dir adds children: \(e)")
        }
        // A nested key opens; the parent view carries its children's index.
        let routing = await kernel.send([Item("kernel_state", ["type": "reflect", "readme": "frontend/routing"])])[0].result
        #expect(routing["readme"]["key"].asString == "frontend/routing")
        #expect(routing["readme"]["body"].asString?.isEmpty == false)
        #expect(routing["readme"]["children"].isNull)
        let parent = await kernel.send([Item("kernel_state", ["type": "reflect", "readme": "frontend"])])[0].result
        #expect(parent["readme"]["children"].asArray?.count == 2)
        // Python's texts: `no doc {sel!r}` inside the tree, `bad key {sel!r}` escaping it;
        // `keys` is the flat pre-order list either way.
        let miss = await kernel.send([Item("kernel_state", ["type": "reflect", "readme": "nope"])])[0].result
        #expect(miss["readme"]["error"].asString == "no doc 'nope'")
        #expect(miss["readme"]["keys"].asArray?.map { $0.asString ?? "" }.contains("frontend/routing") == true)
        let esc = await kernel.send([Item("kernel_state", ["type": "reflect", "readme": "../root_readme"])])[0].result
        #expect(esc["readme"]["error"].asString == "bad key '../root_readme'")
        // A `..` segment is a bad key even when it lands inside the tree —
        // python's lexical check, never path resolution.
        let inside = await kernel.send([Item("kernel_state", ["type": "reflect", "readme": "frontend/../reflect"])])[0].result
        #expect(inside["readme"]["error"].asString == "bad key 'frontend/../reflect'")
        // The bundled tree equals python's docs dir.
        let py = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
            .deletingLastPathComponent()
            .appendingPathComponent("python/bundled_agents/loader/kernel_state/src/kernel_state/docs")
        for (key, body) in RootDocs.all {
            let want = try? String(contentsOf: py.appendingPathComponent("\(key).md"), encoding: .utf8)
            #expect(want == body, "docs/\(key).md differs from python's")
        }
        #expect(RootDocs.all.count == 14)
    }

    @Test func nonRootHasNoReadmesFrame() async {
        let kernel = makeKernel()
        _ = await kernel.send([Item("kernel_state", ["type": "create_agent", "parent_id": "kernel_state", "handler_module": "echo.tools", "id": "kid"])])[0].result
        let kid = await kernel.send([Item("kid", ["type": "reflect"])])[0].result
        #expect(kid["readmes"] == nil || kid["readmes"].isNull)
        #expect(kid["docs"] == nil || kid["docs"].isNull)
    }

    // ─── self_extension: the authoring contract ────────────────
    //
    // A durable program is a FILE plus a COMMAND — there is no `code` field
    // (a program embedded as a JSON string is exactly how a call gets lost to
    // escaping). Write it with the file leg's STREAM verbs, the document
    // verbatim in `<bytes>`, and run it on the exec_bridge COMMAND runner.

    @Test func selfExtensionStatesTheFileCommandModel() async throws {
        let kernel = makeLoaderKernel()
        let s = try #require(
            await kernel.send([Item("kernel_state", ["type": "reflect"])])[0].result["self_extension"].asString)
        #expect(s.contains("A DURABLE PROGRAM is a FILE"), "\(s)")
        #expect(s.contains("no `code` field") || s.contains("There is no `code` field"), "\(s)")
        #expect(s.contains("write_stream"), "\(s)")
        #expect(s.contains("`bytes` verbatim"), "\(s)")
        #expect(!s.contains("send_binary"), "\(s)")
        #expect(!s.contains("Begin Bytes"), "\(s)")
        #expect(s.contains("exec_bridge.tools"), "\(s)")
        #expect(s.contains("can_call") && s.contains("callable_by"), "\(s)")
        // The superseded script phrasing must be gone.
        #expect(!s.contains("IS the tool"), "\(s)")
        #expect(!s.contains("code:'<your program"), "\(s)")
    }

    /// The tree, `tree=ids` and every descendant walk read python's
    /// INSERTION order (`self._children.values()`), not an alphabetical sort.
    @Test func treeChildrenKeepCreationOrder() async {
        let kernel = makeKernel()
        for id in ["zeta", "alpha", "mid"] {
            _ = await kernel.send([
                Item(
                    "kernel_state",
                    [
                        "type": "create_agent", "parent_id": "kernel_state",
                        "handler_module": "echo.tools", "id": .string(id),
                    ])
            ])[0].result
        }
        // The distilled ROSTER walks creation order...
        let distilled = await kernel.send([Item("kernel_state", ["type": "reflect"])])[0].result
        #expect(
            distilled["tree"]["children"].asArray?.compactMap { $0["id"].asString }
                == ["zeta", "alpha", "mid"])
        // ...and so does the `detail:full` wiring tree...
        let full = await kernel.send([
            Item("kernel_state", ["type": "reflect", "detail": "full"])
        ])[0].result
        #expect(
            full["tree"]["children"].asArray?.compactMap { $0["id"].asString }
                == ["zeta", "alpha", "mid"])
        // ...and the flat descendant index (DFS, self first).
        let ids = await kernel.send([Item("kernel_state", ["type": "reflect", "tree": "ids"])])[0].result
        #expect(
            ids["tree"].asArray?.compactMap { $0.asString }
                == ["kernel_state", "zeta", "alpha", "mid"])
    }

    // ─── the `tree` flag is decided by KEY PRESENCE ───────
    //
    // Python reads `payload.get("tree", "all")`: ABSENT is `all`; a present
    // `"all"`/`"ids"` acts; ANY other present value omits the tree — reading
    // `asString ?? "all"` would insert the WHOLE tree for `tree:null` / `tree:true` /
    // `tree:5`, the one answer the caller did not ask for.
    @Test func nonStringTreeFlagOmitsTheTree() async {
        let kernel = makeKernel()
        // Absent ⇒ all.
        let absent = await kernel.send([Item("kernel_state", ["type": "reflect"])])[0].result
        #expect(absent["tree"]["id"].asString == "kernel_state")
        // Present but not "all"/"ids" ⇒ no tree at all.
        let others: [JSON] = [
            .null, .bool(true), .bool(false), .integer(5), .double(1.5), .string("none"),
            .string("ALL"), .array([.string("all")]), .object(["t": .string("all")]),
        ]
        for flag in others {
            let r = await kernel.send([
                Item("kernel_state", .object(["type": .string("reflect"), "tree": flag]))
            ])[0].result
            #expect(r.asObject?["tree"] == nil, "tree:\(flag.serialize()) must omit the tree")
            #expect(r.asObject?["next"] == nil)
        }
        // "ids" still acts.
        let ids = await kernel.send([Item("kernel_state", ["type": "reflect", "tree": "ids"])])[0].result
        #expect(ids["tree"].asArray?.count == 1)
    }

    // ─── no verb docs on ANY reflect (they are `docs`) ───────
    @Test func reflectCarriesNoVerbDocs() async {
        let kernel = makeKernel()
        _ = await kernel.send([
            Item(
                "kernel_state",
                [
                    "type": "create_agent", "parent_id": "kernel_state",
                    "handler_module": "echo.tools", "id": "kid",
                ])
        ])[0].result
        let flagSets: [JSON] = [
            ["type": "reflect"],
            ["type": "reflect", "tree": "none"],
            ["type": "reflect", "tree": "ids"],
            ["type": "reflect", "bundles": "ids"],
            ["type": "reflect", "readme": true],
        ]
        for target in ["kernel_state", "kid"] {
            for flags in flagSets {
                let r = await kernel.send([Item(AgentId(target), flags)])[0].result
                // A reflect carries no verb docs: no `more`,
                // `kernel_verbs`, `verbs`, `emits` or `cancel` — those are `docs`.
                for key in ["more", "kernel_verbs", "verbs", "emits", "cancel"] {
                    #expect(r.asObject?[key] == nil, "\(target) \(flags.serialize()) \(key)")
                }
            }
            let full = await kernel.send([
                Item(AgentId(target), ["type": "reflect", "detail": "full"])
            ])[0].result
            #expect(full.asObject?["more"] == nil)
        }
    }

    // ─── reflect paging keys ───────────────────────────────
    //
    // `tree=ids` names its cursor `next` (never `tree_next`) and only when a
    // further page follows; `bundles=all|ids` pages by the SAME `limit`/`after`
    // and names `bundles_next` only when non-null.
    @Test func bundlesPageLikePython() async {
        let registry = BundleRegistry()
        for n in ["a", "b", "c", "d", "e"] { registry.register("\(n).tools", NamedProbe(name: n)) }
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: "kernel_state", handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)

        // Unpaged: everything, no cursor key.
        let all = await kernel.send([Item("kernel_state", ["type": "reflect", "bundles": "ids"])])[0].result
        #expect(all["bundles"].asArray?.count == 5)
        #expect(all.asObject?["bundles_next"] == nil, "no further page ⇒ no cursor key")

        // Paged, and the walk reassembles the catalog exactly once.
        var seen: [String] = []
        var cursor: String? = nil
        while true {
            var p: OrderedDictionary<String, JSON> = [
                "type": .string("reflect"), "bundles": .string("all"), "limit": .integer(2),
            ]
            if let c = cursor { p["after"] = .string(c) }
            let r = await kernel.send([Item("kernel_state", .object(p))])[0].result
            seen += r["bundles"].asArray?.compactMap { $0["name"].asString } ?? []
            guard let n = r["bundles_next"].asString else { break }
            cursor = n
        }
        #expect(seen == ["a", "b", "c", "d", "e"])

        // ONE `limit`/`after` pair serves `tree=ids` and `bundles` together.
        let both = await kernel.send([
            Item("kernel_state", ["type": "reflect", "tree": "ids", "bundles": "ids", "limit": 2])
        ])[0].result
        #expect(both["tree"].asArray?.count == 1)
        #expect(both.asObject?["next"] == nil, "one root id is not a further page")
        #expect(both["bundles"].asArray?.count == 2)
        #expect(both["bundles_next"].asString == "b")
    }

    // ─── the catalog lists INSTALLABLE bundles only ───────
    //
    // Python's catalog is the `fantastic.bundles` ENTRY-POINT scan, so an
    // importable module without one (the e2e scenario probes) dispatches and is
    // invisible here.
    @Test func anUnlistedBundleIsNotInTheCatalog() async {
        let registry = BundleRegistry()
        registry.register("installed.tools", NamedProbe(name: "installed"))
        registry.registerUnlisted("fixture_probe.tools", NamedProbe(name: "fixture_probe"))
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: "kernel_state", handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)

        let r = await kernel.send([Item("kernel_state", ["type": "reflect", "bundles": "ids"])])[0].result
        #expect(r["bundles"].asArray?.compactMap { $0.asString } == ["installed"])

        // It still DISPATCHES — unlisted is about the catalog, not routing.
        _ = await kernel.send([
            Item(
                "kernel_state",
                [
                    "type": "create_agent", "parent_id": "kernel_state",
                    "handler_module": "fixture_probe.tools", "id": "fx",
                ])
        ])[0].result
        let fx = await kernel.send([Item("fx", ["type": "reflect"])])[0].result
        #expect(fx["sentence"].asString == "fixture_probe speaking.")
    }

    // ─── an EMPTY `display_name` is no name ───────────────
    @Test func emptyDisplayNameFallsBackToId() async {
        let kernel = makeKernel()
        _ = await kernel.send([
            Item(
                "kernel_state",
                [
                    "type": "create_agent", "parent_id": "kernel_state",
                    "handler_module": "echo.tools", "id": "blank", "display_name": "",
                ])
        ])[0].result
        // The bare-agent identity (the shape a node without a handler answers).
        let ident = kernel.reflectIdentity(kernel.agent("blank")!)
        #expect(ident["display_name"].asString == "blank")
        // ...and the `detail:full` wiring node.
        let full = await kernel.send([
            Item("kernel_state", ["type": "reflect", "detail": "full"])
        ])[0].result
        let node = full["tree"]["children"].asArray?.first { $0["id"].asString == "blank" }
        #expect(node?["display_name"].asString == "blank")
    }
}

/// A bundle that answers `reflect` with its own name in the sentence — the
/// fixture for the catalog and paging cases.
struct NamedProbe: AgentBundle {
    let name: String
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        payload["type"].asString == "reflect"
            ? .object([
                "id": .string(agentId.value), "sentence": .string("\(name) speaking."),
            ])
            : .object(["ok": .bool(true)])
    }
}


@Suite("Root identity with a handler_module")
struct RootWithHandlerTests {
    /// The tree root answers `reflect` with the SUBSTRATE identity even
    /// though it now carries `handler_module: kernel_state.tools`. Its other
    /// verbs still route to the bundle; `reflect <root>` is the kernel's own
    /// identity + tree, matching the reserved `kernel` address (python `_agent.py`:
    /// `if verb == "reflect" and self.parent is None`).
    @Test func rootWithHandlerReflectsIdentity() async {
        let kernel = makeLoaderKernel()
        let r = await kernel.send([Item("kernel_state", ["type": "reflect"])])[0].result
        #expect(r["id"].asString == "kernel_state")
        #expect(r["handler_module"].asString == "kernel_state.tools")
        // The substrate sentence, NOT the loader's own `Disk persistence…` one.
        #expect(r["sentence"].asString?.hasPrefix("Fantastic kernel") == true, "\(r)")
        #expect(r["tree"]["id"].asString == "kernel_state")
        // The loader's identity keys never leak onto the root reflect.
        #expect(r["root"] == .null, "\(r)")
        #expect(r["verbs"] == .null, "\(r)")
        // But the loader's ROOT FRAME does — through `reflectRootExtra`.
        #expect(r["persistence"]["provider"] == .null)
        #expect(r["self_extension"].asString?.hasPrefix("A DURABLE PROGRAM is a FILE plus") == true)
        // …and the `kernel` address answers identically.
        let viaAlias = await kernel.send([Item("kernel", ["type": "reflect"])])[0].result
        #expect(viaAlias["id"].asString == "kernel_state")
    }

    /// The loader's OWN reflect is reachable on a non-root loader (a session
    /// loader serving a sub-namespace), where the root special case does not
    /// apply.
    @Test func aNonRootLoaderReflectsTheLoaderIdentity() async {
        let kernel = makeLoaderKernel()
        _ = await kernel.send([
            Item(
                "kernel",
                [
                    "type": "create_agent", "parent_id": "kernel_state",
                    "handler_module": "kernel_state.tools", "id": "session",
                    "watch": false,
                ])
        ])[0].result
        let r = await kernel.send([Item("session", ["type": "reflect"])])[0].result
        #expect(r["sentence"].asString?.hasPrefix("Disk persistence/hydration root") == true, "\(r)")
        #expect(r.asObject?["verbs"] == nil, "no verb docs on a reflect: \(r)")
        #expect(r["persistence"] == .null, "the root frame is the ROOT's alone: \(r)")
    }

    /// A verb the loader does not answer is python's CODED refusal, not a bare
    /// `{error}` a reflect decorator would dress up as a healthy reply.
    @Test func unknownTypeRefusalIsVerbatim() async {
        let kernel = makeLoaderKernel()
        _ = await kernel.send([
            Item(
                "kernel",
                [
                    "type": "create_agent", "parent_id": "kernel_state",
                    "handler_module": "kernel_state.tools", "id": "session",
                    "watch": false,
                ])
        ])[0].result
        let r = await kernel.send([Item("session", ["type": "nope"])])[0].result
        #expect(r["error"].asString == "kernel_state: unknown type 'nope'", "\(r)")
        #expect(r["reason"].asString == "unhandled")
        #expect(r["verb"].asString == "nope")
    }
}

/// A bundle that ships a readme and a flat keyed docs table — the only place a
/// manual lives (never copied to disk, never read from it).
private struct DocsBundle: AgentBundle {
    let name = "docs_probe"
    let readme: String?
    let docs: [(key: String, body: String)]

    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? { nil }
}

/// Doc-tree reads served from the agent's BUNDLE — python's
/// `_readme_index`/`_readme_view` over the package `docs/`.
@Suite("Readme doc tree — served from the bundle")
struct ReadmeBundleDocsTests {
    /// A DISK kernel whose root runs a [`DocsBundle`]; its dir holds a stray
    /// `readme.md` and `docs/` that must never be read.
    private func makeDocsKernel(
        readme: String? = nil, _ docs: [(key: String, body: String)]
    ) -> (kernel: Kernel, root: URL) {
        let storeRoot = URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("readme_bundle_\(UUID().uuidString.prefix(8))")
        let fm = FileManager.default
        try? fm.createDirectory(
            at: storeRoot.appendingPathComponent("docs"), withIntermediateDirectories: true)
        try? "> stray\nDISK".write(
            to: storeRoot.appendingPathComponent("docs/stray.md"), atomically: true, encoding: .utf8)
        try? "DISK README".write(
            to: storeRoot.appendingPathComponent("readme.md"), atomically: true, encoding: .utf8)
        let registry = BundleRegistry()
        registry.register("docs_probe.tools", DocsBundle(readme: readme, docs: docs))
        let kernel = Kernel(storage: .disk(storeRoot), bundles: registry)
        let root = Agent(
            id: AgentId("kernel_state"), handlerModule: "docs_probe.tools", parentId: nil,
            rootPath: storeRoot)
        kernel.register(root)
        kernel.setRoot(root)
        return (kernel, storeRoot)
    }

    private func reflect(_ k: Kernel, _ readme: JSON) async -> JSON {
        await k.send([Item("kernel_state", .object(["type": "reflect", "readme": readme]))])[0]
            .result["readme"]
    }

    /// The bundle is the one source: the stray files on disk are never read.
    @Test func nothingIsReadFromDisk() async throws {
        let (kernel, root) = makeDocsKernel(readme: "BUNDLE README", [("a", "> a\nbody")])
        defer { try? FileManager.default.removeItem(at: root) }
        #expect(await reflect(kernel, true).asString == "BUNDLE README")
        let idx = await reflect(kernel, "index")
        #expect(idx["index"].asArray?.map { $0["key"].asString ?? "" } == ["a"], "\(idx)")
        #expect(await reflect(kernel, "stray")["error"].asString == "no doc 'stray'")
    }

    /// A bundle with no readme and no docs answers `null` and `[]`.
    @Test func aBundleWithoutDocsAnswersNullAndEmpty() async throws {
        let (kernel, root) = makeDocsKernel([])
        defer { try? FileManager.default.removeItem(at: root) }
        #expect(await reflect(kernel, true).isNull)
        #expect(await reflect(kernel, "index")["index"].asArray == [])
    }

    /// `\r\n`/lone `\r` translate to `\n` before `body`/`desc`/`tokens`
    /// are computed from it — python text mode.
    @Test func docReadsTranslateNewlinesLikePythonTextMode() async throws {
        let (kernel, root) = makeDocsKernel([("topic", "> desc\r\nline2\rline3\n")])
        defer { try? FileManager.default.removeItem(at: root) }
        let r = await reflect(kernel, "topic")
        let body = r["body"].asString ?? ""
        #expect(body == "> desc\nline2\nline3\n", "\(r)")
        #expect(r["desc"].asString == "desc")
        #expect(r["tokens"].asInt == Int64((body.unicodeScalars.count + 3) / 4))
    }

    /// Tiers are ordered by `<stem>.md` like python's sorted dir walk, and a
    /// tier whose parent topic is absent is not listed (python lists only
    /// `<key>.md` files, so a bare subdir never appears).
    @Test func tiersNestUnderTheirParentTopic() async throws {
        let (kernel, root) = makeDocsKernel([
            ("frontend/routing", "> routing doc\nbody"),
            ("frontend", "> frontend doc\nbody"),
            ("frontend/html-agent", "> panel doc\nbody"),
            ("orphan/child", "> orphan\nbody"),
        ])
        defer { try? FileManager.default.removeItem(at: root) }
        let idx = await reflect(kernel, "index")["index"]
        #expect(idx.asArray?.map { $0["key"].asString ?? "" } == ["frontend"], "\(idx)")
        #expect(
            idx[0]["children"].asArray?.map { $0["key"].asString ?? "" }
                == ["frontend/html-agent", "frontend/routing"])
    }

    /// Python: the key check is LEXICAL and then the INDEX — a `..`,
    /// `.` or empty segment is a `bad key`, a listed key is served, an unlisted
    /// one is `no doc`.
    @Test func keyCheckIsLexicalThenTheIndex() async throws {
        let (kernel, root) = makeDocsKernel([
            ("frontend", "> frontend doc\nbody"),
            ("frontend/routing", "> routing doc\nbody"),
            ("frontend/html-agent", "> panel doc\nbody"),
        ])
        defer { try? FileManager.default.removeItem(at: root) }
        for key in [
            "frontend/../frontend/routing", "frontend//routing", "./frontend/routing",
            "../escape", "frontend/html-agent/../routing", "/",
        ] {
            #expect(
                await reflect(kernel, .string(key))["error"].asString == "bad key \(pyRepr(key))",
                "\(key)")
        }
        let listed = await reflect(kernel, "/frontend/routing/")
        #expect(listed["key"].asString == "frontend/routing", "\(listed)")
        #expect(listed["body"].asString?.contains("routing doc") == true, "\(listed)")
        #expect(Array(listed.asObject?.keys ?? []) == ["key", "desc", "tokens", "body"])
        #expect(await reflect(kernel, "frontend/nothing")["error"].asString == "no doc 'frontend/nothing'")
    }
}
