//! VERBS DECLARED ONCE — the one place a verb's arguments are stated (issue #642).
//!
//! A bundle declares each verb as a struct (`#[derive(Deserialize, JsonSchema)]`,
//! `#[serde(deny_unknown_fields)]`): its fields are the arguments, `Option` or
//! `#[serde(default)]` for optional ones, `///` for descriptions; the struct's own
//! `///` says what the verb does. [`verbs!`] lists them (`VERBS` + `enum Call`),
//! [`record!`] declares the fields `create_agent` / `update_agent` may set.
//!
//! THE AGENT, NOT THE KERNEL, CHECKS THE CALL, AND THE GENERATED CODE OWNS THE
//! DISPATCH. A bundle implements [`Handler`](crate::bundle::Handler): its `handle`
//! takes the typed `Call` and nothing else, so no bundle can see a raw payload. The
//! `Call` the `verbs!` macro generates implements [`Parse`], the shared library
//! entry the kernel-facing [`Bundle`](crate::Bundle) runs BEFORE the handler: the
//! verb is in the list (builtin, from `verbs!`, or dynamic, from
//! [`Handler::dynamic_verbs`](crate::bundle::Handler::dynamic_verbs)), then each argument
//! is known, present when required and of its type — ONE engine, the JSON schema
//! ([`validate`]), no coercion: a wrongly typed value is refused by name and the
//! refusal is the agent's reply. A record field is known to the bundle's own record
//! declaration ([`check_record`]). `reflect` and `docs` render the same
//! declarations ([`line`], [`detail`]).

use serde_json::{json, Map, Value};
use std::sync::OnceLock;

use crate::refusal::{refusal_hint, with, BAD_ARG, MISSING_ARG, UNHANDLED};

#[doc(hidden)]
pub use schemars;
#[doc(hidden)]
pub use serde;
#[doc(hidden)]
pub use serde_json;

/// One declared verb: its name and its arguments' struct, kept as the JSON schema
/// (the struct's doc comment is the schema's `description`, each field's the
/// property's).
pub struct Verb {
    /// The verb's name — `payload["type"]`.
    pub name: &'static str,
    make: fn() -> Value,
    schema: OnceLock<Value>,
}

impl Verb {
    /// Declare `name` as the struct `T`. What [`verbs!`] calls.
    pub const fn of<T: schemars::JsonSchema>(name: &'static str) -> Verb {
        Verb {
            name,
            make: schema_of::<T>,
            schema: OnceLock::new(),
        }
    }

    /// The JSON schema of the verb's arguments (subschemas inlined), built once.
    pub fn schema(&self) -> &Value {
        self.schema.get_or_init(self.make)
    }

    /// What the verb does — the struct's doc comment.
    pub fn doc(&self) -> String {
        doc_of(self.schema())
    }
}

/// The fields a bundle's agents' RECORD may carry, declared as a struct like a
/// verb's arguments. What [`record!`] builds.
pub struct RecordDecl {
    make: fn() -> Value,
    schema: OnceLock<Value>,
    /// `true` when the record is freeform: any field is accepted (a bundle
    /// marks it explicitly — a script agent carries the fields its program reads).
    pub open: bool,
}

impl RecordDecl {
    /// Declare the record as the struct `T`.
    pub const fn of<T: schemars::JsonSchema>(open: bool) -> Self {
        RecordDecl {
            make: schema_of::<T>,
            schema: OnceLock::new(),
            open,
        }
    }

    /// The JSON schema of the record struct, built once.
    pub fn schema(&self) -> &Value {
        self.schema.get_or_init(self.make)
    }
}

fn schema_of<T: schemars::JsonSchema>() -> Value {
    let settings = schemars::gen::SchemaSettings::draft07().with(|s| {
        s.inline_subschemas = true;
    });
    let root = settings.into_generator().into_root_schema_for::<T>();
    serde_json::to_value(root).unwrap_or(Value::Null)
}

/// A verb that exists per AGENT INSTANCE, produced at runtime (an MCP client's
/// tools): the same schema shape as a builtin verb's, checked by the same engine.
#[derive(Debug, Clone, PartialEq)]
pub struct DynVerb {
    /// The verb's name — `payload["type"]`.
    pub name: String,
    /// The JSON schema of its arguments (`type: object`, `properties`, `required`);
    /// its `description` is what the verb does.
    pub schema: Value,
}

impl DynVerb {
    /// Declare a dynamic verb from a name, what it does and the JSON schema of its
    /// arguments (an MCP tool's `name`, `description`, `inputSchema`). An absent or
    /// non-object schema is a verb without arguments.
    #[must_use]
    pub fn new(name: impl Into<String>, description: &str, input_schema: &Value) -> DynVerb {
        let mut schema = match input_schema {
            Value::Object(o) => o.clone(),
            _ => Map::new(),
        };
        schema.insert("type".to_string(), json!("object"));
        schema.insert("description".to_string(), json!(description));
        DynVerb {
            name: name.into(),
            schema: Value::Object(schema),
        }
    }
}

/// A call of a dynamic verb, its arguments checked against the declared schema.
#[derive(Debug)]
pub struct DynCall {
    /// The verb's name.
    pub name: String,
    /// Its arguments (the payload without `type`).
    pub args: Value,
}

/// What a call resolved to.
#[doc(hidden)]
#[derive(Debug)]
pub enum Resolved {
    /// A builtin verb of the bundle (or a common one): its name, arguments checked.
    Builtin(&'static str, Value),
    /// A dynamic verb of the agent.
    Dynamic(DynCall),
}

/// What an agent's entry made of a call: the typed call for its handler, or the
/// reply the shared code gives itself (`boot` and `shutdown` of an agent that
/// declares neither: nothing to open or release).
#[derive(Debug)]
pub enum Routed<C> {
    /// The call, its arguments checked and typed: the handler's.
    Call(C),
    /// The entry answered; no handler runs.
    Answered(Value),
}

/// THE ENTRY of every call to a bundle's agents, implemented by the `Call` that
/// [`verbs!`] generates: the verb is looked up (builtin, common, dynamic), its
/// arguments are checked against the declaration and typed. `Err` is the agent's
/// reply. The kernel-facing [`Bundle`](crate::Bundle) runs it before the handler, so
/// a handler only ever sees a `Call`.
pub trait Parse: Sized + Send + 'static {
    /// Parse `payload` as a call to the agent `agent`, whose verbs built at runtime
    /// are `dynamic`.
    fn parse(agent: &str, payload: &Value, dynamic: &[DynVerb]) -> Result<Routed<Self>, Value>;
}

/// Declare a bundle's verbs: `verbs! { "name" => Struct, … }` generates
/// `pub static VERBS`, `pub enum Call` with one variant per struct (named like it)
/// and its ENTRY, `impl Parse for Call`: the shared parse + validation that replies
/// with the refusal when the call is not one of the verbs. A bundle with no dynamic
/// verbs also gets `dynamic_verbs(&agent)`, an explicit empty list.
///
/// `verbs! { dynamic; "name" => Struct, … }` is the form for a bundle whose agents
/// also carry verbs built at runtime: `Call` has a `Dynamic(DynCall)` variant and
/// the bundle writes its own `dynamic_verbs`.
#[macro_export]
macro_rules! verbs {
    // The kernel's own lists (`declared`): no `Reflect` variant added, nothing
    // dynamic; `Call::parse(agent, payload)` answers `Result<Call, refusal>`.
    (@own $($name:literal => $ty:ident),+ $(,)?) => {
        $crate::verbs!(
            @list args
            [&[VERBS]]
            [] []
            $($name => $ty),+
        );
        impl Call {
            /// THE ENTRY of every call to the kernel's own verbs (it is an agent
            /// too): resolve the verb (unknown ⇒ the refusal listing what it
            /// answers), check its arguments against the declaration, then type
            /// them. `Err` is the kernel's reply.
            pub fn parse(
                agent: &str,
                payload: &$crate::verbs::serde_json::Value,
            ) -> Result<Call, $crate::verbs::serde_json::Value> {
                let (vname, args) = $crate::verbs::resolve_builtin(agent, payload, &[VERBS])?;
                Self::arm(agent, payload, vname, args)
            }
        }
    };
    // A bundle's list with verbs built at runtime.
    (dynamic; $($name:literal => $ty:ident),+ $(,)?) => {
        $crate::verbs!(
            @list args
            [&[VERBS, $crate::declared::common::VERBS]]
            [
                /// The `reflect` verb every agent answers.
                Reflect($crate::declared::common::Reflect),
                /// A verb built at runtime for this agent.
                Dynamic($crate::verbs::DynCall),
            ]
            [
                "reflect" => Self::typed::<$crate::declared::common::Reflect>("reflect", args).map(Call::Reflect),
            ]
            $($name => $ty),+
        );
        impl $crate::verbs::Parse for Call {
            fn parse(
                agent: &str,
                payload: &$crate::verbs::serde_json::Value,
                dynamic: &[$crate::verbs::DynVerb],
            ) -> Result<$crate::verbs::Routed<Call>, $crate::verbs::serde_json::Value> {
                match $crate::verbs::resolve(
                    agent,
                    payload,
                    &[VERBS, $crate::declared::common::VERBS],
                    dynamic,
                )? {
                    $crate::verbs::Resolved::Dynamic(call) => {
                        Ok($crate::verbs::Routed::Call(Call::Dynamic(call)))
                    }
                    $crate::verbs::Resolved::Builtin(vname, args) => {
                        Self::route(agent, payload, vname, args)
                    }
                }
            }
        }
    };
    ($($name:literal => $ty:ident),+ $(,)?) => {
        $crate::verbs!(
            @list args
            [&[VERBS, $crate::declared::common::VERBS]]
            [
                /// The `reflect` verb every agent answers.
                Reflect($crate::declared::common::Reflect),
            ]
            [
                "reflect" => Self::typed::<$crate::declared::common::Reflect>("reflect", args).map(Call::Reflect),
            ]
            $($name => $ty),+
        );
        impl $crate::verbs::Parse for Call {
            fn parse(
                agent: &str,
                payload: &$crate::verbs::serde_json::Value,
                _dynamic: &[$crate::verbs::DynVerb],
            ) -> Result<$crate::verbs::Routed<Call>, $crate::verbs::serde_json::Value> {
                let (vname, args) = $crate::verbs::resolve_builtin(
                    agent,
                    payload,
                    &[VERBS, $crate::declared::common::VERBS],
                )?;
                Self::route(agent, payload, vname, args)
            }
        }
        /// This bundle's agents carry no verbs built at runtime: an explicit empty list
        /// (what `Handler::dynamic_verbs` returns).
        #[must_use]
        pub fn dynamic_verbs(_agent: &$crate::Agent) -> Vec<$crate::verbs::DynVerb> {
            Vec::new()
        }
    };
    (@list $args:ident
        [$lists:expr]
        [$($variant:tt)*] [$($arm:tt)*]
        $($name:literal => $ty:ident),+) => {
        /// Every verb this bundle declares: the list its entry validates a call
        /// against and `reflect` and `docs` render.
        pub static VERBS: &[$crate::verbs::Verb] = &__DECLARED_VERBS;

        #[doc(hidden)]
        static __DECLARED_VERBS: [$crate::verbs::Verb; [$($name),+].len()] = [
            $( $crate::verbs::Verb::of::<$ty>($name) ),+
        ];

        /// A call of one of this bundle's verbs, its arguments typed.
        #[derive(Debug)]
        #[allow(dead_code)]
        pub enum Call {
            $($variant)*
            $(
                #[doc = concat!("The `", $name, "` verb.")]
                $ty($ty)
            ),+
        }

        impl Call {
            fn typed<T: $crate::verbs::serde::de::DeserializeOwned>(
                verb: &str,
                args: $crate::verbs::serde_json::Value,
            ) -> Result<T, $crate::verbs::serde_json::Value> {
                $crate::verbs::serde_json::from_value::<T>(args)
                    .map_err(|e| $crate::verbs::unparsed(verb, &e.to_string()))
            }

            // The call of a resolved builtin verb, typed. `boot` and `shutdown` an
            // agent does not declare are answered by the shared entry itself.
            #[allow(dead_code)]
            fn route(
                agent: &str,
                payload: &$crate::verbs::serde_json::Value,
                vname: &str,
                $args: $crate::verbs::serde_json::Value,
            ) -> Result<$crate::verbs::Routed<Call>, $crate::verbs::serde_json::Value> {
                if matches!(vname, "boot" | "shutdown") && !VERBS.iter().any(|v| v.name == vname) {
                    return Ok($crate::verbs::Routed::Answered($crate::verbs::serde_json::Value::Null));
                }
                Self::arm(agent, payload, vname, $args).map($crate::verbs::Routed::Call)
            }

            fn arm(
                agent: &str,
                payload: &$crate::verbs::serde_json::Value,
                vname: &str,
                $args: $crate::verbs::serde_json::Value,
            ) -> Result<Call, $crate::verbs::serde_json::Value> {
                match vname {
                    $($arm)*
                    $(
                        $name => Self::typed::<$ty>($name, $args).map(Call::$ty),
                    )+
                    _ => Err($crate::verbs::unknown_verb(
                        agent,
                        payload.get("type").unwrap_or(&$crate::verbs::serde_json::Value::Null),
                        &$crate::verbs::names($lists, &[]),
                    )),
                }
            }
        }
    };
}

/// Declare a bundle's record: `record!(Struct)` generates `pub static RECORD`
/// (the fields `create_agent` / `update_agent` may set on its agents);
/// `record!(Struct, open)` marks it freeform — any field is accepted.
#[macro_export]
macro_rules! record {
    ($ty:ident) => {
        /// The fields an agent of this bundle's record may carry.
        pub static RECORD: $crate::verbs::RecordDecl = $crate::verbs::RecordDecl::of::<$ty>(false);
    };
    ($ty:ident, open) => {
        /// The fields an agent of this bundle's record may carry (freeform).
        pub static RECORD: $crate::verbs::RecordDecl = $crate::verbs::RecordDecl::of::<$ty>(true);
    };
}

/// The payload without its `type`: the arguments alone.
#[doc(hidden)]
#[must_use]
pub fn args_of(payload: &Value) -> Value {
    let mut obj = payload.as_object().cloned().unwrap_or_default();
    obj.shift_remove("type");
    Value::Object(obj)
}

// ─── reading a schema ───────────────────────────────────────────────────────

/// One argument (or record field) as a declaration states it.
#[derive(Debug, Clone, PartialEq)]
pub struct Arg {
    /// Field name.
    pub name: String,
    /// Its type, rendered (`string`, `int`, `list[string]`, `'a'|'b'`, `any`).
    pub ty: String,
    /// Must the call carry it?
    pub required: bool,
    /// Its default, when the declaration gives one.
    pub default: Option<Value>,
    /// Its `///` description.
    pub doc: String,
}

fn doc_of(schema: &Value) -> String {
    schema
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

fn type_label(s: &Value) -> String {
    let mut label = base_label(s);
    if let Some(n) = s.get("exclusiveMinimum").and_then(Value::as_f64) {
        label.push_str(&format!(" > {n}"));
    }
    // an unsigned integer's own floor, 0, says nothing a reader needs
    if let Some(n) = s
        .get("minimum")
        .and_then(Value::as_f64)
        .filter(|n| *n != 0.0)
    {
        label.push_str(&format!(" >= {n}"));
    }
    if let Some(n) = s.get("minItems").and_then(Value::as_u64) {
        label.push_str(&format!(" with at least {n} item(s)"));
    }
    label
}

fn base_label(s: &Value) -> String {
    let Some(o) = s.as_object() else {
        return "any".to_string();
    };
    if let Some(Value::Array(vals)) = o.get("enum") {
        let names: Vec<String> = vals
            .iter()
            .map(|v| match v {
                Value::String(t) => format!("'{t}'"),
                other => other.to_string(),
            })
            .collect();
        return names.join("|");
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(Value::Array(alts)) = o.get(key) {
            let labels: Vec<String> = alts
                .iter()
                .filter(|a| a.get("type").and_then(Value::as_str) != Some("null"))
                .map(type_label)
                .collect();
            if !labels.is_empty() {
                return labels.join("|");
            }
        }
    }
    let one = |t: &str| -> String {
        match t {
            "string" => "string".to_string(),
            "integer" => "int".to_string(),
            "number" => "number".to_string(),
            "boolean" => "bool".to_string(),
            "array" => format!(
                "list[{}]",
                o.get("items").map_or_else(|| "any".to_string(), type_label)
            ),
            "object" => "object".to_string(),
            other => other.to_string(),
        }
    };
    match o.get("type") {
        Some(Value::String(t)) => one(t),
        Some(Value::Array(ts)) => {
            let labels: Vec<String> = ts
                .iter()
                .filter_map(Value::as_str)
                .filter(|t| *t != "null")
                .map(one)
                .collect();
            if labels.is_empty() {
                "any".to_string()
            } else {
                labels.join("|")
            }
        }
        _ => "any".to_string(),
    }
}

/// The arguments (or fields) a schema declares, in declaration order.
#[must_use]
pub fn args(schema: &Value) -> Vec<Arg> {
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        return Vec::new();
    };
    props
        .iter()
        .map(|(name, p)| Arg {
            name: name.clone(),
            ty: type_label(p),
            required: required.contains(&name.as_str()),
            default: p.get("default").filter(|d| !d.is_null()).cloned(),
            doc: doc_of(p),
        })
        .collect()
}

fn arg_text(a: &Arg) -> String {
    let mut s = format!("{}:{}", a.name, a.ty);
    if a.required {
        s.push_str(" (req)");
    } else if let Some(d) = &a.default {
        s.push_str(&format!(" (default {d})"));
    } else {
        s.push('?');
    }
    s
}

/// The argument list, one line: `id:string (req), tier:'a'|'b'?` — `No args` when
/// the verb takes none.
#[must_use]
pub fn arg_list(schema: &Value) -> String {
    let a = args(schema);
    if a.is_empty() {
        return "No args".to_string();
    }
    format!(
        "args: {}",
        a.iter().map(arg_text).collect::<Vec<_>>().join(", ")
    )
}

/// Does the schema accept fields it does not name? Only `deny_unknown_fields`
/// (`additionalProperties: false`) closes a struct.
fn open_ended(schema: &Value) -> bool {
    !matches!(schema.get("additionalProperties"), Some(Value::Bool(false)))
}

/// The FIRST SENTENCE of a doc: cut at its first `. ` or end of the first paragraph.
fn first_sentence(doc: &str) -> String {
    let para = doc.split("\n\n").next().unwrap_or("");
    let line = para
        .split('\n')
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(" ");
    let mut depth = 0usize;
    let chars: Vec<char> = line.chars().collect();
    for i in 0..chars.len() {
        match chars[i] {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            '.' if depth == 0 && chars.get(i + 1) == Some(&' ') => {
                return chars[..i].iter().collect();
            }
            _ => {}
        }
    }
    line.trim_end_matches('.').to_string()
}

/// One verb as `reflect` and `docs` show it: its name and the schema of its
/// arguments — a builtin's (from `verbs!`) or a dynamic one's, rendered alike.
#[derive(Debug, Clone, Copy)]
pub struct Row<'a> {
    /// The verb's name.
    pub name: &'a str,
    /// The JSON schema of its arguments.
    pub schema: &'a Value,
}

/// The rows of the declared lists in order, then the dynamic ones; a verb named in
/// an earlier list is not repeated from a later one.
#[must_use]
pub fn rows<'a>(lists: &[&'a [Verb]], dynamic: &'a [DynVerb]) -> Vec<Row<'a>> {
    let mut out: Vec<Row<'a>> = Vec::new();
    for l in lists {
        for v in l.iter() {
            if !out.iter().any(|r| r.name == v.name) {
                out.push(Row {
                    name: v.name,
                    schema: v.schema(),
                });
            }
        }
    }
    for d in dynamic {
        if !out.iter().any(|r| r.name == d.name) {
            out.push(Row {
                name: &d.name,
                schema: &d.schema,
            });
        }
    }
    out
}

/// The verb names of the lists, then the dynamic ones, each once.
#[must_use]
pub fn names<'a>(lists: &[&'a [Verb]], dynamic: &'a [DynVerb]) -> Vec<&'a str> {
    rows(lists, dynamic).into_iter().map(|r| r.name).collect()
}

/// The ONE-LINE tier of a verb: its arguments, then what it does.
#[must_use]
pub fn line(row: &Row) -> String {
    let doc = first_sentence(&doc_of(row.schema));
    let list = arg_list(row.schema);
    if doc.is_empty() {
        list
    } else {
        format!("{list} — {doc}")
    }
}

/// The DETAILED tier of a verb: the whole doc, then every argument with its type,
/// whether it is required or its default, and its description.
#[must_use]
pub fn detail(row: &Row) -> String {
    let mut out = doc_of(row.schema);
    let a = args(row.schema);
    if a.is_empty() {
        out.push_str("\n\nNo args.");
        return out.trim().to_string();
    }
    out.push_str("\n\nargs:");
    for x in &a {
        out.push_str(&format!("\n  {}", arg_text(x)));
        if !x.doc.is_empty() {
            out.push_str(&format!(" — {}", x.doc.replace('\n', " ")));
        }
    }
    out.trim().to_string()
}

/// `{verb: one line}` for a list of rows — what `reflect` carries as its `verbs` block.
#[must_use]
pub fn lines(rows: &[Row]) -> Value {
    let mut m = Map::new();
    for r in rows {
        m.insert(r.name.to_string(), Value::String(line(r)));
    }
    Value::Object(m)
}

// ─── checking a call ────────────────────────────────────────────────────────

fn matches_type(schema: &Value, v: &Value) -> bool {
    let Some(o) = schema.as_object() else {
        return true;
    };
    if let Some(Value::Array(vals)) = o.get("enum") {
        return vals.contains(v);
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(Value::Array(alts)) = o.get(key) {
            return alts.iter().any(|a| matches_type(a, v));
        }
    }
    let within = |n: &serde_json::Number| -> bool {
        let f = n.as_f64().unwrap_or(f64::NAN);
        o.get("minimum")
            .and_then(Value::as_f64)
            .is_none_or(|m| f >= m)
            && o.get("maximum")
                .and_then(Value::as_f64)
                .is_none_or(|m| f <= m)
            && o.get("exclusiveMinimum")
                .and_then(Value::as_f64)
                .is_none_or(|m| f > m)
    };
    let one = |t: &str| -> bool {
        match t {
            "string" => v.as_str().is_some_and(|t| {
                o.get("minLength")
                    .and_then(Value::as_u64)
                    .is_none_or(|m| t.chars().count() as u64 >= m)
            }),
            "boolean" => v.is_boolean(),
            "null" => v.is_null(),
            "integer" => v.as_number().is_some_and(|n| {
                (n.is_i64() || n.is_u64() || fantastic_bundle::py_type_name(v) == "int")
                    && within(n)
            }),
            "number" => v.as_number().is_some_and(within),
            "array" => match (v, o.get("items")) {
                (Value::Array(items), it) => {
                    o.get("minItems")
                        .and_then(Value::as_u64)
                        .is_none_or(|m| items.len() as u64 >= m)
                        && it.is_none_or(|it| items.iter().all(|x| matches_type(it, x)))
                }
                _ => false,
            },
            "object" => v.is_object(),
            _ => true,
        }
    };
    match o.get("type") {
        Some(Value::String(t)) => one(t),
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).any(one),
        _ => true,
    }
}

fn refuse_arg(verb: &str, code: &str, arg: &str, why: String, schema: &Value) -> Value {
    // the hint: the verb's real argument list, and what the named argument is
    let mut hint = format!("{verb} takes — {}", arg_list(schema));
    if let Some(a) = args(schema).into_iter().find(|a| a.name == arg) {
        if !a.doc.is_empty() {
            hint.push_str(&format!(". {arg}: {}", a.doc.replace('\n', " ")));
        }
    }
    let mut r = refusal_hint(code, format!("{verb}: {why}"), hint);
    with(&mut r, "arg", json!(arg));
    with(&mut r, "verb", json!(verb));
    r
}

/// Check the ARGUMENTS of one call against a schema — the ONE validation engine,
/// builtin and dynamic verbs alike. `Ok` carries the arguments; `Err` the refusal, by
/// argument name, with the verb's real argument list. NO coercion: a value of the
/// wrong type is refused, never typed on the caller's behalf.
///
/// Order: unknown → missing → wrongly typed.
pub fn validate(name: &str, schema: &Value, payload: &Value) -> Result<Value, Value> {
    let Value::Object(raw) = args_of(payload) else {
        return Ok(Value::Object(Map::new()));
    };
    check_object(name, "", schema, &raw, schema)?;
    Ok(Value::Object(raw))
}

fn bound_of(schema: &Value, name: &str, key: &str) -> u64 {
    schema
        .get("properties")
        .and_then(|p| p.get(name))
        .and_then(|p| p.get(key))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

/// Is a required argument ABSENT: not sent, null, or an empty text where the
/// declaration asks for a non-empty one, or an empty list where it asks for at
/// least one item (python's falsy rule).
fn is_missing(schema: &Value, name: &str, value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) if s.is_empty() => bound_of(schema, name, "minLength") >= 1,
        Some(Value::Array(a)) if a.is_empty() => bound_of(schema, name, "minItems") >= 1,
        Some(_) => false,
    }
}

/// Does the schema describe an object with declared properties (so its fields are
/// checked by name, not just "is an object")?
fn described_object(schema: &Value) -> Option<&Value> {
    let is = |s: &Value| {
        let object = match s.get("type") {
            Some(Value::String(t)) => t == "object",
            Some(Value::Array(ts)) => ts.iter().any(|t| t.as_str() == Some("object")),
            _ => false,
        };
        object && (s.get("properties").is_some() || s.get("required").is_some())
    };
    if is(schema) {
        return Some(schema);
    }
    // an optional object: `anyOf` the object and null
    ["anyOf", "oneOf"]
        .iter()
        .filter_map(|k| schema.get(*k).and_then(Value::as_array))
        .flatten()
        .find(|alt| is(alt))
}

/// One object against its schema: unknown → missing → wrongly typed, a nested
/// object checked the same way under its dotted path (`record.id`). `top` is the
/// verb's own schema, for the hint.
fn check_object(
    name: &str,
    path: &str,
    schema: &Value,
    raw: &Map<String, Value>,
    top: &Value,
) -> Result<(), Value> {
    let dotted = |k: &str| {
        if path.is_empty() {
            k.to_string()
        } else {
            format!("{path}.{k}")
        }
    };
    let props = schema.get("properties").and_then(Value::as_object);
    let open = open_ended(schema);
    for k in raw.keys() {
        let known = props.is_some_and(|p| p.contains_key(k));
        if !known && !open {
            return Err(refuse_arg(
                name,
                BAD_ARG,
                &dotted(k),
                format!("unknown argument {}", fantastic_bundle::py_repr(&dotted(k))),
                top,
            ));
        }
    }
    if let Some(Value::Array(req)) = schema.get("required") {
        for r in req.iter().filter_map(Value::as_str) {
            if is_missing(schema, r, raw.get(r)) {
                return Err(refuse_arg(
                    name,
                    MISSING_ARG,
                    &dotted(r),
                    format!("missing argument {}", fantastic_bundle::py_repr(&dotted(r))),
                    top,
                ));
            }
        }
    }
    if let Some(props) = props {
        for (k, v) in raw {
            let Some(p) = props.get(k) else { continue };
            if let (Some(obj), Value::Object(inner)) = (described_object(p), v) {
                check_object(name, &dotted(k), obj, inner, top)?;
                continue;
            }
            if !matches_type(p, v) {
                return Err(refuse_arg(
                    name,
                    BAD_ARG,
                    &dotted(k),
                    format!(
                        "argument {} must be {}, got {}",
                        fantastic_bundle::py_repr(&dotted(k)),
                        type_label(p),
                        fantastic_bundle::py_type_name(v)
                    ),
                    top,
                ));
            }
        }
    }
    Ok(())
}

/// The typed form of a call an agent's entry ALREADY checked (the agent answered
/// it): the arguments, decoded into the declared struct. A decode that fails means
/// the struct and its schema disagree.
pub fn decode<T: serde::de::DeserializeOwned>(verb: &str, payload: &Value) -> Result<T, Value> {
    serde_json::from_value::<T>(args_of(payload)).map_err(|e| unparsed(verb, &e.to_string()))
}

/// The refusal for arguments the schema accepted and the declared struct could not
/// decode: the struct and its schema disagree, which is a bug in the declaration,
/// never a second round of validation.
#[doc(hidden)]
#[must_use]
pub fn unparsed(verb: &str, why: &str) -> Value {
    refusal_hint(
        BAD_ARG,
        format!("{verb}: the arguments do not parse ({why})"),
        format!("docs of=<agent> detailed verb={verb} lists the arguments"),
    )
}

/// `unknown verb 'X'; <agent> answers: a, b, c` — the refusal for a verb not in the
/// list. `verb` is the call's raw `type` (`null` when absent), echoed as sent.
/// `answers` are the verb names the agent takes.
#[must_use]
pub fn unknown_verb(agent: &str, verb: &Value, answers: &[&str]) -> Value {
    let mut r = refusal_hint(
        UNHANDLED,
        format!(
            "unknown verb {}; {agent} answers: {}",
            fantastic_bundle::py_repr_value(verb),
            answers.join(", ")
        ),
        format!("docs of={agent} lists each verb's arguments"),
    );
    with(&mut r, "verb", verb.clone());
    r
}

fn verb_name(payload: &Value) -> Option<&str> {
    payload.get("type").and_then(Value::as_str)
}

/// Resolve a call among BUILTIN verbs only (searched in order — the bundle's first,
/// then the verbs every agent answers) and check its arguments: the verb's name and
/// the arguments. The entry `verbs!` generates for a bundle with no dynamic verbs.
pub fn resolve_builtin(
    agent: &str,
    payload: &Value,
    lists: &[&'static [Verb]],
) -> Result<(&'static str, Value), Value> {
    let found =
        verb_name(payload).and_then(|n| lists.iter().find_map(|l| l.iter().find(|v| v.name == n)));
    let Some(v) = found else {
        return Err(unknown_verb(
            agent,
            payload.get("type").unwrap_or(&Value::Null),
            &names(lists, &[]),
        ));
    };
    Ok((v.name, validate(v.name, v.schema(), payload)?))
}

/// Resolve a call among the builtin verbs and the agent's DYNAMIC ones (a builtin
/// of the same name wins) and check its arguments with the same engine. The entry
/// `verbs!{ dynamic; … }` generates.
pub fn resolve(
    agent: &str,
    payload: &Value,
    lists: &[&'static [Verb]],
    dynamic: &[DynVerb],
) -> Result<Resolved, Value> {
    let name = verb_name(payload);
    if let Some(n) = name {
        if lists.iter().any(|l| l.iter().any(|v| v.name == n)) {
            let (vname, args) = resolve_builtin(agent, payload, lists)?;
            return Ok(Resolved::Builtin(vname, args));
        }
        if let Some(d) = dynamic.iter().find(|d| d.name == n) {
            let args = validate(&d.name, &d.schema, payload)?;
            return Ok(Resolved::Dynamic(DynCall {
                name: d.name.clone(),
                args,
            }));
        }
    }
    Err(unknown_verb(
        agent,
        payload.get("type").unwrap_or(&Value::Null),
        &names(lists, dynamic),
    ))
}

/// Check the FIELDS of a record patch (`create_agent` / `update_agent`) against what
/// the kernel declares for every record (`common`) and the bundle's own record
/// (`own`) — the SHARED ENTRY a bundle's [`Bundle::check_record`](crate::Bundle::check_record)
/// runs, so the target's bundle judges the fields, never the kernel.
///
/// First the SUBSTRATE WIRING no record may carry (`root_path`, `ctx`, `parent`, and
/// at update `handler_module`, fixed at create, and `parent_id`, the tree's) and the `children_dir` rule
/// (one safe path segment). Then a field neither record names is refused by name,
/// with both lists; a known one must be of its type. A freeform record (`own.open`)
/// accepts any field the common record does not own.
pub fn check_record(
    verb: &str,
    bundle: &str,
    fields: &Map<String, Value>,
    common: &RecordDecl,
    own: &RecordDecl,
) -> Result<(), Value> {
    // REFUSED, not silently skipped — python's `for k in ("handler_module",
    // *sorted(RESERVED_RECORD_KEYS)):`. `handler_module` is fixed at create:
    // swapping it would leave a live agent whose in-process state belongs to the OLD
    // bundle. The reserved keys name substrate wiring; dropping them without a word
    // would let the caller believe a change had landed that never did.
    let mut wiring: Vec<&str> = crate::agent::RESERVED_RECORD_KEYS.to_vec();
    wiring.sort_unstable();
    if verb == "update_agent" {
        wiring.insert(0, "parent_id");
        wiring.insert(0, "handler_module");
    }
    for k in wiring {
        if fields.contains_key(k) {
            let why = if k == "parent_id" {
                "absent — parent_id is structural: where an agent sits is the \
                 tree's, never its record's"
            } else if k == "handler_module" {
                "absent — handler_module is fixed at create_agent, and \
                 root_path/ctx/parent name substrate wiring the record may \
                 not carry"
            } else {
                "absent — root_path/ctx/parent name substrate wiring the \
                 record may not carry"
            };
            return Err(crate::refusal::bad(verb, k, why));
        }
    }
    // `children_dir` BECOMES A DIRECTORY (`<agent>/<children_dir>/<child>`): one safe
    // segment, exactly as an id is. A non-string is judged as "".
    if let Some(cd) = fields.get("children_dir") {
        if let Some(r) =
            crate::refusal::guard_agent_id_as(verb, "children_dir", cd.as_str().unwrap_or(""))
        {
            return Err(r);
        }
    }
    let (cs, os) = (common.schema(), own.schema());
    let cprops = cs.get("properties").and_then(Value::as_object);
    let oprops = os.get("properties").and_then(Value::as_object);
    let list = || {
        let mut all: Vec<Arg> = args(cs);
        all.extend(args(os));
        all.iter().map(arg_text).collect::<Vec<_>>().join(", ")
    };
    let mut mine = Map::new();
    let mut theirs = Map::new();
    for (k, v) in fields {
        if cprops.is_some_and(|p| p.contains_key(k)) {
            theirs.insert(k.clone(), v.clone());
        } else if oprops.is_some_and(|p| p.contains_key(k)) || own.open {
            mine.insert(k.clone(), v.clone());
        } else {
            let mut r = refusal_hint(
                BAD_ARG,
                format!(
                    "{verb}: {bundle} has no record field {}",
                    fantastic_bundle::py_repr(k)
                ),
                format!("a {bundle} record takes — {}", list()),
            );
            with(&mut r, "arg", json!(k));
            return Err(r);
        }
    }
    for (schema, part) in [(cs, theirs), (os, mine)] {
        for (k, v) in &part {
            let Some(p) = schema.get("properties").and_then(|p| p.get(k)) else {
                continue;
            };
            // a field that is itself a declared object is checked the same way,
            // by its own fields (`credentials.store`)
            if let (Some(obj), Value::Object(inner)) = (described_object(p), v) {
                check_object(verb, k, obj, inner, schema)?;
                continue;
            }
            if !matches_type(p, v) {
                let mut r = refusal_hint(
                    BAD_ARG,
                    format!(
                        "{verb}: record field {} must be {}, got {}",
                        fantastic_bundle::py_repr(k),
                        type_label(p),
                        fantastic_bundle::py_type_name(v)
                    ),
                    format!("a {bundle} record takes — {}", list()),
                );
                with(&mut r, "arg", json!(k));
                return Err(r);
            }
        }
    }
    // a record is BORN with the fields its bundle requires
    if verb == "create_agent" {
        for schema in [cs, os] {
            if let Some(Value::Array(req)) = schema.get("required") {
                for r in req.iter().filter_map(Value::as_str) {
                    if matches!(fields.get(r), None | Some(Value::Null)) {
                        let mut refused = refusal_hint(
                            MISSING_ARG,
                            format!(
                                "{verb}: a {bundle} record needs field {}",
                                fantastic_bundle::py_repr(r)
                            ),
                            format!("a {bundle} record takes — {}", list()),
                        );
                        with(&mut refused, "arg", json!(r));
                        return Err(refused);
                    }
                }
            }
        }
    }
    Ok(())
}

/// The record's fields as the catalog shape (`{fields:[{name, type, required,
/// default?, hint}]}`) `reflect bundles=all` lists per bundle.
#[must_use]
pub fn record_fields(decl: &RecordDecl) -> Value {
    let fields: Vec<Value> = args(decl.schema())
        .into_iter()
        .map(|a| {
            let mut f =
                json!({"name": a.name, "type": a.ty, "required": a.required, "hint": a.doc});
            if let Some(d) = a.default {
                f["default"] = d;
            }
            f
        })
        .collect();
    json!({ "fields": fields, "open": decl.open })
}
