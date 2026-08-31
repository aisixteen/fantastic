// Stateful fixture agents the shared e2e scenarios name.
//
// `e2e/tests/scenario/*/scenario.yaml` declares a roster by ROLE. Python and js
// register these roles in process — a handler is thirty lines there. A compiled
// kernel can only run what is compiled INTO it, and the e2e driver reaches this
// binary over REST, so there is no way to inject a handler through a socket.
// Without this module `ledger`, `deadletter` and `user_actions` are SKIPPED on
// swift — six of twenty scenario cells that never execute.
//
// TEST-ONLY, and gated so it cannot leak: registration is behind
// `#if SCENARIO_PROBES`, which only `e2e/runners/host.py` builds with. A
// production binary carries none of it.
//
// Substituting a shipped bundle instead (`durable_memory` for `dispenser`) was the
// alternative and is worse: it has no `dispense` verb and no side-effect
// counter, so the row would go green while measuring something else.
//
// Each role COUNTS its side effects, because that is what the scenarios assert
// on — a model that claims it bought once cannot fake the desk's own tally.

import FantasticJSON
import FantasticKernel
import Foundation

/// A counter a scenario reads back through the agent's own `count` verb.
private final class Counter: @unchecked Sendable {
    private let lock = NSLock()
    private var n = 0
    func bump() -> Int {
        lock.lock(); defer { lock.unlock() }
        n += 1
        return n
    }
    var value: Int {
        lock.lock(); defer { lock.unlock() }
        return n
    }
}

private func token() -> String {
    String(UUID().uuidString.prefix(8)).lowercased()
}

public let DISPENSER_MODULE = "dispenser_probe.tools"
public let PLANT_MODULE = "plant_probe.tools"
public let PURCHASE_MODULE = "purchase_probe.tools"

/// `dispenser` — one coin per `dispense`. Issuing is a SIDE EFFECT: calling
/// twice dispenses twice, which is exactly what the double-fire scenario exists
/// to catch. A named execution must return the FIRST run's recorded outcome
/// rather than running this handler again.
public struct DispenserProbe: AgentBundle {
    public let name = "dispenser_probe"
    private let count = Counter()
    public init() {}

    public func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString ?? "" {
        case "boot", "shutdown": return nil
        case "reflect":
            return .object([
                "id": .string(agentId.value),
                "sentence": .string(
                    "Coin dispenser — one coin per `dispense` call. A coin must never be issued twice for one job."),
                "verbs": .object([
                    "dispense": .string(
                        "args: job:str — issues ONE coin, returns {coin, receipt}. Issuing is a side effect: calling twice dispenses twice."),
                    "count": .string("how many coins have ever been dispensed"),
                ]),
            ])
        case "dispense":
            return .object([
                "coin": .integer(Int64(count.bump())),
                "receipt": .string(token()),
                "job": payload["job"],
            ])
        case "count":
            return .object(["count": .integer(Int64(count.value))])
        case let other:
            return .object(["error": .string("dispenser_probe: unknown verb \(other)")])
        }
    }
}

/// `flaky_plant` — a pump that fails twice then works, and a valve that never
/// does. Both THROW: a terminal failure is what a retry budget and a dead-letter
/// row are for, and a role that quietly succeeded would make the scenario
/// unfalsifiable.
public struct PlantProbe: AgentBundle {
    public let name = "plant_probe"
    private let runs = Counter()
    public init() {}

    public func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString ?? "" {
        case "boot", "shutdown": return nil
        case "reflect":
            return .object([
                "id": .string(agentId.value),
                "sentence": .string(
                    "Industrial plant. Its pump is unreliable; its valve has seized."),
                "verbs": .object([
                    "pump": .string("no args — runs the pump. Fails intermittently."),
                    "valve": .string("no args — operates the valve."),
                    "count": .string("how many times either has been attempted"),
                ]),
            ])
        case "pump":
            // FLAKY, not broken: the third attempt works. A caller that gives up
            // after one try never learns that — which is the measurement.
            if runs.bump() < 3 { throw ProbeError.failed("pump stalled") }
            return .object(["pumped": .bool(true), "serial": .string(token())])
        case "valve":
            _ = runs.bump()
            throw ProbeError.failed("valve is rusted shut")
        case "count":
            return .object(["count": .integer(Int64(runs.value))])
        case let other:
            return .object(["error": .string("plant_probe: unknown verb \(other)")])
        }
    }
}

/// `purchase_desk` — spends real money, so only a person may press the button.
/// The AI is deliberately granted NO edge to it: that it cannot buy is half the
/// measurement, and the ledger must make a double-click harmless.
public struct PurchaseProbe: AgentBundle {
    public let name = "purchase_probe"
    private let bought = Counter()
    public init() {}

    public func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString ?? "" {
        case "boot", "shutdown": return nil
        case "reflect":
            return .object([
                "id": .string(agentId.value),
                "sentence": .string(
                    "Purchase desk. Spends real money — only a person may press the button."),
                "verbs": .object([
                    "buy": .string("args: item:str — buys ONE. Spending is a side effect."),
                    "count": .string("how many purchases have been made"),
                ]),
            ])
        case "buy":
            return .object([
                "bought": payload["item"],
                "receipt": .string(token()),
                "n": .integer(Int64(bought.bump())),
            ])
        case "count":
            return .object(["count": .integer(Int64(bought.value))])
        case let other:
            return .object(["error": .string("purchase_probe: unknown verb \(other)")])
        }
    }
}

public enum ProbeError: Error { case failed(String) }
