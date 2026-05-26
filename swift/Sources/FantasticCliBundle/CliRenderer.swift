// Stdout CLI renderer.
//
// Mirrors Rust's `fantastic-cli-bundle`. Subscribes to kernel state
// events + formats them to stdout (token/done/say/error).

import FantasticJSON
import FantasticKernel
import Foundation

/// Attach a CLI renderer to `kernel`'s state event stream. Returns
/// the subscriber token so caller can detach. Mirrors Rust's
/// `attach(kernel)` returning a SubscriberToken.
@discardableResult
public func attach(_ kernel: Kernel) -> SubscriberToken {
    return kernel.subscribe { event in
        let type = event["type"].asString ?? ""
        let target = event["target"].asString ?? ""
        let verb = event["verb"].asString ?? ""
        switch type {
        case "send", "emit":
            FileHandle.standardOutput.write(
                "[\(type)] \(target) \(verb)\n".data(using: .utf8) ?? Data())
        case "created":
            FileHandle.standardOutput.write(
                "[created] \(event["id"].asString ?? "?")\n".data(using: .utf8) ?? Data())
        case "removed":
            FileHandle.standardOutput.write(
                "[removed] \(event["id"].asString ?? "?")\n".data(using: .utf8) ?? Data())
        case "updated":
            FileHandle.standardOutput.write(
                "[updated] \(event["id"].asString ?? "?")\n".data(using: .utf8) ?? Data())
        default:
            break
        }
    }
}
