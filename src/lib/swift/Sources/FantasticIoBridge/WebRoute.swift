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
// is why `handle_route` is not a verb: as one, every request would be a second,
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
    /// The second half is THE OCTET REPLY: `nil` = no chunk,
    /// `.some(o)` = a chunk, `o.bytes` possibly EMPTY (an empty file read), with
    /// the field it came out of and the reply without it (`RestOctet.split`). The
    /// host turns that — never the body's length — into
    /// `application/octet-stream`.
    case http(@Sendable (_ request: JSON) async -> (JSON, OctetReply?))
    /// Serve one upgraded websocket: the handler the host installs on the
    /// upgraded channel.
    case websocket(@Sendable (_ upgrade: WebSocketUpgrade) -> any ChannelHandler & Sendable)
}

/// A bundle whose agents declare web routes. Python `route_endpoints(agent)`:
/// `{declared path: live endpoint}` for one agent of this bundle.
public protocol WebRouteEndpoints {
    func routeEndpoints(agent: Agent, kernel: Kernel) -> [String: WebEndpoint]
}

/// A REST reply that goes back as `application/octet-stream`: the chunk, the
/// reply key it came out of (`X-Fantastic-Bytes-Field`) and the reply WITHOUT that
/// key (`X-Fantastic-Payload`).
public struct OctetReply: Sendable, Equatable {
    public let field: String
    public let payloadHeader: String
    public let bytes: Data
}

/// The ONE reply-shape decision of both REST legs (the JSON leg's endpoint and
/// the host's octet leg) — python `web_rest._first_bytes_field`. A kernel reply
/// whose chunk sits at a TOP-LEVEL key goes back the way a body comes in: the
/// chunk as the octet body, the reply WITHOUT that key on `X-Fantastic-Payload`
/// (python `json.dumps(rest)`, default separators) and the key on
/// `X-Fantastic-Bytes-Field` — `bytes` for a `read_stream` chunk, `x.y` for a
/// peer's reply that put it there. A chunk anywhere else has no JSON form on the
/// leg (python's `JSONResponse` cannot render bytes inside the reply): the
/// stated `noJSONForm` refusal, 500. Rust `octet_or_json`.
public enum RestOctet {
    /// The octet reply, or `nil` when the chunk does not sit at a top-level key
    /// of an object reply.
    public static func split(_ reply: JSON, _ body: Blob) -> OctetReply? {
        guard body.parts.count == 1, case .key(let field) = body.parts[0],
            case .object(var rest) = reply
        else { return nil }
        // the bytes value never travels twice: the header is the reply MINUS it
        rest.removeValue(forKey: field)
        return OctetReply(field: field, payloadHeader: pyJsonDumps(.object(rest)), bytes: body.bytes)
    }

    /// python `_json_reply`'s refusal, served 500: the reply holds bytes where
    /// this leg has no JSON for them.
    public static let noJSONForm: JSON = Refusal.make(
        Refusal.transportError, "web_rest: the reply has no JSON representation on this leg",
        hint: "this verb answers with in-process objects; call it in-process, not over HTTP")
}
