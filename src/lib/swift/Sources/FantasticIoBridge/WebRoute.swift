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
import FantasticKernel

/// A bundle that serves the HTTP routes it declared through `get_routes`.
public protocol WebRouteOwner {
    /// Serve one matched request.
    ///
    /// `request` carries `{method, path, params, query, headers, body}` (headers
    /// lowercased, latin-1 decoded as python's ASGI layer hands them over). The
    /// reply is `{status, content_type?, headers?, body?}` — a missing
    /// `content_type` means the response carries no body at all (204).
    func serveRoute(legId: AgentId, request: JSON, kernel: Kernel) async -> JSON
}
