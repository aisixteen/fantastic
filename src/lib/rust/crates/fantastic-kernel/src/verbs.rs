//! VERBS DECLARED ONCE — the one place a verb's arguments are stated (issue #642).
//!
//! A bundle declares each verb as a struct (`#[derive(Deserialize, JsonSchema)]`,
//! `#[serde(deny_unknown_fields)]`): its fields are the arguments, `Option` or
//! `#[serde(default)]` for optional ones, `///` for descriptions; the struct's own
//! `///` says what the verb does. [`verbs!`] lists them (`VERBS` + `enum Call`),
//! [`record!`] declares the fields `create_agent` / `update_agent` may set.
//!
//! THE KERNEL, NOT THE BUNDLE, CHECKS THE CALL — at dispatch, for every caller
//! (model, CLI, another agent): the verb is in the list, then each argument is
//! known, present when required and of its type ([`check_call`]); a record field is
//! known to the bundle ([`check_record`]). `reflect` and `docs` render the same
//! declarations ([`line`], [`detail`]). A bundle contains no validation code: its
//! `handle` parses the already-checked payload with [`Call::parse`] — the generated
//! one, which cannot fail after the kernel's check.

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
/// property's) and a typed parse of the payload into that struct.
pub struct Verb {
    /// The verb's name — `payload["type"]`.
    pub name: &'static str,
    make: fn() -> Value,
    parse: fn(Value) -> Result<(), String>,
    schema: OnceLock<Value>,
}

impl Verb {
    /// Declare `name` as the struct `T`. What [`verbs!`] calls.
    pub const fn of<T: schemars::JsonSchema + serde::de::DeserializeOwned>(
        name: &'static str,
    ) -> Verb {
        Verb {
            name,
            make: schema_of::<T>,
            parse: parse_as::<T>,
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

fn parse_as<T: serde::de::DeserializeOwned>(v: Value) -> Result<(), String> {
    serde_json::from_value::<T>(v)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Declare a bundle's verbs: `verbs! { "name" => Struct, … }` generates
/// `pub static VERBS` (name → schema, read by the kernel) and `pub enum Call` with
/// one variant per struct (named like it) and `Call::parse(&payload)`.
#[macro_export]
macro_rules! verbs {
    ($($name:literal => $ty:ident),+ $(,)?) => {
        /// Every verb this bundle declares: the list the kernel validates a call
        /// against and renders in `reflect` and `docs`.
        pub static VERBS: &[$crate::verbs::Verb] = &__DECLARED_VERBS;

        #[doc(hidden)]
        static __DECLARED_VERBS: [$crate::verbs::Verb; [$($name),+].len()] = [
            $( $crate::verbs::Verb::of::<$ty>($name) ),+
        ];

        /// A call of one of this bundle's verbs, its arguments typed.
        #[derive(Debug)]
        pub enum Call {
            $(
                #[doc = concat!("The `", $name, "` verb.")]
                $ty($ty)
            ),+
        }

        impl Call {
            /// Parse a payload the kernel has already checked into its verb's
            /// struct. `Err` is the serde text — unreachable after the kernel's check.
            pub fn parse(payload: &$crate::verbs::serde_json::Value) -> Result<Call, String> {
                let verb = payload
                    .get("type")
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                let args = $crate::verbs::args_of(payload);
                match verb {
                    $(
                        $name => $crate::verbs::serde_json::from_value::<$ty>(args)
                            .map(Call::$ty)
                            .map_err(|e| e.to_string()),
                    )+
                    other => Err(format!("unknown verb {other:?}")),
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
            default: p.get("default").cloned(),
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

/// The ONE-LINE tier of a declared verb: its arguments, then what it does.
#[must_use]
pub fn line(verb: &Verb) -> String {
    let doc = first_sentence(&verb.doc());
    let list = arg_list(verb.schema());
    if doc.is_empty() {
        list
    } else {
        format!("{list} — {doc}")
    }
}

/// The DETAILED tier of a declared verb: the whole doc, then every argument with
/// its type, whether it is required or its default, and its description.
#[must_use]
pub fn detail(verb: &Verb) -> String {
    let mut out = verb.doc();
    let a = args(verb.schema());
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

/// `{verb: one line}` for a list of declarations — what `reflect` carries as its
/// `verbs` block.
#[must_use]
pub fn lines(list: &[&Verb]) -> Value {
    let mut m = Map::new();
    for v in list {
        m.insert(v.name.to_string(), Value::String(line(v)));
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
    let one = |t: &str| -> bool {
        match t {
            "string" => v.is_string(),
            "boolean" => v.is_boolean(),
            "null" => v.is_null(),
            "integer" => v.as_number().is_some_and(|n| {
                n.is_i64() || n.is_u64() || fantastic_bundle::py_type_name(v) == "int"
            }),
            "number" => v.is_number(),
            "array" => match (v, o.get("items")) {
                (Value::Array(items), Some(it)) => items.iter().all(|x| matches_type(it, x)),
                (Value::Array(_), None) => true,
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

/// Does the schema take `v` only as a number or a bool (never as text)? Then text
/// a caller typed is typed first ([`crate::scalar`]), as the CLI and the model's
/// `<k>v</k>` path both would.
fn wants_scalar(schema: &Value) -> bool {
    if matches_type(schema, &Value::String(String::new())) {
        return false;
    }
    ["integer", "number", "boolean"]
        .iter()
        .any(|t| matches_type(schema, &scalar_probe(t)))
}

fn scalar_probe(t: &str) -> Value {
    match t {
        "integer" => json!(0),
        "number" => json!(0.5),
        _ => json!(true),
    }
}

/// `args` with the text of a number/bool argument typed by `scalar.rs`.
fn typed(schema: &Value, mut args: Map<String, Value>) -> Map<String, Value> {
    if let Some(props) = schema.get("properties").and_then(Value::as_object) {
        for (k, p) in props {
            if let Some(Value::String(s)) = args.get(k) {
                if wants_scalar(p) {
                    let t = crate::scalar::coerce_scalar(s);
                    if !t.is_string() {
                        args.insert(k.clone(), t);
                    }
                }
            }
        }
    }
    args
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

/// Check the ARGUMENTS of one call against a declaration. `Ok` carries the
/// arguments typed (a number written as text is a number); `Err` the refusal, by
/// argument name, with the verb's real argument list.
///
/// Order: unknown → missing → wrongly typed; the declared struct then parses the
/// result as the last word.
pub fn check_args(verb: &Verb, payload: &Value) -> Result<Value, Value> {
    let schema = verb.schema();
    check_against(verb.name, schema, verb.parse, payload)
}

fn check_against(
    name: &str,
    schema: &Value,
    parse: fn(Value) -> Result<(), String>,
    payload: &Value,
) -> Result<Value, Value> {
    let Value::Object(raw) = args_of(payload) else {
        return Ok(Value::Null);
    };
    let props = schema.get("properties").and_then(Value::as_object);
    let open = open_ended(schema);
    for k in raw.keys() {
        let known = props.is_some_and(|p| p.contains_key(k));
        if !known && !open {
            return Err(refuse_arg(
                name,
                BAD_ARG,
                k,
                format!("unknown argument {}", fantastic_bundle::py_repr(k)),
                schema,
            ));
        }
    }
    let raw = typed(schema, raw);
    if let Some(Value::Array(req)) = schema.get("required") {
        for r in req.iter().filter_map(Value::as_str) {
            if matches!(raw.get(r), None | Some(Value::Null)) {
                return Err(refuse_arg(
                    name,
                    MISSING_ARG,
                    r,
                    format!("missing argument {}", fantastic_bundle::py_repr(r)),
                    schema,
                ));
            }
        }
    }
    if let Some(props) = props {
        for (k, v) in &raw {
            let Some(p) = props.get(k) else { continue };
            if !matches_type(p, v) {
                return Err(refuse_arg(
                    name,
                    BAD_ARG,
                    k,
                    format!(
                        "argument {} must be {}, got {}",
                        fantastic_bundle::py_repr(k),
                        type_label(p),
                        fantastic_bundle::py_type_name(v)
                    ),
                    schema,
                ));
            }
        }
    }
    let out = Value::Object(raw);
    parse(out.clone()).map_err(|e| {
        refuse_arg(
            name,
            BAD_ARG,
            "",
            format!("the arguments do not parse ({e})"),
            schema,
        )
    })?;
    Ok(out)
}

/// `unknown verb 'X'; <agent> answers: a, b, c` — the refusal for a verb not in the
/// list. `answers` are the verb names the agent takes.
#[must_use]
pub fn unknown_verb(agent: &str, verb: &str, answers: &[&str]) -> Value {
    let mut r = refusal_hint(
        UNHANDLED,
        format!(
            "unknown verb {}; {agent} answers: {}",
            fantastic_bundle::py_repr(verb),
            answers.join(", ")
        ),
        format!("docs of={agent} lists each verb's arguments"),
    );
    with(&mut r, "verb", json!(verb));
    r
}

/// Check a whole call: the verb must be one of `lists` (searched in order — the
/// agent's bundle first, then the verbs every agent answers), and its arguments
/// must match its declaration. `Ok` is the payload to hand the handler: the same,
/// with a number or bool a caller wrote as text typed.
pub fn check_call(
    agent: &str,
    verb: &str,
    payload: &Value,
    lists: &[&[Verb]],
) -> Result<Value, Value> {
    let found = lists.iter().find_map(|l| l.iter().find(|v| v.name == verb));
    let Some(v) = found else {
        let mut names: Vec<&str> = Vec::new();
        for l in lists {
            for v in l.iter() {
                if !names.contains(&v.name) {
                    names.push(v.name);
                }
            }
        }
        return Err(unknown_verb(agent, verb, &names));
    };
    let args = check_args(v, payload)?;
    let mut out = match args {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    out.insert("type".to_string(), Value::String(verb.to_string()));
    Ok(Value::Object(out))
}

/// Check the FIELDS of a record patch (`create_agent` / `update_agent`) against what
/// the kernel declares for every record (`common`) and the bundle's own record
/// (`own`). A field neither names is refused by name, with both lists; a known one
/// must be of its type. A freeform record (`own.open`) accepts any field the
/// common record does not own.
pub fn check_record(
    verb: &str,
    bundle: &str,
    fields: &Map<String, Value>,
    common: &RecordDecl,
    own: &RecordDecl,
) -> Result<(), Value> {
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
            let typed_v = match v {
                Value::String(s) if wants_scalar(p) => {
                    let t = crate::scalar::coerce_scalar(s);
                    if t.is_string() {
                        v.clone()
                    } else {
                        t
                    }
                }
                _ => v.clone(),
            };
            if !matches_type(p, &typed_v) {
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
