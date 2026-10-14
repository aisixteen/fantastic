// SECRET MARKS on a send item — python `kernel/_secret.py`.
//
// An item may carry `secret: ["<dotted path>", ...]` — paths INTO its payload
// (`"fields.api_key"`). The handler gets the payload as sent; everything the
// kernel keeps or shows of the call — the call row, the state stream's
// `payload`/`summary`, the inbox mirrors to watchers — carries it AS SHOWN: each
// marked value replaced by `Secret.shownValue`. A path naming nothing is
// skipped; `type` cannot be marked (the verb is routing, not data). A handler
// reads the marks as `KernelTaskLocals.currentSecret` and marks its own onward
// send the same way.

import FantasticJSON

extension KernelTaskLocals {
    /// The marks of the item the running dispatch serves.
    @TaskLocal static var secret: [String] = []

    /// The `secret` marks of the item this dispatch serves — dotted paths into
    /// its payload. Empty outside a marked dispatch.
    public static var currentSecret: [String] { secret }
}

public enum Secret {
    /// What a marked value is shown as.
    public static let shownValue = "[secret]"

    /// The item's marks, or a `bad_arg` refusal for a malformed field. Absent ⇒
    /// none.
    static func check(_ raw: JSON?) -> Result<[String], RefusalError> {
        guard let raw, raw != .null else { return .success([]) }
        let marks: [String]? = raw.asArray.flatMap { arr in
            let strs = arr.compactMap { v -> String? in
                guard let s = v.asString, !s.isEmpty,
                    !s.split(separator: ".", omittingEmptySubsequences: false).contains(where: \.isEmpty)
                else { return nil }
                return s
            }
            return strs.count == arr.count ? strs : nil
        }
        guard let marks else {
            return .failure(
                RefusalError(
                    Refusal.make(
                        Refusal.badArg,
                        "send: secret must be a list of dotted paths into the payload "
                            + "(e.g. [\"fields.api_key\"]), got \(pyRepr(raw))",
                        extra: [("arg", .string("secret"))])))
        }
        if marks.contains("type") {
            return .failure(
                RefusalError(
                    Refusal.make(
                        Refusal.badArg, "send: secret may not mark `type` — the verb is not data",
                        extra: [("arg", .string("secret"))])))
        }
        return .success(marks)
    }

    /// `payload` with every marked value replaced by `shownValue`.
    public static func shown(_ payload: JSON, _ marks: [String]) -> JSON {
        var out = payload
        for mark in marks {
            let steps = mark.split(separator: ".").map(String.init)
            guard let leaf = steps.last else { continue }
            var node = out
            var ok = true
            for key in steps.dropLast() {
                guard node.asObject?[key] != nil else { ok = false; break }
                node = node[key]
            }
            guard ok, node.asObject?[leaf] != nil else { continue }
            let parts = steps.map { PathStep.key($0) }
            try? pySetPath(&out, parts: parts, .string(shownValue))
        }
        return out
    }
}

/// A refusal as an `Error`, for `Result`.
struct RefusalError: Error {
    let refusal: JSON
    init(_ refusal: JSON) { self.refusal = refusal }
}
