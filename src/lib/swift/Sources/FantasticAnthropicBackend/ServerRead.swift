// What the record leaves out is READ FROM THE API (owner, 2026-10-07) — python
// `anthropic_backend/server.py`, text for text: `max_tokens` from the model entry's
// `max_tokens`, `context_window` from its `max_input_tokens`
// (`GET {endpoint with /messages replaced by /models}/{model}`), written back.

import AsyncHTTPClient
import FantasticAICore
import FantasticJSON
import FantasticKernel
import Foundation
import NIOCore
import OrderedCollections

enum AnthropicServerRead {
    /// The fields a record may leave to the API, in the order they are read.
    static let INFERRED = ["max_tokens", "context_window"]
    /// The verbs that run a turn of the model — the ones the API is read for.
    static let TURN_VERBS: Set<String> = ["send", "inquiry", "context_recall"]
    static let READ_TIMEOUT: Int64 = 30

    struct Failure: Error, Equatable {
        let field: String
        let message: String
    }

    static func source(_ field: String) -> String {
        field == "max_tokens" ? "max_tokens" : "max_input_tokens"
    }

    /// The record's fields left to the API.
    static func missing(_ rec: OrderedDictionary<String, JSON>) -> [String] {
        INFERRED.filter { rec[$0] == nil || rec[$0] == .null }
    }

    /// The model list beside a Messages endpoint; nil when it does not end in
    /// `/messages`.
    static func modelsURL(_ endpoint: String) -> String? {
        var e = endpoint
        while e.hasSuffix("/") { e.removeLast() }
        guard e.hasSuffix("/messages") else { return nil }
        return String(e.dropLast("/messages".count)) + "/models"
    }

    /// The values for the record's absent fields.
    static func read(
        _ rec: OrderedDictionary<String, JSON>, key: KeyReader
    ) async throws(Failure) -> OrderedDictionary<String, JSON> {
        let want = missing(rec)
        let first = want.first ?? "max_tokens"
        var endpoint = rec["endpoint"]?.asString ?? ""
        while endpoint.hasSuffix("/") { endpoint.removeLast() }
        guard let base = modelsURL(endpoint) else {
            throw Failure(
                field: first,
                message: "\(first): the endpoint \(endpoint) does not end in /messages, so the model "
                    + "list is not known — set \(first) on the record")
        }
        let url = "\(base)/\(rec["model"]?.asString ?? "")"
        let secret: String?
        do {
            secret = try await key(endpoint)
        } catch {
            throw Failure(field: first, message: pyErrorText(error))
        }
        var req = HTTPClientRequest(url: url)
        req.method = .GET
        anthropicHeaders(&req, key: secret)
        let unreachable = Failure(
            field: first,
            message: "\(first): GET \(url) failed — the API is not reachable there; set \(first) on the record")
        let response: HTTPClientResponse
        do {
            response = try await HTTPClient.shared.execute(req, timeout: .seconds(READ_TIMEOUT))
        } catch {
            throw unreachable
        }
        let code = Int(response.status.code)
        guard code == 200 else {
            _ = try? await response.body.collect(upTo: 1 << 20)
            let why = code == 401 || code == 403
                ? " — the API wants a key: name it in `credentials` ({store, ref})" : ""
            throw Failure(
                field: first, message: "\(first): GET \(url) answered HTTP \(code)\(why); set \(first) on the record")
        }
        guard let buf = try? await response.body.collect(upTo: 16 << 20) else { throw unreachable }
        let entry = (try? JSON.parse(String(buffer: buf))) ?? .null
        var out: OrderedDictionary<String, JSON> = [:]
        for f in want {
            guard case .integer(let n) = entry[source(f)], n >= 1 else {
                throw Failure(field: f, message: "\(f): \(url) names no \(source(f)); set \(f) on the record")
            }
            out[f] = .integer(n)
        }
        return out
    }

    /// Read the record's absent fields from the API and write them back. nil when
    /// nothing is absent or all were written; otherwise the verb's refusal.
    static func resolve(
        _ agentId: AgentId, kernel: Kernel, key: KeyReader,
        validate: (OrderedDictionary<String, JSON>, OrderedDictionary<String, JSON>) -> String?
    ) async -> JSON? {
        guard let agent = kernel.agent(agentId) else { return nil }
        let rec = agent.meta
        let named: (String) -> Bool = { !pyStrip(rec[$0]?.asString ?? "").isEmpty }
        // nothing to read, or nothing to read it with — the provider names a missing
        // `model` / `endpoint` when it is built
        if missing(rec).isEmpty || !named("model") || !named("endpoint") { return nil }
        let values: OrderedDictionary<String, JSON>
        do {
            values = try await read(rec, key: key)
        } catch {
            return Refusal.make(Refusal.badArg, error.message, extra: [("arg", .string(error.field))])
        }
        if let bad = validate(values, rec) {
            return Refusal.make(
                Refusal.badArg, "the API's values do not fit the record: \(bad)",
                extra: [("arg", .string(values.keys.first ?? ""))])
        }
        kernel.update(agentId, values)
        return nil
    }
}
