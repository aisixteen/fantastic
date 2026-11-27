//! MAP ONCE: the one place the text a model reads is written.
//!
//! The assembly and the projection SELECT what a request shows — which rows, at which
//! resolution, what stands for a body that is not shown whole — and write no model text:
//! a message in flight carries [`Piece`]s with facts (its `pieces`; a system message its
//! [`SysPiece`]s, `sys`). Its `content` is the pieces' skeleton ([`skeleton`]: the stored
//! text and the tag lines, no marker), what a strategy reads of it; a message with no
//! `pieces` shows its `content` whole.
//!
//! MAP writes a message for one model type ([`Decl`]): a stored text through the type's
//! call renderer, every other piece through the type's marker template, the system block
//! from the type's section texts. It is a pure function of (pieces, decl, the chat's own
//! id): no clock, no process time zone, no unordered map, so a restart maps the same bytes.
//!
//! The rules, the same for every type:
//! - An assistant message is the model's own stored text and nothing else.
//! - A marker sits in a user-side message, in the type's reply form, and names the row's
//!   id, its size and the call that unfolds it: the existing `recall` with that `id`.
//! - A thought marker rides on the next user-side message, at its top.
//! - No row shows a time: the date is said once, in the system block's environment
//!   section, from the first row of the thread.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{fill, Decl, Section};

/// What a folded reply block was.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FoldKind {
    /// A large reply body.
    Reply,
    /// A reply to the model's own `recall`.
    Recall,
}

/// How an image that is not sent whole shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ImageForm {
    /// The model answered it: a line naming it, `recall` brings it back.
    Anchor,
    /// The model cannot be shown it.
    Alt,
}

/// One thing a message shows. Facts only: the words are the type's.
///
/// Externally tagged on purpose: serde's internally tagged enums buffer their content,
/// and with `arbitrary_precision` a buffered number does not read back as an integer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Piece {
    /// A stored text, in the internal call form: the type's call renderer writes it.
    Text {
        /// The text.
        text: String,
    },
    /// An answered reply block shown as its tag line and a marker.
    Folded {
        /// A large body, or a recall's reply.
        kind: FoldKind,
        /// The agent that replied.
        agent: String,
        /// The verb it answered.
        verb: String,
        /// The body's size in tokens.
        tokens: usize,
        /// The stored row that holds the body whole.
        id: i64,
        /// The block's `[reply to …]` line.
        tag_line: String,
        /// The body's first line, capped.
        head: String,
        /// The summary the model kept of the block (`attributes.keep`), when it kept one.
        kept: Option<String>,
        /// What a recall reply recalled (`id 5`, `query "x"`); empty otherwise.
        what: String,
    },
    /// The thinking of an earlier answer, folded: on the next user-side message.
    Thought {
        /// The thinking's size in tokens.
        tokens: usize,
        /// The assistant row that holds it.
        id: i64,
    },
    /// A reply block whose body a strategy stripped.
    Stub {
        /// The block's `[reply to …]` line.
        tag_line: String,
        /// The body's size in characters.
        bytes: usize,
        /// The stored row that holds the body.
        id: i64,
    },
    /// The `keep` call template after a reply block that will fold (#641).
    KeepCue {
        /// The block's pid.
        pid: i64,
    },
    /// THE CONTEXT NOTICE a projection prepends (`[context-notice] …`): what it did, and
    /// how to page messages back, in the type's call form.
    Notice {
        /// The strategy that ran.
        strategy: String,
        /// The messages it dropped from the view.
        dropped: usize,
        /// The summary of the dropped span, when it made one.
        summary: Option<String>,
        /// A span was omitted in place (no summary).
        omitted: bool,
        /// Reply bodies stripped, and the bytes that left the view with them.
        stripped: usize,
        /// See `stripped`.
        stripped_bytes: i64,
    },
    /// An image part that is not sent whole (#622).
    Image {
        /// How it shows.
        form: ImageForm,
        /// The stored row that holds it.
        id: Option<i64>,
        /// The name it goes by.
        name: String,
        /// Pixels.
        width: u64,
        /// Pixels.
        height: u64,
        /// Its media type.
        media_type: String,
        /// What it costs the model when shown.
        tokens: u64,
    },
}

impl Piece {
    /// A stored text.
    pub(crate) fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// What the piece leaves in a message's skeleton: the stored text and the tag lines.
    fn skeleton(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text),
            Self::Folded { tag_line, .. } | Self::Stub { tag_line, .. } => Some(tag_line),
            _ => None,
        }
    }

    /// Whether it sits on the line right after the piece before it (a cue, an image),
    /// not after a blank line.
    fn joins_closely(&self) -> bool {
        matches!(self, Self::KeepCue { .. } | Self::Image { .. })
    }
}

/// The pieces' skeleton: their stored texts and tag lines, a blank line between.
#[must_use]
pub(crate) fn skeleton(pieces: &[Piece]) -> String {
    pieces
        .iter()
        .filter_map(Piece::skeleton)
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The pieces a message in flight shows: its `pieces`, else its `content` whole.
#[must_use]
pub(crate) fn pieces_of(message: &Value) -> Vec<Piece> {
    if let Some(p) = message
        .get("pieces")
        .and_then(|v| serde_json::from_value::<Vec<Piece>>(v.clone()).ok())
    {
        return p;
    }
    match message.get("content").and_then(Value::as_str) {
        Some(c) if !c.is_empty() => vec![Piece::text(c)],
        _ => Vec::new(),
    }
}

/// `message` showing `pieces`: its `pieces` set and its `content` their skeleton.
#[must_use]
pub(crate) fn with_pieces(mut message: Value, pieces: &[Piece]) -> Value {
    if let Some(m) = message.as_object_mut() {
        m.insert("content".into(), Value::String(skeleton(pieces)));
        m.insert(
            "pieces".into(),
            serde_json::to_value(pieces).expect("pieces serialize"),
        );
    }
    message
}

/// Whether `message` shows a piece for which `f` holds.
pub(crate) fn shows(message: &Value, f: impl Fn(&Piece) -> bool) -> bool {
    message.get("pieces").is_some() && pieces_of(message).iter().any(f)
}

// ── the system block ─────────────────────────────────────────────────────────

/// The first row of a thread, as the environment section reads it: its `ts` (epoch
/// seconds) and its stored UTC offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Env {
    /// Epoch seconds.
    pub(crate) ts: i64,
    /// Seconds east of UTC, stored with the row.
    pub(crate) offset: i64,
}

impl Env {
    /// The env of a row: its `attributes.ts` and `attributes.utc_offset` (none stored:
    /// UTC). `None` when it has no `ts`.
    #[must_use]
    pub(crate) fn of(row: &Value) -> Option<Self> {
        let a = row.get("attributes")?;
        let ts = a.get("ts")?.as_f64()?.floor() as i64;
        let offset = a
            .get(crate::row_attrs::UTC_OFFSET)
            .and_then(Value::as_i64)
            .unwrap_or(0);
        Some(Self { ts, offset })
    }
}

/// The how-to part of a system block, as sections the type writes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Howto {
    /// The sections, in order.
    pub(crate) sections: Vec<Section>,
    /// The record's memory store id (`{store}`).
    pub(crate) store: String,
    /// The first row of the thread (`{date}`, `{offset}`).
    pub(crate) env: Option<Env>,
}

/// What a system piece holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SysBody {
    /// A text that is the same under every type, in the internal form.
    Text(String),
    /// The how-to sections.
    Howto(Howto),
}

/// One piece of the system block: what it is (`system_text`, `prompt_file`,
/// `self_reflect`, `verb_docs`, or empty for the blank line between two pieces) and what
/// it holds. The block is the pieces' mapped texts end to end.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SysPiece {
    /// The `part` of the request's parts list.
    pub(crate) part: String,
    /// What it holds.
    pub(crate) body: SysBody,
}

/// `reflect`'s declaration, detailed: the `{detail}` of the reflect section.
fn reflect_detail() -> String {
    let rows = fantastic_kernel::verbs::rows(&[fantastic_kernel::declared::common::VERBS], &[]);
    rows.first()
        .map(fantastic_kernel::verbs::detail)
        .unwrap_or_default()
}

/// One section in the type's words.
fn section(decl: &Decl<'_>, s: Section, h: &Howto) -> String {
    let text = (decl.texts)(s);
    match s {
        Section::Env => {
            let (date, offset) = h
                .env
                .map(|e| crate::row_attrs::day_of(e.ts, e.offset))
                .unwrap_or_default();
            fill(text, &[("date", &date), ("offset", &offset)])
        }
        Section::Memory => fill(text, &[("store", &h.store)]),
        Section::Reflect => fill(text, &[("detail", &reflect_detail())]),
        _ => text.to_string(),
    }
}

/// The how-to as `decl` writes it: its sections, a blank line between. An environment
/// section with no row to read is left out.
fn howto_text(decl: &Decl<'_>, h: &Howto) -> String {
    h.sections
        .iter()
        .filter(|s| **s != Section::Env || h.env.is_some())
        .map(|s| section(decl, *s, h))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The system block as `decl` writes it, piece by piece: `(part, text)`.
#[must_use]
pub(crate) fn system_parts(decl: &Decl<'_>, pieces: &[SysPiece]) -> Vec<(String, String)> {
    pieces
        .iter()
        .map(|p| {
            let text = match &p.body {
                SysBody::Text(t) => decl.calls.render(t),
                SysBody::Howto(h) => decl.calls.render(&howto_text(decl, h)),
            };
            (p.part.clone(), text)
        })
        .collect()
}

/// The system block as `decl` writes it: the pieces' texts end to end.
#[must_use]
pub(crate) fn system(decl: &Decl<'_>, pieces: &[SysPiece]) -> String {
    system_parts(decl, pieces)
        .into_iter()
        .map(|(_, t)| t)
        .collect()
}

// ── one message ──────────────────────────────────────────────────────────────

/// What a message holds, as the map reads it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Body {
    /// A row or a harness message: its pieces.
    Pieces(Vec<Piece>),
    /// The system message: its pieces.
    System(Vec<SysPiece>),
    /// Content that is not text, carried as it is.
    Raw(Value),
}

/// What a request carries of one message: what the render cache keys and what [`row`]
/// writes from.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Shown {
    /// The stored row it shows, when it shows one.
    pub(crate) id: Option<i64>,
    /// Its role.
    pub(crate) role: Value,
    /// The form it took (`whole`, `anchor`, `recalled`, `image_anchor`, …).
    pub(crate) form: Option<String>,
    /// What it holds.
    pub(crate) body: Body,
    /// The image parts it sends whole.
    pub(crate) images: Option<Vec<Value>>,
    /// The agent's cache mark.
    pub(crate) cache_mark: Option<Value>,
}

impl Shown {
    /// What a message in flight shows.
    #[must_use]
    pub(crate) fn of(m: &Value) -> Self {
        let body = if let Some(sys) = m
            .get("sys")
            .and_then(|v| serde_json::from_value::<Vec<SysPiece>>(v.clone()).ok())
        {
            Body::System(sys)
        } else {
            match m.get("content") {
                Some(Value::String(_)) | None => Body::Pieces(pieces_of(m)),
                Some(other) => Body::Raw(other.clone()),
            }
        };
        Self {
            id: m.get("id").and_then(Value::as_i64),
            role: m.get("role").cloned().unwrap_or(Value::Null),
            form: m.get("form").and_then(Value::as_str).map(str::to_string),
            body,
            images: crate::images::parts_of(m).cloned(),
            cache_mark: m.get(crate::caching::CACHE_MARK).cloned(),
        }
    }

    /// Whether its content is text the map writes (not raw content).
    #[must_use]
    pub(crate) fn is_text(&self) -> bool {
        !matches!(self.body, Body::Raw(_))
    }
}

/// THE CANONICAL BYTES of what `shown` asks the map to write: its body, every piece with
/// its facts in a fixed field order (no map, no attribute, no clock). Equal bytes map to
/// equal text under one type. The render cache keys its entries by them.
#[must_use]
pub(crate) fn canonical(shown: &Shown) -> Vec<u8> {
    serde_json::to_vec(&shown.body).expect("a shown message serializes")
}

/// A template filled: its trailing newline dropped; `{self}` the chat's own id; every
/// value given as `{key}` and, quoted for the type, as `{key_q}`.
fn marker(decl: &Decl<'_>, template: &str, own: &str, vals: &[(&str, String)]) -> String {
    let mut all: Vec<(String, String)> = vec![
        ("self_q".to_string(), (decl.quote)(own)),
        ("self".to_string(), own.to_string()),
    ];
    for (k, v) in vals {
        all.push((format!("{k}_q"), (decl.quote)(v)));
        all.push(((*k).to_string(), v.clone()));
    }
    let pairs: Vec<(&str, &str)> = all.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    fill(template.strip_suffix('\n').unwrap_or(template), &pairs)
}

/// An optional id as a template writes it: the number, or `null`.
fn id_text(id: Option<i64>) -> String {
    id.map_or_else(|| "null".to_string(), |i| i.to_string())
}

/// One piece that is not text, as `decl` writes it.
fn marker_of(decl: &Decl<'_>, own: &str, p: &Piece) -> String {
    let m = &decl.markers;
    match p {
        Piece::Text { text } => decl.calls.render(text),
        Piece::Folded {
            kind,
            agent,
            verb,
            tokens,
            id,
            tag_line,
            head,
            kept,
            what,
        } => {
            let template = match (kept, kind) {
                (Some(_), FoldKind::Reply) => m.folded_reply_kept,
                (Some(_), FoldKind::Recall) => m.folded_recall_kept,
                (None, FoldKind::Reply) => m.folded_reply,
                (None, FoldKind::Recall) => m.folded_recall,
            };
            marker(
                decl,
                template,
                own,
                &[
                    ("tag_line", tag_line.clone()),
                    ("id", id.to_string()),
                    ("agent", agent.clone()),
                    ("verb", verb.clone()),
                    ("tokens", tokens.to_string()),
                    ("head", head.clone()),
                    ("what", what.clone()),
                    ("kept", kept.clone().unwrap_or_default()),
                ],
            )
        }
        Piece::Thought { tokens, id } => marker(
            decl,
            m.thought,
            own,
            &[("id", id.to_string()), ("tokens", tokens.to_string())],
        ),
        Piece::Stub {
            tag_line,
            bytes,
            id,
        } => marker(
            decl,
            m.stub,
            own,
            &[
                ("tag_line", tag_line.clone()),
                ("id", id.to_string()),
                ("bytes", bytes.to_string()),
            ],
        ),
        Piece::KeepCue { pid } => marker(decl, m.keep_cue, own, &[("pid", pid.to_string())]),
        Piece::Notice {
            strategy,
            dropped,
            summary,
            omitted,
            stripped,
            stripped_bytes,
        } => {
            let mut lines = vec![fill(
                m.notice_head,
                &[("strategy", strategy), ("dropped", &dropped.to_string())],
            )];
            if let Some(s) = summary {
                lines.push(fill(m.notice_summary, &[("summary", s)]));
            } else if *omitted {
                lines.push(m.notice_omitted.to_string());
            }
            if *stripped > 0 {
                lines.push(fill(
                    m.notice_stripped,
                    &[
                        ("stripped", &stripped.to_string()),
                        ("bytes", &stripped_bytes.to_string()),
                    ],
                ));
            }
            lines.push(m.notice_tail.to_string());
            // the summary is model text: its calls in the type's form, as any stored text
            decl.calls.render(&lines.join("\n"))
        }
        Piece::Image {
            form,
            id,
            name,
            width,
            height,
            media_type,
            tokens,
        } => marker(
            decl,
            match form {
                ImageForm::Anchor => m.image_anchor,
                ImageForm::Alt => m.image_alt,
            },
            own,
            &[
                ("id", id_text(*id)),
                ("name", name.clone()),
                ("width", width.to_string()),
                ("height", height.to_string()),
                ("media_type", media_type.clone()),
                ("tokens", tokens.to_string()),
            ],
        ),
    }
}

/// THE ROW MAP: `pieces` as `decl` writes them for the chat `own`. A run of stored texts
/// is joined by a blank line and goes through the type's call renderer whole (so a
/// message with no marker maps exactly as its stored text does); every other piece is its
/// marker template filled; a blank line between pieces, a line break before a cue or an
/// image line.
#[must_use]
pub(crate) fn row(decl: &Decl<'_>, own: &str, pieces: &[Piece]) -> String {
    let mut out = String::new();
    let mut run: Vec<&str> = Vec::new();
    let mut first = true;
    let mut push = |out: &mut String, text: &str, close: bool| {
        if !first {
            out.push_str(if close { "\n" } else { "\n\n" });
        }
        first = false;
        out.push_str(text);
    };
    for p in pieces {
        if let Piece::Text { text } = p {
            run.push(text);
            continue;
        }
        if !run.is_empty() {
            push(&mut out, &decl.calls.render(&run.join("\n\n")), false);
            run.clear();
        }
        push(&mut out, &marker_of(decl, own, p), p.joins_closely());
    }
    if !run.is_empty() {
        push(&mut out, &decl.calls.render(&run.join("\n\n")), false);
    }
    out
}

/// The content `shown` maps to under `decl`.
#[must_use]
pub(crate) fn content(decl: &Decl<'_>, own: &str, shown: &Shown) -> Value {
    match &shown.body {
        Body::Pieces(p) => Value::String(row(decl, own, p)),
        Body::System(p) => Value::String(system(decl, p)),
        Body::Raw(v) => v.clone(),
    }
}

/// The message a request carries for `shown`, its content `content`: its role, the
/// content, its images and its cache mark; nothing else.
#[must_use]
pub(crate) fn message_with(shown: &Shown, content: Value) -> Value {
    let mut out = json!({"role": shown.role, "content": content});
    if let Some(images) = &shown.images {
        out["images"] = Value::Array(images.clone());
    }
    if let Some(v) = &shown.cache_mark {
        out[crate::caching::CACHE_MARK] = v.clone();
    }
    out
}

/// The message a request carries for `shown` under `decl`, uncached.
#[must_use]
pub(crate) fn message(decl: &Decl<'_>, own: &str, shown: &Shown) -> Value {
    message_with(shown, content(decl, own, shown))
}

/// `messages` in flight as `decl` writes them for the chat `own`, uncached.
#[must_use]
pub(crate) fn messages(decl: &Decl<'_>, own: &str, messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .map(|m| message(decl, own, &Shown::of(m)))
        .collect()
}
