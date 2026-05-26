// Spatial UI host agent.
//
// Mirrors Rust's `fantastic-canvas-backend::CanvasBackendBundle`. Members
// of the canvas are REAL CHILD AGENTS spawned via `core.create_agent`,
// not entries in a JSON array. Layout metadata (`x`, `y`, `width`,
// `height`) lives on each child's own `meta`. Discovery is a spatial
// filter over the children.
//
// Earlier this Swift port stored members in `meta.members` as a JSON
// array of dicts — that diverged from Rust and meant the canvas frontend
// couldn't `t.call(<member_id>, {type:"get_gl_view"})` because there was
// no real agent at that id. This file restores the Rust contract.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

public let HANDLER_MODULE = "canvas_backend.tools"

public struct CanvasBackendBundle: AgentBundle {
    public let name = "canvas_backend"
    public init() {}

    public func handle(
        agentId: AgentId,
        payload: JSON,
        kernel: Kernel
    ) async throws -> JSON? {
        let verb = payload["type"].asString ?? ""
        guard let agent = kernel.agent(agentId) else {
            return .object(["error": .string("no agent")])
        }
        switch verb {
        case "reflect":
            return [
                "id": .string(agent.id.value),
                "kind": .string("canvas_backend"),
                "sentence": .string("Spatial UI host — children = members."),
                "member_count": .integer(Int64(agent.childIds().count)),
                "verbs": [
                    "add_agent": "args: handler_module (or agent_id), x?, y?, w?, h?.",
                    "remove_agent": "args: agent_id.",
                    "discover": "args: x, y, w, h. Returns intersecting members.",
                    "list_members": "Returns [{id}] for every child.",
                ] as JSON,
            ] as JSON
        case "boot", "shutdown":
            return .object(["ok": .bool(true)])
        case "add_agent":
            return await addAgentVerb(canvasId: agent.id, payload: payload, kernel: kernel)
        case "remove_agent":
            return await removeAgentVerb(canvasId: agent.id, payload: payload, kernel: kernel)
        case "discover":
            return discoverVerb(canvasAgent: agent, payload: payload, kernel: kernel)
        case "list_members":
            let members = agent.childIds().map { JSON.string($0.value) }
            return .object(["members": .array(members)])
        default:
            return .object(["error": .string("unknown verb \(verb)")])
        }
    }

    /// Spawn a child agent under the canvas, then probe it for one of the
    /// two render contracts (`get_webapp` for DOM iframes, `get_gl_view`
    /// for shared-canvas GL). Cascade-delete the new member if neither
    /// answers — canvas-eligibility requires a UI verb.
    ///
    /// Ports `rust/crates/bundles/fantastic-canvas-backend/src/lib.rs:102-194`.
    private func addAgentVerb(canvasId: AgentId, payload: JSON, kernel: Kernel) async -> JSON {
        let handlerModule = payload["handler_module"].asString
        let existingId = payload["agent_id"].asString

        let newMemberId: AgentId
        if let hm = handlerModule {
            // Path A: spawn a fresh member as a child of the canvas.
            // Build a create_agent payload that flattens every key from
            // the original (so x/y/width/height land on the new record)
            // except framing fields.
            var createPayload: OrderedDictionary<String, JSON> = [:]
            createPayload["type"] = .string("create_agent")
            createPayload["handler_module"] = .string(hm)
            if case let .object(obj) = payload {
                for (k, v) in obj {
                    if k == "type" || k == "handler_module" || k == "agent_id" {
                        continue
                    }
                    createPayload[k] = v
                }
            }
            let reply = await kernel.send(canvasId, .object(createPayload))
            if let err = reply["error"].asString {
                return .object([
                    "error": .string("add_agent: create failed: \(err)")
                ])
            }
            guard let id = reply["id"].asString else {
                return .object([
                    "error": .string("add_agent: create returned no id")
                ])
            }
            newMemberId = AgentId(id)
        } else if existingId != nil {
            // Path B: re-parent. Rust kernel refuses this as Phase-1
            // scope (substrate doesn't expose re-parenting as a system
            // verb yet). Match.
            return .object([
                "error": .string(
                    "add_agent: re-parenting existing agent not yet supported (Phase 1)")
            ])
        } else {
            return .object([
                "error": .string("add_agent: requires handler_module or agent_id")
            ])
        }

        // Probe both render verbs in parallel.
        let webappReply = await kernel.send(newMemberId, .object(["type": .string("get_webapp")]))
        let hasDom = webappReply["error"].asString == nil
            && webappReply["url"].asString != nil
        let glReply = await kernel.send(newMemberId, .object(["type": .string("get_gl_view")]))
        let hasGl = glReply["error"].asString == nil
            && glReply["source"].asString != nil

        if !hasDom && !hasGl {
            // Cascade-delete the just-spawned member.
            _ = await kernel.send(
                canvasId,
                .object([
                    "type": .string("delete_agent"),
                    "id": .string(newMemberId.value),
                ]))
            return .object([
                "error": .string(
                    "add_agent: '\(newMemberId.value)' answers neither get_webapp nor get_gl_view; nothing to render"
                )
            ])
        }

        // Re-fetch the canvas to read its current child_ids (the spawn
        // mutated it in place).
        let members =
            kernel.agent(canvasId)?.childIds().map { JSON.string($0.value) } ?? []
        await kernel.emit(
            canvasId,
            .object([
                "type": .string("members_updated"),
                "members": .array(members),
            ]))
        return .object([
            "ok": .bool(true),
            "member_id": .string(newMemberId.value),
            "members": .array(members),
        ])
    }

    /// Cascade-delete a member.
    private func removeAgentVerb(canvasId: AgentId, payload: JSON, kernel: Kernel) async -> JSON {
        guard let targetIdStr = payload["agent_id"].asString else {
            return .object(["error": .string("remove_agent requires agent_id")])
        }
        let reply = await kernel.send(
            canvasId,
            .object([
                "type": .string("delete_agent"),
                "id": .string(targetIdStr),
            ]))
        let members =
            kernel.agent(canvasId)?.childIds().map { JSON.string($0.value) } ?? []
        await kernel.emit(
            canvasId,
            .object([
                "type": .string("members_updated"),
                "members": .array(members),
            ]))
        return .object([
            "removed": .bool(reply["error"].asString == nil),
            "members": .array(members),
        ])
    }

    /// Spatial bbox query over members' meta (x/y/width/height).
    private func discoverVerb(canvasAgent: Agent, payload: JSON, kernel: Kernel) -> JSON {
        let x = payload["x"].asDouble ?? 0
        let y = payload["y"].asDouble ?? 0
        let w = payload["w"].asDouble ?? 0
        let h = payload["h"].asDouble ?? 0
        var hits: [JSON] = []
        for cid in canvasAgent.childIds() {
            guard let child = kernel.agent(cid) else { continue }
            let mx = child.metaValue(forKey: "x")?.asDouble ?? 0
            let my = child.metaValue(forKey: "y")?.asDouble ?? 0
            let mw = child.metaValue(forKey: "width")?.asDouble ?? 320
            let mh = child.metaValue(forKey: "height")?.asDouble ?? 240
            if mx < x + w && mx + mw > x && my < y + h && my + mh > y {
                hits.append(.string(cid.value))
            }
        }
        return .object(["members": .array(hits)])
    }
}
