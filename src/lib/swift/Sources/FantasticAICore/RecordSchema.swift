// THE RECORD SCHEMA of an AI backend kind — what its record takes, readable with
// NO agent existing (owner, 2026-09-30, lib request #3). Python `ai_core/schema.py`.
//
// A set-up page asks the catalog (`reflect bundles=all`: each entry carries the
// bundle's `schema` when it publishes one — `AgentBundle.recordSchema`) and builds
// its inputs from it; it guesses nothing. Per field: `name`, `type`
// (`str|int|float|bool|enum|list|map|credentials`), `required`, `enum` (the legal
// values, for an `enum`), `default` (THE BACKEND'S OWN value, present only where
// the backend has one — a required field never has one: nothing is guessed), and a
// one-line `hint`. Keys never ride the record: a backend that sends one has a
// `credentials` field (type `credentials`, `{store, ref}` — the credential store
// and the ref it reads the key from at each request, `Credentials.swift`).
//
// ONE TRUTH. The same schema is what the backend's `validateMeta` enforces AT THE
// WRITE (`validate`): every required field is refused at `create_agent` /
// `update_agent` when absent, an `enum` field outside its values is refused, a
// `credentials` value that is not `{store, ref}` is refused, and a `system_prompt`
// that is not a non-blank string is refused (null clears it) — so the page and the kernel cannot disagree about what a record needs. A field's
// other checks (a positive int, a budget below `max_tokens`) stay in the
// backend's own `validateMeta`, beside this one.
//
// A backend's schema is its own fields followed by `AI_FIELDS` — the ones every
// AI-core agent takes. Built as OrderedDictionaries so the JSON is python's, key
// for key.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

public enum RecordSchema {
    /// The field types a page renders. Python `TYPES`.
    public static let TYPES = ["str", "int", "float", "bool", "enum", "list", "map", "credentials"]

    /// One field, in python's key order: `name, type, required, enum?, default?,
    /// hint`. `default` is omitted when the backend has none (a required field
    /// never has one); `enum` is present exactly when `type` is `enum`. A schema
    /// that breaks those rules is a programming error, caught the first time the
    /// schema is built — python raises `ValueError` at import.
    public static func field(
        _ name: String, _ type: String, _ hint: String,
        required: Bool = false, default dflt: JSON? = nil, enumValues: [String]? = nil
    ) -> JSON {
        precondition(TYPES.contains(type), "field \(name): type \(type) is not one of \(TYPES)")
        precondition(
            (type == "enum") == (enumValues != nil),
            "field \(name): `enum` values go with type 'enum', only")
        precondition(!(required && dflt != nil), "field \(name): a required field has no default")
        var out: OrderedDictionary<String, JSON> = [
            "name": .string(name), "type": .string(type), "required": .bool(required),
        ]
        if let enumValues { out["enum"] = .array(enumValues.map { .string($0) }) }
        if let dflt { out["default"] = dflt }
        out["hint"] = .string(hint)
        return .object(out)
    }

    /// The fields EVERY AI-core agent takes, whatever its backend. Python
    /// `AI_FIELDS`.
    public static let AI_FIELDS: [JSON] = aiFields(contextWindow: AIConstants.DEFAULT_CONTEXT_WINDOW)

    /// `AI_FIELDS` with `context_window` defaulting to `contextWindow` — the
    /// backend's own window (`AIBackendConfig.modelWindow`), where it can read one.
    public static func aiFields(contextWindow: Int64) -> [JSON] {
        [
            field(
                "memory_store_id", "str",
                "the memory store agent that keeps this agent's conversation (e.g. a ram_store)",
                required: true),
            field(
                "context_window", "int", "tokens the model sees per turn",
                default: .integer(contextWindow)),
            field(
                "output_reserve", "int", "tokens of the window held back for the model's reply",
                default: .integer(AIConstants.DEFAULT_OUTPUT_RESERVE)),
            field(
                "context_strategy", "enum", "how the conversation is fitted into the window",
                default: .string(AIStrategies.defaultStrategy),
                enumValues: AIStrategies.strategyNames()),
            field(
                "recent_n", "int",
                "recent turns a compaction keeps verbatim "
                    + "(\(AIConstants.RECENT_N_RANGE.0)–\(AIConstants.RECENT_N_RANGE.1))",
                default: .integer(AIConstants.RECENT_N)),
            field("system_prompt", "str", SYSTEM_PROMPT_HINT),
            field("file_bridge_id", "str", FILE_BRIDGE_ID_HINT),
            field("system_prompt_file", "str", SYSTEM_PROMPT_FILE_HINT),
            field("pre_system_prompt", "str", PRE_SYSTEM_PROMPT_HINT),
            field("post_system_prompt", "str", POST_SYSTEM_PROMPT_HINT),
        ]
    }

    /// The `pre_system_prompt` field's hint. Python `AI_FIELDS`.
    public static let PRE_SYSTEM_PROMPT_HINT =
        "text placed BEFORE the system block on every turn — whichever is in force "
        + "(the default, `system_prompt` or `system_prompt_file`), never replacing it; "
        + "verbatim, no placeholders; null = nothing"

    /// The `post_system_prompt` field's hint. Python `AI_FIELDS`.
    public static let POST_SYSTEM_PROMPT_HINT =
        "text placed AFTER the system block on every turn — whichever is in force, "
        + "never replacing it; verbatim, no placeholders; null = nothing"

    /// The refusal for a `pre_system_prompt` / `post_system_prompt` the record
    /// cannot use. Python `WRAP_PROMPT_REFUSAL.format(name=…)`.
    public static func wrapPromptRefusal(_ name: String) -> String {
        "\(name) must be a non-empty string (it wraps the system block on every turn) — "
            + "null clears it"
    }

    /// The `system_prompt` field's hint. Python `AI_FIELDS`.
    public static let SYSTEM_PROMPT_HINT =
        "a system block that REPLACES the default one on every turn of this agent "
        + "(a per-call `system_prompt` beats it for that delivery); placeholders {tree} "
        + "{self} {store} {howto} are filled each turn; null = the default"

    /// The `file_bridge_id` field's hint. Python `AI_FIELDS`.
    public static let FILE_BRIDGE_ID_HINT =
        "the file_bridge this agent reads files THROUGH (its `system_prompt_file`); "
        + "null = none, nothing is read"

    /// The `system_prompt_file` field's hint. Python `AI_FIELDS`.
    public static let SYSTEM_PROMPT_FILE_HINT =
        "a path on `file_bridge_id`, read through it as this agent's system block "
        + "(placeholders filled), re-read when the bridge reports it changed; a record "
        + "or per-call `system_prompt` beats it; null = none"

    /// The refusal for a `system_prompt` the record cannot use. Python
    /// `SYSTEM_PROMPT_REFUSAL`.
    public static let SYSTEM_PROMPT_REFUSAL =
        "system_prompt must be a non-empty string (it replaces the default system "
        + "block on every turn) — null clears it"

    /// The record's own `system_prompt` — a non-blank string (python
    /// `str.strip()`), else nil (the default block). Precedence at a turn: the
    /// per-call argument > this > the default block.
    public static func recordSystemPrompt(_ v: JSON?) -> String? {
        guard case .string(let s)? = v, !pyStrip(s).isEmpty else { return nil }
        return s
    }

    /// The `credentials` field of a backend that sends a key. Python
    /// `credentials_field(required=…)`.
    public static func credentialsField(required: Bool) -> JSON {
        field(
            "credentials", "credentials",
            "{store, ref}: the credential store and ref holding `api_key` — read at "
                + "each request, sent only to the ref's host"
                + (required ? "" : "; absent = no key (a keyless server)"),
            required: required)
    }

    /// A backend's whole record schema: its own fields, then `AI_FIELDS`. A backend
    /// field named like a shared one REPLACES it (vllm_backend: `context_window` has
    /// no default there — left out, it is read from the server). Python
    /// `schema(backend_fields)`.
    public static func schema(_ backendFields: [JSON]) -> JSON {
        schema(backendFields, contextWindow: AIConstants.DEFAULT_CONTEXT_WINDOW)
    }

    /// The same, for a backend whose model window is its own (`modelWindow`):
    /// `context_window` defaults to it.
    public static func schema(_ backendFields: [JSON], contextWindow: Int64) -> JSON {
        let own = Set(backendFields.compactMap { $0["name"].asString })
        let shared = aiFields(contextWindow: contextWindow).filter {
            !own.contains($0["name"].asString ?? "")
        }
        let fields = backendFields + shared
        let names = fields.compactMap { $0["name"].asString }
        precondition(Set(names).count == names.count, "duplicate field in schema: \(names)")
        return .object(["fields": .array(fields)])
    }

    /// Whether a record NAMES this field in a usable form — a string must be
    /// non-blank, anything else non-null. Python `_present`.
    static func present(_ f: JSON, _ v: JSON?) -> Bool {
        guard let v, v != .null else { return false }
        if f["type"].asString == "str" {
            guard case .string(let s) = v else { return false }
            return !pyStrip(s).isEmpty
        }
        return true
    }

    /// THE WRITE-TIME CHECK the schema implies, on the MERGED record: an `enum`
    /// field the write names must be one of its values, a `credentials` field it
    /// names must be `{store, ref}`, a `system_prompt` it names a non-empty string
    /// (or null), and every required field must be present. Every missing field is named at once, each with its hint,
    /// so one retry fixes the record. Python `validate`, its texts byte for byte.
    public static func validate(
        _ schema: JSON, backend: String,
        meta: OrderedDictionary<String, JSON>, record: OrderedDictionary<String, JSON>
    ) -> String? {
        let fields = schema["fields"].asArray ?? []
        for f in fields {
            guard let name = f["name"].asString, let v = meta[name], v != .null else { continue }
            if f["type"].asString == "enum" {
                let legal = f["enum"].asArray ?? []
                if !legal.contains(v) {
                    return "\(name)=\(pyRepr(v)) is not one of the legal values: "
                        + legal.map { $0.asString ?? pyRepr($0) }.joined(separator: ", ")
                }
            }
            if f["type"].asString == "credentials", let bad = credentialsShapeError(v) {
                return bad
            }
        }
        // `system_prompt` (every AI-core agent's): a non-blank string, or null to
        // clear it — never a blank or a non-string, which would read as "the
        // default" without saying so.
        if let v = meta["system_prompt"], v != .null, recordSystemPrompt(v) == nil {
            return SYSTEM_PROMPT_REFUSAL
        }
        for name in ["pre_system_prompt", "post_system_prompt"] {
            if let v = meta[name], v != .null, recordSystemPrompt(v) == nil {
                return wrapPromptRefusal(name)
            }
        }
        if let bad = PromptFile.shapeError(meta) {
            return bad
        }
        var merged = record
        for (k, v) in meta { merged[k] = v }
        let missing = fields.filter { f in
            f["required"].asBool == true && !present(f, merged[f["name"].asString ?? ""])
        }
        if !missing.isEmpty {
            let names = missing.map { $0["name"].asString ?? "" }
            return "\(backend): required on the record and missing: "
                + names.joined(separator: ", ") + " — nothing is guessed. "
                + missing.map { "\($0["name"].asString ?? "") = \($0["hint"].asString ?? "")" }
                .joined(separator: "; ")
        }
        return nil
    }
}
