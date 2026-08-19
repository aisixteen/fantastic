// The interpreter ALLOWLIST — swift mirror of py `exec_bridge/proc.py` (canonical).
//
// `interpreters` on the record is the ONLY set that can be spawned. No per-call
// override, no default, no PATH fallback: an empty list refuses telling the
// operator what to set; several entries and no `interpreter` refuses as ambiguous;
// an unlisted request refuses NAMING the legal set (the mechanism measured to let
// a small model recover). NOT a sandbox — the allowlist buys exactly one property:
// the OPERATOR chose which runtime and environment run here.

import FantasticJSON
import FantasticKernel
import Foundation

enum Interpreter {
    /// The leg's allowlisted interpreters, `~` expanded. Empty ⇒ spawns nothing.
    static func allowed(record: Agent?) -> [String] {
        let raw = record?.metaValue(forKey: "interpreters")
        var items: [String] = []
        switch raw {
        case .some(.string(let s)): items = [s]
        case .some(.array(let a)): items = a.compactMap { $0.asString }
        default: items = []
        }
        return items
            .filter { !$0.trimmingCharacters(in: .whitespaces).isEmpty }
            .map { (($0 as NSString).expandingTildeInPath) }
    }

    /// The interpreter to spawn, or a refusal naming the legal set.
    static func resolve(payload: JSON, record: Agent?) throws -> String {
        let ok = allowed(record: record)
        if ok.isEmpty {
            throw Err(
                "no interpreters are allowlisted on this leg — an operator must set "
                    + "`interpreters=[<abs path to an interpreter>]` before a program can run")
        }
        let requested = (payload["interpreter"].asString
            ?? record?.metaValue(forKey: "interpreter")?.asString)?
            .trimmingCharacters(in: .whitespaces)
        guard let req = requested, !req.isEmpty else {
            if ok.count == 1 { return ok[0] }
            throw Err(
                "this leg allows \(ok.count) interpreters, so name one: "
                    + "interpreter=<one of \(ok)>")
        }
        let want = (req as NSString).expandingTildeInPath
        guard ok.contains(want) else {
            throw Err("interpreter \"\(req)\" is not allowlisted — \(ok)")
        }
        return want
    }

    struct Err: Error, CustomStringConvertible {
        let message: String
        init(_ m: String) { message = m }
        var description: String { message }
    }
}
