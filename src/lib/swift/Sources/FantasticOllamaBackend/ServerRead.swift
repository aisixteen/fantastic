// A record that leaves `model` out has it READ FROM THE SERVER
// — python `ollama_backend/server.py`, text for text.
//
// Before a verb that runs a turn (`TURN_VERBS`), an absent `model` is read from the
// server the record's `endpoint` names — the ONE model `GET {endpoint}/api/tags`
// lists — and WRITTEN BACK to the record. Only the model: the window ollama loads a
// model with is not in the API.

import AsyncHTTPClient
import FantasticAICore
import FantasticJSON
import FantasticKernel
import Foundation
import NIOCore
import OrderedCollections

enum OllamaServerRead {
    /// The verbs that run a turn of the model — the ones the server is read for.
    static let TURN_VERBS: Set<String> = ["send", "inquiry", "context_recall"]
    /// Seconds to wait on the model list.
    static let READ_TIMEOUT: Int64 = 30

    struct Failure: Error, Equatable { let message: String }

    /// The ONE model a `/api/tags` body lists.
    static func servedModel(_ body: String, url: String) throws(Failure) -> String {
        guard let parsed = try? JSON.parse(body), let models = parsed["models"].asArray else {
            throw Failure(message: "model: GET \(url) is not a model list")
        }
        let names: [JSON] = models.compactMap {
            guard case .object(let o) = $0 else { return nil }
            return o["name"] ?? .null
        }
        if names.count == 1, case .string(let n) = names[0], !n.isEmpty { return n }
        let served = names.map { pyStr($0) }.joined(separator: ", ")
        throw Failure(
            message: "model: \(url) serves \(served.isEmpty ? "nothing" : served) — not one model; "
                + "set model on the record")
    }

    /// The model the server at `endpoint` serves.
    static func readModel(_ endpoint: String, key: KeyReader) async throws(Failure) -> String {
        var base = endpoint
        while base.hasSuffix("/") { base.removeLast() }
        let url = "\(base)/api/tags"
        let secret: String?
        do {
            secret = try await key(endpoint).flatMap { $0.isEmpty ? nil : $0 }
        } catch {
            throw Failure(message: pyErrorText(error))
        }
        var req = HTTPClientRequest(url: url)
        req.method = .GET
        if let secret { req.headers.add(name: "Authorization", value: "Bearer \(secret)") }
        let unreachable = Failure(
            message: "model: GET \(url) failed — the server is not reachable there; set model on the record")
        let response: HTTPClientResponse
        do {
            response = try await HTTPClient.shared.execute(req, timeout: .seconds(READ_TIMEOUT))
        } catch {
            throw unreachable
        }
        let code = Int(response.status.code)
        guard code == 200 else {
            _ = try? await response.body.collect(upTo: 1 << 20)
            throw Failure(
                message: "model: GET \(url) answered HTTP \(code) — the server (or a door in front "
                    + "of it) does not serve it; set model on the record")
        }
        guard let buf = try? await response.body.collect(upTo: 16 << 20) else { throw unreachable }
        return try servedModel(String(buffer: buf), url: url)
    }

    /// Read an absent `model` from the server and write it back. nil when the record
    /// names one or it was written; otherwise the verb's refusal.
    static func resolve(_ agentId: AgentId, kernel: Kernel, key: KeyReader) async -> JSON? {
        guard let agent = kernel.agent(agentId) else { return nil }
        let rec = agent.meta
        let endpoint = pyStrip(rec["endpoint"]?.asString ?? "")
        // Nothing to read without an endpoint — the provider names it when it is built.
        if (rec["model"] != nil && rec["model"] != .null) || endpoint.isEmpty { return nil }
        do {
            let model = try await readModel(endpoint, key: key)
            kernel.update(agentId, ["model": .string(model)])
            return nil
        } catch {
            return Refusal.make(Refusal.badArg, error.message, extra: [("arg", .string("model"))])
        }
    }
}
