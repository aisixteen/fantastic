// `reflect readme=<key>` on an IN-MEMORY root — the swift-only resolver.
//
// Python has no in-memory mode; its (and swift's disk-mode) key check is
// LEXICAL: any `..`, `.` or empty segment is a `bad key` (pinned in
// `ReflectUniformTests`). The in-memory resolver instead walks the key the way
// pathlib resolves a path — `.` and empty segments vanish, `..` pops, popping
// past the root is the `bad key` — and echoes the key AS GIVEN. The two
// resolvers disagree on `frontend/../frontend/routing`; serving both from one
// check is a behaviour decision, so the current in-memory answers are pinned.

import FantasticJSON
import Foundation
import Testing

@testable import FantasticKernel

private func readme(_ k: Kernel, _ key: String) async -> JSON {
    await k.send([Item("kernel_state", .object(["type": "reflect", "readme": .string(key)]))])[0]
        .result["readme"]
}

@Suite("in-memory readme keys")
struct InMemoryReadmeKeyPinTests {

    @Test func dotDotAndEmptySegmentsResolveInMemory() async {
        let k = makeKernel(withEcho: false)
        let routing = await readme(k, "frontend/routing")
        let body = routing["body"].asString
        #expect(body?.isEmpty == false)
        for key in [
            "frontend/../frontend/routing", "frontend//routing", "./frontend/routing",
            "/frontend/routing/",
        ] {
            let r = await readme(k, key)
            #expect(r["body"].asString == body, "\(key): \(r.serialize())")
            // the key is echoed with the outer slashes trimmed, never normalised
            let trimmed = key.trimmingCharacters(in: CharacterSet(charactersIn: "/"))
            #expect(r["key"].asString == trimmed, "\(key)")
            #expect(Array(r.asObject?.keys ?? []) == ["key", "desc", "tokens", "body"], "\(key)")
        }
    }

    @Test func poppingPastTheRootIsABadKeyAndAnEmptyLookupIsNoDoc() async {
        let k = makeKernel(withEcho: false)
        let esc = await readme(k, "frontend/../../x")
        #expect(esc["error"].asString == "bad key 'frontend/../../x'")
        #expect(Array(esc.asObject?.keys ?? []) == ["error", "keys"])
        let dot = await readme(k, ".")
        #expect(dot["error"].asString == "no doc '.'")
        let back = await readme(k, "frontend/..")
        #expect(back["error"].asString == "no doc 'frontend/..'")
    }
}
