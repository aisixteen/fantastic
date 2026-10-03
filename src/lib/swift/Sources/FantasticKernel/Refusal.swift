// The ONE shape every refused call answers with.
//
// Port of python `kernel/_refusal.py` (canonical). Before this there were ~18
// shapes across the runtimes: `{error}`, `{error, reason, side, hint}`,
// `{error, created:false}`, `{error, locked:true}`. A caller — human, LLM, or
// another runtime — had to know which verb it had called to know which shape
// came back, and a machine could not branch on the failure at all without
// string-matching the prose.
//
// So: one object, one machine field, one human field.
//
//     {"error":  "<one line a person reads>",
//      "reason": "<code a machine branches on>",
//      "hint":   "<what to do instead>",    // optional but strongly encouraged
//      "see":    "<readme key>",            // optional doc pointer
//      ...extra}                            // facts specific to this refusal
//
// The machine field is `reason`, which ALREADY EXISTS — the call gate and every
// io leg answer `reason:"unauthorized"`. Nothing is renamed. What was missing is
// that most refusals carried NO machine field at all: `{"error": "create_agent:
// handler_module required"}` could only be branched on by matching prose.
//
// The set is CLOSED — a new code is a deliberate addition here, not an ad-hoc
// string at a call site.
//
// The `hint` is not decoration. Measured 2026-08-04: a model copied the literal
// placeholder `<id>` out of a prompt example, received `{"error": "no agent
// '<id>'"}` thirty-four times in one run, and never once corrected. The error
// stated a fact and withheld every fact that would have repaired it.

import FantasticJSON
import OrderedCollections

/// The closed refusal vocabulary. Each names a KIND of refusal, not a call
/// site — two verbs that fail for the same reason share a code.
public enum Refusal {
    /// The call gate said no (carries `side`).
    public static let unauthorized = "unauthorized"
    /// No agent by that id (carries `agents`).
    public static let unknownAgent = "unknown_agent"
    /// A required argument was absent (carries `arg`).
    public static let missingArg = "missing_arg"
    /// Present but the wrong type or value (carries `arg`).
    public static let badArg = "bad_arg"
    /// The bundle's own `validateMeta` refused.
    public static let invalidMeta = "invalid_meta"
    /// `delete_lock` (carries `id`).
    public static let locked = "locked"
    /// Right verb, wrong agent — kernel verbs are root-only.
    public static let notHere = "not_here"
    /// An id that is already taken (carries `id`).
    public static let exists = "exists"
    /// The agent has no such verb (carries `verb`).
    public static let unhandled = "unhandled"
    /// The named THING is absent — a path, a job, a key. NOT `unknownAgent`: the
    /// agent answered, and what it was asked for is not there. Python has carried
    /// this code since the set was written; the ports omitted it, so every bundle
    /// that needed it (exec_bridge's `no job`, a file bundle's missing path)
    /// answered a bare `{error}` and a caller branching on `reason` never matched.
    public static let notFound = "not_found"

    // Transport codes — a leg that could not carry the call at all. Owned by the
    // bridge/relay bundles; listed here because they share the field, and a
    // caller branching on `reason` must see the whole vocabulary in one place.

    /// The leg has no live connection.
    public static let notConnected = "not_connected"
    /// The leg failed mid-carry.
    public static let transportError = "transport_error"
    /// The leg closed under the call.
    public static let transportClosed = "transport_closed"
    /// The call outlived its deadline.
    public static let timeout = "timeout"

    /// The kernel is stopping; no new roots are accepted.
    public static let draining = "draining"
    /// `watch`: the watcher has no registered inbox, so nothing would read what
    /// is mirrored there. Registering a consumer is what `watch` requires, not
    /// what it does.
    public static let noConsumer = "no_consumer"

    /// Every legal code. A value outside this set is a programming error.
    public static let codes: Set<String> = [
        unauthorized, unknownAgent, missingArg, badArg, invalidMeta, locked,
        notHere, exists, unhandled, notFound, notConnected, transportError,
        transportClosed, timeout, draining, noConsumer,
    ]

    /// How many live ids an `unknownAgent` refusal carries — enough to name the
    /// legal values, bounded so a big tree cannot turn one typo into a context
    /// dump.
    public static let unknownAgentIdsMax = 40

    /// Build the one refusal shape.
    ///
    /// `code` must be one of ``codes`` — an unknown code traps, because the
    /// point of a closed set is that it cannot quietly grow at a call site.
    public static func make(
        _ code: String,
        _ error: String,
        hint: String = "",
        see: String = "",
        extra: [(String, JSON)] = []
    ) -> JSON {
        precondition(codes.contains(code), "unknown refusal code \(code)")
        var o: OrderedDictionary<String, JSON> = [:]
        o["error"] = .string(error)
        o["reason"] = .string(code)
        if !hint.isEmpty { o["hint"] = .string(hint) }
        if !see.isEmpty { o["see"] = .string(see) }
        for (k, v) in extra { o[k] = v }
        return .object(o)
    }

    /// Refuse an agent id that is not ONE SAFE PATH SEGMENT. `nil` when fine.
    ///
    /// An agent id is not just a name — it BECOMES A DIRECTORY. A loader builds
    /// a record's on-disk home by joining the id onto the store root, so an id
    /// carrying `..` or a separator walks out of the store and the write, or a
    /// recursive delete, lands wherever it pointed.
    ///
    /// Measured in python 2026-08-28: an agent holding NO explicit grant — only
    /// the structural baseline, which lets any child call its parent — sent
    /// `forget_record` with `id: "../../x"` to the root loader and recursively
    /// deleted a directory OUTSIDE the store. The gate was not bypassed;
    /// nothing validated the id.
    ///
    /// `arg` names the field when the segment is not an id: a record's
    /// `children_dir` becomes a directory too (`<agent>/<children_dir>/<child>`),
    /// so `create_agent` / `update_agent` hold it to the same rule (python
    /// `guard_agent_id(..., arg="children_dir")`).
    public static func guardAgentId(_ verb: String, _ agentId: String, arg: String = "id")
        -> JSON?
    {
        if agentId.isEmpty { return bad(verb, arg, "a non-empty string") }
        if agentId.contains("/") || agentId.contains("\\") {
            return bad(
                verb, arg, "a single path segment (no separators); got \(pyRepr(agentId))")
        }
        if agentId == "." || agentId == ".." || agentId.contains("..") {
            return bad(verb, arg, "a name that is not a path traversal; got \(pyRepr(agentId))")
        }
        if agentId.hasPrefix(".") {
            return bad(verb, arg, "a name that is not hidden; got \(pyRepr(agentId))")
        }
        // Message ids are plain integers; in prompt text an all-digit agent id
        // would be indistinguishable from a row id. Python `guard_agent_id`'s
        // `str.isdigit()` — any Unicode digit (`²`, `١٢`), not ASCII only.
        if pyIsDigitString(agentId) {
            return bad(
                verb, arg,
                "a name, not a number (message ids are ints); got \(pyRepr(agentId))")
        }
        // `|` qualifies a FOREIGN agent (`<guid>|<agent>`): a local id never
        // carries one, so a gate can tell local from foreign by the id alone.
        if agentId.contains("|") {
            return bad(
                verb, arg,
                "a local name without `|` (it marks a foreign `guid|agent`); got \(pyRepr(agentId))")
        }
        // `kernel` names the operator: sends to it reach the root, and a memory
        // store serves the operator under it. An agent holding that id would BE
        // the operator to every store (measured in python 2026-09-28).
        if agentId == reservedId {
            return bad(verb, arg, "a name other than the reserved \(pyRepr(reservedId))")
        }
        return nil
    }

    /// The one reserved agent id — the operator's name (send target, store identity).
    public static let reservedId = "kernel"

    /// `<verb>: <arg> required` — the commonest refusal, spelled once.
    public static func missing(_ verb: String, _ arg: String, hint: String = "") -> JSON {
        make(
            missingArg, "\(verb): \(arg) required", hint: hint,
            extra: [("arg", .string(arg))])
    }

    /// Present but unusable — say what was WANTED, not just that it was wrong.
    public static func bad(_ verb: String, _ arg: String, _ want: String) -> JSON {
        make(
            badArg, "\(verb): \(arg) must be \(want)",
            extra: [("arg", .string(arg))])
    }

    /// [`bad`], plus what was actually GIVEN — python's `f"{verb}: {arg} must
    /// be {want}, got {value!r}"` tail (C30/C38's `timeout`/`id`/`retries`
    /// refusals all carry it; `bad` alone omits it; `payload.type` names only a
    /// non-string's TYPE — see `BatchSend.launch`).
    public static func badGot(_ verb: String, _ arg: String, _ want: String, got: JSON) -> JSON {
        make(
            badArg, "\(verb): \(arg) must be \(want), got \(pyRepr(got))",
            extra: [("arg", .string(arg))])
    }

    /// Right verb, wrong agent. Kernel verbs act on the TREE, so the agent named
    /// is the OPERAND, not the actor — the refusal says where to send it.
    ///
    /// PYTHON'S PROSE, verbatim, plus the machine field. The runtimes said the
    /// same thing in different words on the same `not_here` code, and the ports
    /// dropped `verb` entirely — so a caller could branch on the code but not
    /// learn WHICH verb it had misaddressed without parsing the sentence.
    public static func notHere(_ verb: String, _ id: String) -> JSON {
        make(
            notHere,
            "\(verb) is a kernel verb — \(id) does not answer it",
            hint: "address the kernel root ('kernel'); name the agent you mean in "
                + "`parent_id` / `id`",
            extra: [("verb", .string(verb))]
        )
    }

    /// The reply for an id that names no agent — carrying the LIVE id index.
    ///
    /// Reading the flat index costs nothing, and a bare `no agent 'x'` teaches
    /// the caller nothing it can act on. NOT a repair and NOT a fallback: the
    /// call still fails, exactly once, by the one path — it just says what the
    /// legal ids are on the way out.
    public static func unknown(_ agentId: String, live: [String]) -> JSON {
        let sorted = live.sorted()
        let extra: [(String, JSON)] = [
            ("agent_count", .integer(Int64(sorted.count))),
            ("agents", .array(sorted.prefix(unknownAgentIdsMax).map { .string($0) })),
        ]
        var reply = make(unknownAgent, "no agent \(pyRepr(agentId))", extra: extra)
        // An id wrapped in angle brackets is structurally a SLOT, not a name —
        // no id this kernel mints can contain them, so this costs no guessing.
        //
        // Python assigns the hint AFTER the refusal is built, so it lands behind
        // `agent_count`/`agents`, not in `make`'s hint slot.
        if agentId.hasPrefix("<") && agentId.hasSuffix(">"),
            case .object(var o) = reply
        {
            o["hint"] = .string(
                "\(pyRepr(agentId)) is a PLACEHOLDER from an example, not an id — "
                    + "substitute one of `agents`.")
            reply = .object(o)
        }
        return reply
    }
}
