// The seam between an HTTP host and the leg that declared a route.
//
// Python's `get_routes` hands the host a CALLABLE (`endpoint`), so a request
// served on a leg's route is NOT an agent send on that leg: the leg's own
// function runs, calls `kernel.send` under `sender_context(self_id)`, and the
// request holds exactly ONE task row. Swift cannot put a callable in JSON, so
// the host resolves the leg's bundle and calls THIS protocol instead of
// dispatching a `handle_route` verb — which is why `handle_route` is not a verb
// any more (A11.28): it made every request a second, PARENT row on the leg.

import FantasticJSON
import Foundation
import FantasticKernel

/// A bundle that serves the HTTP routes it declared through `get_routes`.
public protocol WebRouteOwner {
    /// Serve one matched request.
    ///
    /// `request` carries `{method, path, params, query, headers, body}` (headers
    /// lowercased, latin-1 decoded as python's ASGI layer hands them over). The
    /// reply is `({status, content_type?, headers?, body?}, chunk?)` — a missing
    /// `content_type` means the response carries no body at all (204).
    ///
    /// The second half is THE BINARY REPLY BODY (C37/A13.1): `nil` = the verb
    /// returned no chunk, `.some(d)` = it returned one, `d` possibly EMPTY (an
    /// empty file read). The host turns that — never the body's length — into
    /// `application/octet-stream`, because a `{status, content_type, body:String}`
    /// JSON reply cannot carry raw bytes or response headers.
    func serveRoute(legId: AgentId, request: JSON, kernel: Kernel) async -> (JSON, Data?)
}
