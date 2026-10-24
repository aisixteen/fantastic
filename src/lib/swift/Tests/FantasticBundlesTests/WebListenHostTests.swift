// Twins of python `web/host/tests/test_listen_host.py`: the listener binds the
// record's `bind`, else its `host`, else loopback; a wildcard `bind` leaves the
// Host/Origin guard judging by `host` (`WebOriginGuardTests`).

import FantasticJSON
import Foundation
import Testing

@testable import FantasticWeb

@Suite("WebListenHost")
struct WebListenHostTests {
    @Test func theListenerBindsTheBindFieldElseTheHostElseLoopback() {
        #expect(webListenHost(.object([:])) == "127.0.0.1")
        #expect(webListenHost(.object(["host": .string("192.168.1.5")])) == "192.168.1.5")
        #expect(webListenHost(.object(["bind": .string("0.0.0.0")])) == "0.0.0.0")
        // a bind beats a host: the publish is the listener, the host stays the guard's identity
        let both: JSON = .object(["bind": .string("0.0.0.0"), "host": .string("192.168.1.5")])
        #expect(webListenHost(both) == "0.0.0.0")
        #expect(
            webOwnAuthorities(.object(["bind": .string("0.0.0.0"), "host": .string("192.168.1.5"), "port": .integer(8088)]))
                == ["192.168.1.5:8088"])
        // an empty bind is no bind, as python's `or` reads it
        #expect(webListenHost(.object(["bind": .string(""), "host": .string("0.0.0.0")])) == "0.0.0.0")
    }
}
