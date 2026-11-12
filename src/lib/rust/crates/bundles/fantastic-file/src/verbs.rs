//! The verbs a file bridge declares (issue #642): one struct per verb, then
//! `verbs!` — the list the agent's entry checks every call against and `reflect`
//! and `docs` render.
//!
//! A document or a raw chunk rides a call as its REQUEST BODY at `bytes` (the
//! binary channel); a text document may also ride `bytes` as text on the text
//! channel, so `bytes` is declared optional and the verb refuses a call that
//! carries neither.

use schemars::JsonSchema;
use serde::Deserialize;

/// List the exposed entries of a directory and the directories on the way to one. Returns {path, files:[{name, path, type, size?}]}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct List {
    /// A directory, relative to the root, no leading `/`; absent, the root.
    #[schemars(regex(pattern = r"^(?:[^/][\s\S]*)?$"))]
    pub path: Option<String>,
}

/// Read a text page of a file; refuses binary. Returns {path, lines:[{n,text}], total, eof, newline, version, origin}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Read {
    /// The file, relative to the root (a bridge needs it; a file view reads its own file).
    #[schemars(length(min = 1), regex(pattern = r"^[^/][\s\S]*$"))]
    pub path: Option<String>,
    /// The first line of the page, counted from 0.
    #[serde(default)]
    pub offset: u64,
    /// How many lines.
    #[serde(default = "page_lines")]
    #[schemars(range(min = 1))]
    pub limit: u64,
}

/// Write a whole document atomically; the document is `bytes`, verbatim. Returns {path, size, version}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Write {
    /// The file, relative to the root (a bridge needs it; a file view writes its own file).
    #[schemars(length(min = 1), regex(pattern = r"^[^/][\s\S]*$"))]
    pub path: Option<String>,
    /// The whole document (text; a binary channel carries it as the request body at `bytes`).
    pub bytes: Option<String>,
    /// The version you read; a changed document is refused.
    pub if_match: Option<String>,
}

/// One file of an `edit` transaction.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditFile {
    /// The file, relative to the root.
    #[schemars(length(min = 1), regex(pattern = r"^[^/][\s\S]*$"))]
    pub path: String,
    /// How many hunks of the patch belong to this file; left out, with one file every block applies to it, and with named blocks no count is needed.
    pub hunks: Option<std::num::NonZeroU64>,
    /// The version you read; a changed document is refused.
    pub if_match: Option<String>,
}

/// Apply SEARCH/REPLACE hunks to one or more files as a TRANSACTION: every hunk of every edit is verified before any write, and any failure writes nothing. Returns {edited:true, edits:[{path,hunks,size,version}]}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Edit {
    /// The files, each at most once — even one file rides the list.
    #[schemars(length(min = 1))]
    pub edits: Vec<EditFile>,
    /// The patch: every hunk of every edit, concatenated in `edits` order (text; a binary channel carries it as the request body at `bytes`).
    #[schemars(length(min = 1))]
    pub bytes: Option<String>,
    /// Verify every hunk and write nothing.
    #[serde(default)]
    pub dry_run: bool,
}

/// Append one chunk to a file (created when absent); the chunk is `bytes`. Returns {path, size, version}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Append {
    /// The file, relative to the root.
    #[schemars(length(min = 1), regex(pattern = r"^[^/][\s\S]*$"))]
    pub path: String,
    /// The chunk (text; a binary channel carries it as the request body at `bytes`).
    pub bytes: Option<String>,
}

/// Swap a document with the bytes the last write, edit or append replaced (a second undo is redo); one slot per path, in RAM, lost on restart. Returns {undone:true, path, version|deleted}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Undo {
    /// The file, relative to the root.
    #[schemars(length(min = 1), regex(pattern = r"^[^/][\s\S]*$"))]
    pub path: String,
}

/// The SOURCE half of a stream: read ONE raw chunk, returned as the reply body at `bytes`. Returns {path, offset, bytes, next_offset, eof, size}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadStream {
    /// The file, relative to the root.
    #[schemars(length(min = 1), regex(pattern = r"^[^/][\s\S]*$"))]
    pub path: String,
    /// Where the chunk starts, in bytes.
    #[serde(default)]
    pub offset: u64,
    /// The chunk size in bytes.
    #[serde(default = "chunk_bytes")]
    #[schemars(range(min = 1))]
    pub length: u64,
}

/// The SINK half of a stream: write ONE raw chunk, carried as the request body at `bytes`. Returns {path, written, offset, size}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteStream {
    /// The file, relative to the root.
    #[schemars(length(min = 1), regex(pattern = r"^[^/][\s\S]*$"))]
    pub path: String,
    /// Where the chunk lands, in bytes; absent, at the end of the file.
    pub offset: Option<u64>,
    /// Start the file over with this chunk.
    #[serde(default)]
    pub truncate: bool,
}

/// A server-side stream copy SOURCE to SINK, chunk by chunk, in ONE call; both ends run AS THIS BRIDGE and each gates and clamps itself. Returns {source, sink, bytes, chunks}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Pump {
    /// The agent that answers `read_stream`; absent, this bridge.
    #[schemars(length(min = 1))]
    pub source: Option<String>,
    /// The file to read from the source.
    #[schemars(length(min = 1), regex(pattern = r"^[^/][\s\S]*$"))]
    pub source_path: String,
    /// The agent that answers `write_stream`; absent, this bridge.
    #[schemars(length(min = 1))]
    pub sink: Option<String>,
    /// The file to write on the sink; absent, `source_path`.
    #[schemars(length(min = 1), regex(pattern = r"^[^/][\s\S]*$"))]
    pub sink_path: Option<String>,
    /// The chunk size in bytes.
    #[serde(default = "chunk_bytes")]
    #[schemars(range(min = 1))]
    pub chunk: u64,
}

/// Delete a file, an empty directory or (`recursive`) a directory tree; a symlink is unlinked, never followed. Returns {path, deleted:true}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Delete {
    /// What goes, relative to the root (never the root itself).
    #[schemars(length(min = 1), regex(pattern = r"^[^/][\s\S]*$"))]
    pub path: String,
    /// Remove a directory with what is in it.
    #[serde(default)]
    pub recursive: bool,
}

/// Rename a file or directory, creating the new parent directories. Returns {old_path, new_path}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Rename {
    /// What moves, relative to the root.
    #[schemars(length(min = 1), regex(pattern = r"^[^/][\s\S]*$"))]
    pub old_path: String,
    /// Where it goes, relative to the root.
    #[schemars(length(min = 1), regex(pattern = r"^[^/][\s\S]*$"))]
    pub new_path: String,
}

/// Make a directory, parents included. Returns {path, created:true}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Mkdir {
    /// The directory, relative to the root.
    #[schemars(length(min = 1), regex(pattern = r"^[^/][\s\S]*$"))]
    pub path: String,
}

/// Emit `{type: changed, path, kind}` on this bridge's inbox whenever an exposed file or directory changes on disk, by any writer; WATCH this bridge to receive them. `kind` is `added`, `modified` or `deleted`: treat any `changed` as "read it again". A directory reports its entries (not recursive). Held per caller until `unwatch_path`; RAM only. Returns {watching, paths}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WatchPath {
    /// An exposed file or directory, relative to the root.
    #[schemars(length(min = 1), regex(pattern = r"^[^/][\s\S]*$"))]
    pub path: String,
}

/// Stop YOUR watch of a path; the disk watch ends when no caller holds a path. Returns {watching:false, paths}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UnwatchPath {
    /// The path you watched.
    #[schemars(length(min = 1), regex(pattern = r"^[^/][\s\S]*$"))]
    pub path: String,
}

fn page_lines() -> u64 {
    crate::text::READ_LIMIT
}

fn chunk_bytes() -> u64 {
    65536
}

/// Which kind of path a view shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ViewKind {
    /// One file.
    File,
    /// One folder.
    Folder,
}

/// Reuse the view of a path or make it: a `file_bridge.tools` record carrying `view`, a child of this bridge, whose grant here is its own verbs only. Returns {id, kind, path, created}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OpenView {
    /// `file` or `folder`.
    pub kind: ViewKind,
    /// Root-relative, exposed by `allow`; `.` is the whole root, folders only.
    #[schemars(length(min = 1), regex(pattern = r"^[^/][\s\S]*$"))]
    pub path: String,
    /// The new view's agent id, taken only when it is free.
    pub id: Option<String>,
}

/// Start relaying this view's changes onto this agent (a file: `{type: changed, path, kind}`; a folder: `{type: added|changed|removed, path, name}`); WATCH this agent to receive them. Idempotent. Answered by a view only.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Watch {}

/// Show where a path inside this folder view sits: the folders to open on the way down. Returns {revealed, folder_path, dirs}, or not_found when nothing is there. Answered by a folder view only.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Reveal {
    /// A file or folder inside this one, relative to it.
    #[schemars(length(min = 1), regex(pattern = r"^[^/][\s\S]*$"))]
    pub path: String,
}

fantastic_kernel::verbs! {
    dynamic;
    "list" => List,
    "read" => Read,
    "write" => Write,
    "edit" => Edit,
    "append" => Append,
    "undo" => Undo,
    "read_stream" => ReadStream,
    "write_stream" => WriteStream,
    "pump" => Pump,
    "delete" => Delete,
    "rename" => Rename,
    "mkdir" => Mkdir,
    "watch_path" => WatchPath,
    "unwatch_path" => UnwatchPath,
    "open_view" => OpenView,
}
