// Unit tests for the uniform `reflect` surface — tree/bundles/readme
// flags, the `description` field, and the guarantee that the old
// root-only primer keys are gone. Mirrors Python's test_reflect_flags
// and Rust's reflect/tests.rs.

import FantasticJSON
import Foundation
import Testing

@testable import FantasticKernel

@Suite("Uniform reflect")
struct ReflectUniformTests {
    private static let primerKeysGone = [
        "transports", "primitive", "envelope", "universal_verb",
        "binary_protocol", "browser_bus", "well_known", "agent_count",
        "available_bundles",
    ]

    @Test func rootReflectIsUniformNoPrimerKeys() async {
        let kernel = makeKernel()
        let r = await kernel.send("core", ["type": "reflect"])
        #expect(r["id"].asString == "core")
        #expect(r["sentence"].asString?.hasPrefix("Fantastic kernel") == true)
        #expect(r["parent_id"].isNull)
        // tree default = all.
        #expect(r["tree"]["id"].asString == "core")
        // bundles omitted by default.
        #expect(r["bundles"].asArray == nil)
        // kernel runtime identity — root only, lowercase enum.
        #expect(r["runtime"].asString == "swift")
        for k in Self.primerKeysGone {
            #expect(r[k] == nil || r[k] == .null, "primer key \(k) still present")
        }
    }

    @Test func rootReflectSurfacesGuidFromMeta() async {
        let kernel = makeKernel()
        // Bare root has no guid in meta → reflect surfaces an explicit null
        // (persistence-scoped field, always present on the root reflect).
        let bare = await kernel.send("core", ["type": "reflect"])
        #expect(bare["guid"].isNull)
        // Seed a 32-hex guid into root meta → reflect surfaces it verbatim.
        kernel.root?.updateMeta(["guid": .string("0123456789abcdef0123456789abcdef")])
        let r = await kernel.send("core", ["type": "reflect"])
        #expect(r["guid"].asString == "0123456789abcdef0123456789abcdef")
        // Non-root reflects never carry guid.
        _ = await kernel.send(
            "core", ["type": "create_agent", "handler_module": "echo.tools", "id": "kid"])
        let kid = await kernel.send("kid", ["type": "reflect"])
        #expect(kid["guid"] == nil || kid["guid"].isNull)
    }

    @Test func treeTiers() async {
        let kernel = makeKernel()
        _ = await kernel.send(
            "core",
            ["type": "create_agent", "handler_module": "echo.tools", "id": "kid"])
        // tree=ids → flat list, root first.
        let ids = await kernel.send(
            "core", ["type": "reflect", "tree": "ids"])
        #expect(ids["tree"].asArray?.compactMap { $0.asString } == ["core", "kid"])
        // tree=none → omitted.
        let none = await kernel.send(
            "core", ["type": "reflect", "tree": "none"])
        #expect(none["tree"] == nil || none["tree"].isNull)
    }

    @Test func bundlesTiers() async {
        let kernel = makeKernel()
        let all = await kernel.send(
            "core", ["type": "reflect", "bundles": "all"])
        let arr = all["bundles"].asArray
        #expect(arr != nil && !(arr!.isEmpty))
        #expect(arr?.first?["name"].asString != nil)
        #expect(arr?.first?["handler_module"].asString != nil)
        let ids = await kernel.send(
            "core", ["type": "reflect", "bundles": "ids"])
        #expect(ids["bundles"].asArray?.allSatisfy { $0.asString != nil } == true)
    }

    @Test func descriptionSurfacesTopAndTree() async {
        let kernel = makeKernel()
        _ = await kernel.send(
            "core",
            [
                "type": "create_agent", "handler_module": "echo.tools",
                "id": "kid", "description": "holds my notes",
            ])
        // top-level on the child's own reflect.
        let own = await kernel.send("kid", ["type": "reflect"])
        #expect(own["description"].asString == "holds my notes")
        // and in the parent's tree=all node.
        let root = await kernel.send("core", ["type": "reflect"])
        let node = root["tree"]["children"].asArray?.first { $0["id"].asString == "kid" }
        #expect(node?["description"].asString == "holds my notes")
    }

    @Test func readmeFlagTiersAndFallback() async {
        let kernel = makeKernel()
        // default → no readme key.
        let plain = await kernel.send("core", ["type": "reflect"])
        #expect(plain["readme"] == nil || plain["readme"].isNull)
        // readme=true on the root in-memory → embedded canonical readme.
        let r = await kernel.send("core", ["type": "reflect", "readme": true])
        #expect(r["readme"].asString == RootReadme.text)
        #expect(r["readme"].asString?.hasPrefix("# This is a Fantastic kernel.") == true)
        // readme honored.
        let legacy = await kernel.send(
            "core", ["type": "reflect", "readme": true])
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
        #expect(RootDocs.all.count == RootDocs.keys.count)
        #expect(RootDocs.all.contains { $0.key == "self-extension" })
        for (key, body) in RootDocs.all {
            #expect(!body.isEmpty, "empty doc \(key)")
            #expect(body.hasPrefix("> "), "doc \(key) must open with a `> ` descriptor")
        }
    }

    /// The authoring doc must teach the SAME execution model as the distilled
    /// `self_extension`, and BOTH receive shapes — js/lua await `on_message()` in a
    /// registers a CALLBACK — the ONE receive shape now that exec_bridge is the
    /// one executor (the in-proc loop engines are deleted).
    @Test func selfExtensionDocStatesScriptModelAndTheReceiveShape() throws {
        let body = try #require(RootDocs.all.first { $0.key == "self-extension" }?.body)
        #expect(body.contains("SCRIPT"), "\(body)")
        #expect(body.contains("top to bottom"), "\(body)")
        #expect(body.contains("module level"), "\(body)")
        #expect(body.contains("kernel.on_message(callback)"), "\(body)")
        #expect(!body.contains("IS the tool"), "\(body)")
    }

    @Test func rootReflectCarriesReadmesFrame() async {
        let kernel = makeKernel()
        let r = await kernel.send("core", ["type": "reflect"])
        let readmes = r["readmes"].asArray
        #expect(readmes != nil && !(readmes!.isEmpty))
        #expect(readmes?.contains { $0["key"].asString == "self-extension" } == true)
        for e in readmes ?? [] {
            #expect(e["desc"].asString?.isEmpty == false)
            #expect((e["tokens"].asInt ?? 0) > 0)
            #expect(e["body"] == nil || e["body"].isNull)  // index carries no bodies
        }
        #expect(r["docs"].asString?.contains("keyed doc tree") == true)
    }

    @Test func readmeIndexKeyedViewAndErrors() async {
        let kernel = makeKernel()
        // readme=index → the token-annotated topic tree.
        let idx = await kernel.send("core", ["type": "reflect", "readme": "index"])
        #expect(
            idx["readme"]["index"].asArray?.contains { $0["key"].asString == "self-extension" }
                == true)
        // readme=<key> → the topic body + tokens.
        let doc = await kernel.send("core", ["type": "reflect", "readme": "self-extension"])
        #expect(doc["readme"]["key"].asString == "self-extension")
        #expect(doc["readme"]["body"].asString?.lowercased().contains("author") == true)
        #expect((doc["readme"]["tokens"].asInt ?? 0) > 0)
        // unknown key → {error, keys}.
        let miss = await kernel.send("core", ["type": "reflect", "readme": "nope"])
        #expect(miss["readme"]["error"].asString != nil)
        #expect(
            miss["readme"]["keys"].asArray?.contains { $0.asString == "self-extension" } == true)
        // a `..` escape is refused.
        let esc = await kernel.send("core", ["type": "reflect", "readme": "../root_readme"])
        #expect(esc["readme"]["error"].asString != nil)
    }

    @Test func nonRootHasNoReadmesFrame() async {
        let kernel = makeKernel()
        _ = await kernel.send(
            "core", ["type": "create_agent", "handler_module": "echo.tools", "id": "kid"])
        let kid = await kernel.send("kid", ["type": "reflect"])
        #expect(kid["readmes"] == nil || kid["readmes"].isNull)
        #expect(kid["docs"] == nil || kid["docs"].isNull)
    }

    // ─── self_extension: the authoring contract ────────────────
    //
    // Measured twice on gemma-4-12B (2026-08-06): with the old "the `code` IS the
    // tool" the model read "the code DEFINES the tool", authored
    // `def run(payload): ...` and sent {type:'run'} expecting a call. Two agents
    // shipped inert, nothing was written, {queued:true} never appeared because no
    // job stayed alive. Both facts below cost a whole run when wrong, and neither
    // was pinned by any swift test.

    @Test func selfExtensionStatesTheExecutionModel() async throws {
        let kernel = makeKernel()
        let s = try #require(
            await kernel.send("core", ["type": "reflect"])["self_extension"].asString)
        #expect(s.contains("SCRIPT"), "\(s)")
        #expect(s.contains("top to bottom"), "\(s)")
        // The specific misread that burned two runs: a bare `def` is inert.
        #expect(s.contains("`def`"), "\(s)")
        #expect(s.contains("module level"), "\(s)")
        #expect(s.contains("kernel.on_message(cb)"), "\(s)")
        // The superseded phrasing must be gone, not merely supplemented.
        #expect(!s.contains("IS the tool"), "\(s)")
        // Only a LIVE script acks — the model waited for {queued:true} from a
        // script that had already exited.
        #expect(s.contains("a LIVE script acks {queued:true}"), "\(s)")
    }

    /// The advertised executor list must be DERIVED, never hard-coded: a
    /// hard-coded list drifted from what a build actually registered, so a
    /// conditionally-compiled executor went unnamed while the string named a
    /// module that is not registered at all. Both directions are asserted here
    /// against `availableBundles()`.
    @Test func selfExtensionNamesOnlyRegisteredExecutors() async throws {
        for (module, shape) in Kernel.scriptExecutors {
            let registry = BundleRegistry()
            registry.register("echo.tools", EchoBundle())
            registry.register(module, EchoBundle())
            let kernel = Kernel(storage: .inMemory, bundles: registry)
            let root = Agent(id: "core", handlerModule: nil, parentId: nil)
            kernel.register(root)
            kernel.setRoot(root)

            let s = try #require(
                await kernel.send("core", ["type": "reflect"])["self_extension"].asString)
            #expect(s.contains(module), "registered \(module) but self_extension omits it: \(s)")
            #expect(s.contains(shape), "\(module) named without its receive shape: \(s)")
            // A registered echo bundle is no script executor.
            #expect(!s.contains("echo.tools"), "\(s)")
            for other in Kernel.scriptExecutors.map(\.handlerModule) where other != module {
                #expect(!s.contains(other), "phantom executor \(other) named: \(s)")
            }
        }
    }

    @Test func selfExtensionSaysSoWhenNoExecutorIsRegistered() async throws {
        // makeKernel registers echo only — no handler_module here runs authored
        // `code`, and a caller must learn that from the same round-trip rather
        // than being pointed at a module that cannot be created.
        let kernel = makeKernel()
        let s = try #require(
            await kernel.send("core", ["type": "reflect"])["self_extension"].asString)
        #expect(s.contains("NO script executor"), "\(s)")
        for module in Kernel.scriptExecutors.map(\.handlerModule) {
            #expect(!s.contains(module), "unregistered \(module) named: \(s)")
        }
    }
}
