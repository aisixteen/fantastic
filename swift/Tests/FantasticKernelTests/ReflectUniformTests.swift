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
}
