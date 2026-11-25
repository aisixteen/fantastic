//! PER MODEL TYPE CALL FORMATS (#766), chat-side.
//!
//! The provider stays a pure raw-text streamer ([`crate::provider`]). The chat owns the
//! model TYPE of a pass (the adapter that formats its calls, chosen from the model id by
//! the bundled rule list, [`rules`]), and each type is a pair: a [`Renderer`] (inbound:
//! the chat's internal call text into the type's own call text) and a [`Reader`]
//! (outbound: the type's raw answer back into internal call text, as it streams). They
//! are separate so the render cache depends on the renderer only and the attempt loop
//! on the reader only.
//!
//! The internal form is today's tag grammar ([`crate::tool_parse`]): stored rows, the
//! router and every reader of stored call text keep it.

pub(crate) mod cache;
pub(crate) mod default;
pub(crate) mod hermes;
pub(crate) mod pass;
pub(crate) mod rules;
pub(crate) mod stream;
pub(crate) mod trace;

#[cfg(test)]
mod tests_cache;
#[cfg(test)]
mod tests_chain;
#[cfg(test)]
mod tests_default;
#[cfg(test)]
mod tests_determinism;
#[cfg(test)]
mod tests_hermes;
#[cfg(test)]
mod tests_restart;
#[cfg(test)]
mod tests_rules;
#[cfg(test)]
mod tests_scale;
#[cfg(test)]
mod tests_stream;
#[cfg(test)]
mod tests_switch;
#[cfg(test)]
mod tests_trace;

/// The type of a model no rule names: today's tag grammar, a type like the others.
pub(crate) const DEFAULT: &str = "default";

/// One piece of a type's answer as its [`Outbound`] reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Out {
    /// Text to forward now: prose, reasoning, or bytes that are not (yet) a call.
    Text(String),
    /// A call that parsed, as internal call text.
    Call {
        /// The call in the internal form (`<call name="…">…</call>`).
        text: String,
    },
    /// A malformed or foreign call: the attempt failed. The report is worded for the
    /// type, for the re-ask.
    Bad {
        /// What was wrong, in the type's own words.
        report: String,
    },
    /// The stream ended inside a call: the parts of its head that were complete.
    Open {
        /// The call's name, when it was written whole.
        name: Option<String>,
        /// Its target, when it was written whole.
        target: Option<String>,
    },
}

/// The inbound half of a type: internal text in, the type's text out.
pub(crate) trait Renderer: Send + Sync {
    /// The adapter type: a cache key part and a trace value.
    fn name(&self) -> &'static str;
    /// Raised whenever `render`'s output bytes change: part of the cache epoch.
    fn rev(&self) -> u32;
    /// True when `render` returns its input for every text: the cache stores nothing.
    fn identity(&self) -> bool;
    /// Every parseable internal envelope replaced by the type's call text, reply blocks
    /// in the type's reply form, the placeholder form swapped; everything else byte for
    /// byte. Pure: no clock, no environment, no random value, no unordered map.
    fn render(&self, text: &str) -> String;
}

/// The streaming state of one answer, made by [`Reader::outbound`].
pub(crate) trait Outbound: Send {
    /// One delta of the model's raw text in; what can be told of it now out.
    fn feed(&mut self, delta: &str) -> Vec<Out>;
    /// The stream ended: whatever was held.
    fn finish(&mut self) -> Vec<Out>;
}

/// The outbound half of a type: reads the model's raw answer.
pub(crate) trait Reader: Send + Sync {
    /// A new streaming state for one answer.
    fn outbound(&self) -> Box<dyn Outbound>;
    /// The names the model may write.
    fn tools(&self) -> &'static [&'static str];
}

/// One adapter type of the registry: its name, its pair, and the how-to the assembly
/// puts in the system block for it (`res/<type>/howto_send.txt`, Q16).
pub(crate) struct Entry {
    /// The type's name, as the rule list writes it.
    pub(crate) name: &'static str,
    /// Inbound: internal text into the type's text.
    pub(crate) renderer: &'static dyn Renderer,
    /// Outbound: the type's raw answer into internal call text.
    pub(crate) reader: &'static dyn Reader,
    /// The system block's section on reaching other agents, for this type.
    pub(crate) howto_send: &'static str,
    /// The reason of the harness's `stop` (`res/<type>/stop_reason.txt`): `{n}` attempts.
    pub(crate) stop_reason: &'static str,
}

/// Every adapter type, in a fixed order. A rule naming a type not here is a load error.
pub(crate) const REGISTRY: &[Entry] = &[
    Entry {
        name: DEFAULT,
        renderer: &default::Tags,
        reader: &default::Tags,
        howto_send: default::HOWTO_SEND,
        stop_reason: default::STOP_REASON,
    },
    Entry {
        name: hermes::NAME,
        renderer: &hermes::Hermes,
        reader: &hermes::Hermes,
        howto_send: hermes::HOWTO_SEND,
        stop_reason: hermes::STOP_REASON,
    },
];

/// The registry's entry named `name`.
pub(crate) fn entry(name: &str) -> Option<&'static Entry> {
    REGISTRY.iter().find(|e| e.name == name)
}

/// The registry's entry of a type the rule list gave ([`rules::type_of`]): every type a
/// loaded rule names is registered ([`rules::load`]), and so is [`DEFAULT`].
pub(crate) fn of(kind: &str) -> &'static Entry {
    entry(kind).unwrap_or_else(|| panic!("model type `{kind}` is not registered"))
}

/// The type of the chat's next pass: [`rules::type_of`] over [`rules::model_of`], on the
/// chat's own state. The assembly picks the how-to by it (Q16), so the block and the pass
/// read one resolution: the `models` ask is made once per `(provider_id, model)` pair.
pub(crate) async fn type_now(
    self_id: &fantastic_kernel::AgentId,
    kernel: &std::sync::Arc<fantastic_kernel::Kernel>,
    meta: &serde_json::Map<String, serde_json::Value>,
) -> &'static str {
    let state = crate::state::state_for(self_id);
    let id = rules::model_of(self_id, kernel, meta, &state).await;
    rules::type_of(rules::shipped(), id.as_deref().unwrap_or_default())
}

/// A resource text with each `{key}` replaced by its value, in one pass: a value is never
/// scanned again, and a `{…}` that names no key stays as written. No other markup.
pub(crate) fn fill(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    'scan: while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        let tail = &rest[i + 1..];
        for (key, value) in values {
            if let Some(after) = tail.strip_prefix(key).and_then(|a| a.strip_prefix('}')) {
                out.push_str(value);
                rest = after;
                continue 'scan;
            }
        }
        out.push('{');
        rest = tail;
    }
    out.push_str(rest);
    out
}
