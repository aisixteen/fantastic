// What a record leaves out is READ FROM THE SERVER — python
// `vllm_backend/server.py`, text for text.
//
// `model`, `max_tokens`, `context_window` and `kv_slots` may be left off the record.
// Before a verb that runs a turn (`TURN_VERBS`), every one still absent is read from
// the server the record's `endpoint` names and WRITTEN BACK to the record — it then
// holds a measured value like one the operator set: reflect shows it, a reboot
// keeps it, and the server is not asked again until the field is cleared (`null`).
// A field the record sets is used as set, never read.
//
//   * `model` — the ONE model the server serves (`GET {endpoint}/models`); a server
//     serving several names them, and the record must pick one.
//   * `context_window` and `max_tokens` — that model's `max_model_len` (same list).
//   * `kv_slots` — `floor(kv_cache_max_concurrency)` of the `vllm:cache_config_info`
//     line on the server's Prometheus page — the record's `metrics_url`, or, left
//     out, the endpoint with its `/v1` replaced by `/metrics`.
//
// A server that does not answer refuses the verb, naming the field, the URL and
// what came back. The key goes only to the endpoint's host, as on a chat request.

import AsyncHTTPClient
import FantasticAICore
import FantasticJSON
import FantasticKernel
import Foundation
import NIOCore
import OrderedCollections

enum ServerRead {
    /// The fields a record may leave to the server, in the order they are read.
    static let INFERRED = ["model", "max_tokens", "context_window", "kv_slots"]
    /// The verbs that run a turn of the model — the ones the server is read for.
    static let TURN_VERBS: Set<String> = ["send", "inquiry", "context_recall"]

    static let INFO = "vllm:cache_config_info"

    /// A field the server did not answer, and what to tell the caller.
    struct Failure: Error, Equatable {
        let field: String
        let message: String
    }

    /// The record's fields left to the server.
    static func missing(_ rec: OrderedDictionary<String, JSON>) -> [String] {
        INFERRED.filter { rec[$0] == nil || rec[$0] == .null }
    }

    /// The record's `metrics_url`, or the endpoint's `/v1` replaced by `/metrics`;
    /// nil when neither names one (an endpoint that does not end in `/v1`).
    static func metricsURL(_ rec: OrderedDictionary<String, JSON>) -> String? {
        if case .string(let url)? = rec["metrics_url"], !pyStrip(url).isEmpty {
            return pyStrip(url)
        }
        var endpoint = rec["endpoint"]?.asString ?? ""
        while endpoint.hasSuffix("/") { endpoint.removeLast() }
        guard endpoint.hasSuffix("/v1") else { return nil }
        return String(endpoint.dropLast(3)) + "/metrics"
    }

    /// The `/models` entry the record names, or the ONE the server serves.
    static func servedModel(
        _ body: String, model: String?, url: String, field: String
    ) throws(Failure) -> OrderedDictionary<String, JSON> {
        guard let parsed = try? JSON.parse(body), let data = parsed["data"].asArray
        else { throw Failure(field: field, message: "\(field): GET \(url) is not a model list") }
        let entries: [OrderedDictionary<String, JSON>] = data.compactMap {
            if case .object(let o) = $0 { return o }
            return nil
        }
        let names = entries.map { e -> String in
            switch e["id"] {
            case .string(let s)?: return s
            case let v?: return pyStr(v)
            case nil: return "None"
            }
        }.joined(separator: ", ")
        let served = names.isEmpty ? "nothing" : names
        guard let model else {
            if entries.count == 1 { return entries[0] }
            throw Failure(
                field: "model",
                message: "model: \(url) serves \(served) — not one model; set model on the record")
        }
        guard let entry = entries.first(where: { $0["id"]?.asString == model }) else {
            throw Failure(
                field: field,
                message: "\(field): \(url) does not serve model '\(model)' — it serves \(served)")
        }
        return entry
    }

    /// That entry's `max_model_len`.
    static func maxModelLen(
        _ entry: OrderedDictionary<String, JSON>, url: String, field: String
    ) throws(Failure) -> Int64 {
        if case .integer(let n)? = entry["max_model_len"], n >= 1 { return n }
        let id: String
        switch entry["id"] {
        case .string(let s)?: id = "'\(s)'"
        case let v?: id = pyStr(v)
        case nil: id = "None"
        }
        throw Failure(
            field: field,
            message: "\(field): \(url) names no max_model_len for \(id); set \(field) on the record")
    }

    /// The `name="value"` labels of one Prometheus line, in order.
    static func labels(_ line: String) -> [(String, String)] {
        var out: [(String, String)] = []
        guard let open = line.firstIndex(of: "{") else { return out }
        var rest = line[line.index(after: open)...]
        while true {
            while let c = rest.first, c == "," || c == " " { rest = rest.dropFirst() }
            guard let eq = rest.range(of: "=\"") else { return out }
            let name = String(rest[rest.startIndex..<eq.lowerBound])
            var value = ""
            var i = eq.upperBound
            var closed = false
            while i < rest.endIndex {
                let c = rest[i]
                if c == "\\" {
                    let n = rest.index(after: i)
                    if n < rest.endIndex { value.append(rest[n]); i = rest.index(after: n) } else { i = n }
                    continue
                }
                if c == "\"" { closed = true; i = rest.index(after: i); break }
                value.append(c)
                i = rest.index(after: i)
            }
            guard closed else { return out }
            out.append((name, value))
            rest = rest[i...]
        }
    }

    /// `floor(kv_cache_max_concurrency)` of the ONE `vllm:cache_config_info` line.
    static func kvSlots(_ body: String, url: String) throws(Failure) -> Int64 {
        let lines = body.split(separator: "\n", omittingEmptySubsequences: false)
            .filter { $0.hasPrefix(INFO + "{") }
        guard lines.count == 1 else {
            throw Failure(
                field: "kv_slots",
                message: "kv_slots: \(url) carries \(lines.count) \(INFO) lines, not one — set "
                    + "kv_slots on the record")
        }
        guard let raw = labels(String(lines[0])).first(where: { $0.0 == "kv_cache_max_concurrency" })?.1,
            let value = Double(pyStrip(raw))
        else {
            throw Failure(
                field: "kv_slots",
                message: "kv_slots: \(url) names no kv_cache_max_concurrency — set kv_slots on "
                    + "the record")
        }
        let slots = value.rounded(.down)
        guard slots >= 1 else {
            throw Failure(
                field: "kv_slots",
                message: "kv_slots: \(url) holds \(raw) full-length conversations, under one — set "
                    + "kv_slots on the record")
        }
        return Int64(slots)
    }

    static func netloc(_ url: String) -> String {
        let rest = url.range(of: "://").map { String(url[$0.upperBound...]) } ?? url
        return String(rest.prefix { $0 != "/" && $0 != "?" && $0 != "#" })
    }

    /// One GET, its body as text, or the failure that names the field.
    static func get(
        _ url: String, key: String?, field: String, connect: Double, read: Double
    ) async throws(Failure) -> String {
        var req = HTTPClientRequest(url: url)
        req.method = .GET
        if let key { req.headers.add(name: "Authorization", value: "Bearer \(key)") }
        let unreachable = Failure(
            field: field,
            message: "\(field): GET \(url) failed — the server is not reachable there; set "
                + "\(field) on the record")
        let client = VllmHTTP.client(connect: connect, read: read)
        let response: HTTPClientResponse
        do {
            response = try await client.execute(req, deadline: .distantFuture)
        } catch {
            throw unreachable
        }
        let code = Int(response.status.code)
        guard code == 200 else {
            _ = try? await response.body.collect(upTo: 1 << 20)
            throw Failure(
                field: field,
                message: "\(field): GET \(url) answered HTTP \(code) — the server (or a door in "
                    + "front of it) does not serve it; set \(field) on the record")
        }
        guard let buf = try? await response.body.collect(upTo: 64 << 20) else { throw unreachable }
        return String(buffer: buf)
    }

    static func seconds(_ rec: OrderedDictionary<String, JSON>, _ key: String, _ dflt: Double) -> Double {
        switch rec[key] {
        case .integer(let n)? where n > 0: return Double(n)
        case .double(let d)? where d > 0: return d
        default: return dflt
        }
    }

    /// The values for the record's absent fields, read from the server.
    static func read(
        _ rec: OrderedDictionary<String, JSON>, key: KeyReader
    ) async throws(Failure) -> OrderedDictionary<String, JSON> {
        let want = missing(rec)
        var endpoint = rec["endpoint"]?.asString ?? ""
        while endpoint.hasSuffix("/") { endpoint.removeLast() }
        let secret: String?
        do {
            secret = try await key(endpoint).flatMap { $0.isEmpty ? nil : $0 }
        } catch {
            throw Failure(field: want.first ?? "model", message: pyErrorText(error))
        }
        let connect = seconds(rec, "connect_timeout", CONNECT_TIMEOUT)
        let read = seconds(rec, "read_timeout", READ_TIMEOUT)
        var out: OrderedDictionary<String, JSON> = [:]
        if let field = want.first(where: { $0 != "kv_slots" }) {
            let url = "\(endpoint)/models"
            let body = try await get(url, key: secret, field: field, connect: connect, read: read)
            let entry = try servedModel(
                body, model: rec["model"]?.asString, url: url, field: field)
            if want.contains("model") { out["model"] = entry["id"] ?? .null }
            for f in ["max_tokens", "context_window"] where want.contains(f) {
                out[f] = .integer(try maxModelLen(entry, url: url, field: f))
            }
        }
        if want.contains("kv_slots") {
            guard let url = metricsURL(rec) else {
                throw Failure(
                    field: "kv_slots",
                    message: "kv_slots: the endpoint \(endpoint) does not end in /v1, so the "
                        + "server's metrics page is not known — set metrics_url or kv_slots on "
                        + "the record")
            }
            let k = netloc(url) == netloc(endpoint) ? secret : nil
            let body = try await get(url, key: k, field: "kv_slots", connect: connect, read: read)
            out["kv_slots"] = .integer(try kvSlots(body, url: url))
        }
        return out
    }

    /// Read the record's absent fields from the server and write them back. nil
    /// when nothing is absent or all were written; otherwise the verb's refusal.
    static func resolve(
        _ agentId: AgentId, kernel: Kernel, key: KeyReader,
        validate: (OrderedDictionary<String, JSON>, OrderedDictionary<String, JSON>) -> String?
    ) async -> JSON? {
        guard let agent = kernel.agent(agentId) else { return nil }
        let rec = agent.meta
        if missing(rec).isEmpty { return nil }
        let values: OrderedDictionary<String, JSON>
        do {
            values = try await read(rec, key: key)
        } catch {
            return Refusal.make(Refusal.badArg, error.message, extra: [("arg", .string(error.field))])
        }
        // What the server gave passes the same write check as the operator's values.
        if let bad = validate(values, rec) {
            return Refusal.make(
                Refusal.badArg, "the server's values do not fit the record: \(bad)",
                extra: [("arg", .string(values.keys.first ?? ""))])
        }
        kernel.update(agentId, values)
        return nil
    }
}
