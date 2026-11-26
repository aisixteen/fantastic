//! PER MODEL TYPE CALL FORMATS (#766), chat-side.
//!
//! The provider stays a pure raw-text streamer ([`crate::provider`]). The chat owns the
//! model TYPE of a pass (chosen from the model id by the bundled rule list, [`rules`]),
//! and each type is declared once, as a [`Decl`]: a [`Renderer`] (inbound: the chat's
//! internal call text into the type's own call text), a [`Reader`] (outbound: the type's
//! raw answer back into internal call text, as it streams), its marker templates and its
//! system sections, every one a resource file (`res/<type>/`, `res/shared/`). The map
//! ([`map`]) writes every model-visible text from it, and nothing else does.
//!
//! The internal form is today's tag grammar ([`crate::tool_parse`]): stored rows, the
//! router and every reader of stored call text keep it.

pub(crate) mod cache;
pub(crate) mod default;
pub(crate) mod hermes;
pub(crate) mod map;
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
mod tests_map;
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
    /// The call that follows was read after a repair of the closing brackets and braces
    /// at the end of its JSON, nothing else (`hermes`): the turn's `repaired` trace event.
    Repaired {
        /// The closers added, in order (empty when none were).
        added: String,
        /// The surplus closers cut after the complete call, in order (empty when none
        /// were).
        removed: String,
    },
}

/// The inbound half of a type: internal text in, the type's text out.
pub(crate) trait Renderer: Send + Sync {
    /// The adapter type: a cache key part and a trace value.
    fn name(&self) -> &'static str;
    /// Raised whenever `render`'s output bytes change: part of the cache epoch.
    fn rev(&self) -> u32;
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

/// A section of the system block, or a text the harness writes in the type's words: what
/// [`Decl::texts`] answers for. A section with no call form in it is one shared text
/// (`res/shared/`); one that shows a call form is a text per type (`res/<type>/`).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Section {
    /// The date the thread began and where the time now comes from (`{date}`, `{offset}`).
    Env,
    /// How to reach another agent.
    Send,
    /// What `reflect` is (`{detail}`, its declaration).
    Reflect,
    /// The context window: compaction, markers, `recall`.
    Context,
    /// The memory store (`{store}`).
    Memory,
    /// Titling calls (`context_titles`).
    Titles,
    /// Replies as rows (`context_async`).
    Async,
    /// The `line_summary` call (`line_summaries`).
    LineSummary,
    /// The working rules, last.
    Working,
    /// The reason of the harness's `stop` (`{n}` attempts); not a system section.
    StopReason,
}

impl Section {
    /// Every section, in the order of the enum.
    pub(crate) const ALL: [Self; 10] = [
        Self::Env,
        Self::Send,
        Self::Reflect,
        Self::Context,
        Self::Memory,
        Self::Titles,
        Self::Async,
        Self::LineSummary,
        Self::Working,
        Self::StopReason,
    ];
}

/// The marker templates of a type (`res/<type>/marker_*.txt`): one per kind of piece that
/// stands for something the view does not show whole ([`map::Piece`]).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Markers {
    /// An answered large reply block, folded.
    pub(crate) folded_reply: &'static str,
    /// The same, with the summary the model kept of it.
    pub(crate) folded_reply_kept: &'static str,
    /// An answered reply to the model's own `recall`, folded.
    pub(crate) folded_recall: &'static str,
    /// The same, with the summary the model kept of it.
    pub(crate) folded_recall_kept: &'static str,
    /// The thinking of an earlier answer, on the next user-side message.
    pub(crate) thought: &'static str,
    /// A reply block whose body a strategy stripped.
    pub(crate) stub: &'static str,
    /// An image the model has answered, left out of the request.
    pub(crate) image_anchor: &'static str,
    /// An image the model cannot be shown.
    pub(crate) image_alt: &'static str,
    /// The `keep` call template after a reply block that will fold.
    pub(crate) keep_cue: &'static str,
    /// The context notice's first line (`{strategy}`, `{dropped}`).
    pub(crate) notice_head: &'static str,
    /// Its summary of the dropped span (`{summary}`).
    pub(crate) notice_summary: &'static str,
    /// Its line on a span omitted in place.
    pub(crate) notice_omitted: &'static str,
    /// Its line on stripped bodies (`{stripped}`, `{bytes}`).
    pub(crate) notice_stripped: &'static str,
    /// Its last line: how to page turns back, in the type's call form.
    pub(crate) notice_tail: &'static str,
}

impl Markers {
    /// Every template, with its kind's name.
    pub(crate) fn all(&self) -> [(&'static str, &'static str); 14] {
        [
            ("folded_reply", self.folded_reply),
            ("folded_reply_kept", self.folded_reply_kept),
            ("folded_recall", self.folded_recall),
            ("folded_recall_kept", self.folded_recall_kept),
            ("thought", self.thought),
            ("stub", self.stub),
            ("image_anchor", self.image_anchor),
            ("image_alt", self.image_alt),
            ("keep_cue", self.keep_cue),
            ("notice_head", self.notice_head),
            ("notice_summary", self.notice_summary),
            ("notice_omitted", self.notice_omitted),
            ("notice_stripped", self.notice_stripped),
            ("notice_tail", self.notice_tail),
        ]
    }
}

/// ONE TYPE, DECLARED ONCE: everything a model of this type is shown that the harness
/// writes, and how its answers are read. [`map`] writes every model-visible text from it,
/// and nothing else does.
#[derive(Clone, Copy)]
pub(crate) struct Decl<'a> {
    /// The type's name, as the rule list writes it: a cache key part and a trace value.
    pub(crate) name: &'static str,
    /// Raised whenever the map of this type writes other bytes for the same pieces: part
    /// of the cache epoch (each resource text is hashed into it too).
    pub(crate) rev: u32,
    /// Inbound: stored call text into the type's call text.
    pub(crate) calls: &'a dyn Renderer,
    /// Outbound: the type's raw answer into internal call text.
    pub(crate) reader: &'a dyn Reader,
    /// One template per marker kind.
    pub(crate) markers: Markers,
    /// Every system section, and the `stop` reason.
    pub(crate) texts: fn(Section) -> &'static str,
    /// A text as one quoted value of the type's marker notation (`{key_q}` in a template).
    pub(crate) quote: fn(&str) -> String,
}

/// The environment section: the date the thread began, and where the time now comes from.
pub(crate) const ENV: &str = include_str!("res/shared/env.txt");
/// The memory store section (`{store}`).
pub(crate) const MEMORY: &str = include_str!("res/shared/howto_memory.txt");
/// The one line on the `line_summary` call.
pub(crate) const LINE_SUMMARY: &str = include_str!("res/shared/howto_line_summary.txt");

/// The context notice's lines that show no call form: one text for every type.
pub(crate) const NOTICE_HEAD: &str = include_str!("res/shared/notice_head.txt");
/// See [`NOTICE_HEAD`].
pub(crate) const NOTICE_SUMMARY: &str = include_str!("res/shared/notice_summary.txt");
/// See [`NOTICE_HEAD`].
pub(crate) const NOTICE_OMITTED: &str = include_str!("res/shared/notice_omitted.txt");
/// See [`NOTICE_HEAD`].
pub(crate) const NOTICE_STRIPPED: &str = include_str!("res/shared/notice_stripped.txt");

/// A shared section's text, `None` for one that is the type's own.
pub(crate) fn shared_text(s: Section) -> Option<&'static str> {
    match s {
        Section::Env => Some(ENV),
        Section::Memory => Some(MEMORY),
        Section::LineSummary => Some(LINE_SUMMARY),
        _ => None,
    }
}

/// Every adapter type, in a fixed order. A rule naming a type not here is a load error.
pub(crate) const REGISTRY: &[Decl<'static>] = &[
    Decl {
        name: DEFAULT,
        rev: 1,
        calls: &default::Tags,
        reader: &default::Tags,
        markers: default::MARKERS,
        texts: default::texts,
        quote: default::quote,
    },
    Decl {
        name: hermes::NAME,
        rev: 1,
        calls: &hermes::Hermes,
        reader: &hermes::Hermes,
        markers: hermes::MARKERS,
        texts: hermes::texts,
        quote: hermes::quote,
    },
];

/// The registry's declaration named `name`.
pub(crate) fn declared(name: &str) -> Option<&'static Decl<'static>> {
    REGISTRY.iter().find(|e| e.name == name)
}

/// The registry's declaration of a type the rule list gave ([`rules::type_of`]): every
/// type a loaded rule names is registered ([`rules::load`]), and so is [`DEFAULT`].
pub(crate) fn of(kind: &str) -> &'static Decl<'static> {
    declared(kind).unwrap_or_else(|| panic!("model type `{kind}` is not registered"))
}

/// The type of the chat's next pass: [`rules::type_of`] over [`rules::model_of`], on the
/// chat's own state, for a reader that has no pass (a `dry` `context_recall` sizing the
/// block): the `models` ask is made once per `(provider_id, model)` pair.
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
