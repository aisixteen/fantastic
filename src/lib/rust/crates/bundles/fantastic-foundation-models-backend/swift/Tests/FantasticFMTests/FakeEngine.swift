// The unit suite's on-device model: an `FMEngine` that answers from a script,
// so nothing in the default suite needs FoundationModels, Apple Intelligence or
// macOS 26. It runs FMCore's own `fmPreflight` first — the refusals it shows are
// the real engine's, word for word.

import FantasticFMCore
import Foundation

final class FakeEngine: FMEngine, @unchecked Sendable {
    private let lock = NSLock()
    private var _availability: FMAvailability
    private var script: [[FMEvent]]
    private var _requests: [FMRequest] = []
    /// Thrown after a pass's events, when set.
    private let failure: FMError?
    /// After its events, a pass waits until it is cancelled.
    private let hang: Bool
    /// What a pass streams once the script is spent.
    private let reply: [FMEvent]

    let contextSize: Int
    let contextSizeSource: FMWindowSource = .sdk

    init(
        availability: FMAvailability = .available,
        contextSize: Int = 32768,
        script: [[FMEvent]] = [],
        reply: [FMEvent] = [.text("ok "), .text("<call name=\"done\"></call>")],
        failure: FMError? = nil,
        hang: Bool = false
    ) {
        self._availability = availability
        self.contextSize = contextSize
        self.script = script
        self.reply = reply
        self.failure = failure
        self.hang = hang
    }

    func availability() -> FMAvailability { lock.withLock { _availability } }

    func setAvailability(_ a: FMAvailability) { lock.withLock { _availability = a } }

    /// Every request a stream was opened with, in order.
    var requests: [FMRequest] { lock.withLock { _requests } }

    func stream(_ request: FMRequest) -> AsyncThrowingStream<FMEvent, Error> {
        let (availability, events) = lock.withLock { () -> (FMAvailability, [FMEvent]) in
            _requests.append(request)
            return (_availability, script.isEmpty ? reply : script.removeFirst())
        }
        let failure = failure
        let hang = hang
        return AsyncThrowingStream { continuation in
            let task = Task {
                do {
                    try fmPreflight(availability, request)
                    for e in events { continuation.yield(e) }
                    if hang {
                        while !Task.isCancelled { try? await Task.sleep(nanoseconds: 2_000_000) }
                        throw CancellationError()
                    }
                    if let failure { throw failure }
                    continuation.finish()
                } catch {
                    continuation.finish(throwing: error)
                }
            }
            continuation.onTermination = { _ in task.cancel() }
        }
    }
}

/// Poll `cond` until it holds or `seconds` pass — bounded, so a hung stream fails
/// the test instead of the run.
func eventually(_ seconds: Double = 5, _ cond: () -> Bool) async -> Bool {
    let deadline = Date().addingTimeInterval(seconds)
    while Date() < deadline {
        if cond() { return true }
        try? await Task.sleep(nanoseconds: 5_000_000)
    }
    return cond()
}
