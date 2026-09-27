// The seam between an HTTP host and the leg that declared a route — python's
// `route_endpoints(agent)` module hook.
//
// A leg DECLARES its routes as data (`get_routes` → [{kind, method?, path}], a
// plain reply that renders over REST like any other) and hands the host the LIVE
// endpoint for each declared path through this hook, keyed by that path. The
// host mounts what is declared with the endpoint handed over; a leg that
// declares a route and hands over no endpoint for it fails the host's boot loud.
//
// A served request is NOT an agent send on the leg: the endpoint runs, calls
// `kernel.send` as the leg, and the request holds exactly ONE task row — which
// is why `handle_route` is not a verb (A11.28): it made every request a second,
// PARENT row on the leg.

import FantasticJSON
import FantasticKernel
import Foundation
import NIOCore

/// What the host hands a websocket endpoint when a client upgrades onto the
/// route it was handed over for.
public struct WebSocketUpgrade: Sendable {
    /// The route's captured path params (`{host_id}` → its value); empty for a
    /// literal path.
    public let params: [String: String]
    /// The host's boot pid — every task of the socket registers under it
    /// (python serves the ASGI app inside `CURRENT_TASK.scope(<boot pid>)`).
    public let bootParent: TaskId?

    public init(params: [String: String], bootParent: TaskId?) {
        self.params = params
        self.bootParent = bootParent
    }
}

/// The LIVE endpoint a leg hands its host for ONE declared path.
public enum WebEndpoint: Sendable {
    /// Serve one matched HTTP request. `request` carries `{method, path,
    /// params, query, headers, body}` (headers lowercased, latin-1 decoded as
    /// python's ASGI layer hands them over). The reply is `({status,
    /// content_type?, headers?, body?}, chunk?)` — a missing `content_type`
    /// means the response carries no body at all (204).
    ///
    /// The second half is THE BINARY REPLY BODY (C37/A13.1): `nil` = no chunk,
    /// `.some(d)` = a chunk, `d` possibly EMPTY (an empty file read). The host
    /// turns that — never the body's length — into `application/octet-stream`.
    case http(@Sendable (_ request: JSON) async -> (JSON, Data?))
    /// Serve one upgraded websocket: the handler the host installs on the
    /// upgraded channel.
    case websocket(@Sendable (_ upgrade: WebSocketUpgrade) -> any ChannelHandler & Sendable)
}

/// A bundle whose agents declare web routes. Python `route_endpoints(agent)`:
/// `{declared path: live endpoint}` for one agent of this bundle.
public protocol WebRouteEndpoints {
    func routeEndpoints(agent: Agent, kernel: Kernel) -> [String: WebEndpoint]
}
