//! The verbs an exec_bridge agent declares (issue #642): one struct per verb, then
//! `verbs!` in its `dynamic;` form — the list the agent's entry checks every call
//! against, and `reflect` and `docs` render. The program a job runs adds verbs of
//! its own (`declare_verbs`): MAIL is a call of one of those, checked by the same
//! entry against the schema the program declared.

use schemars::gen::SchemaGenerator;
use schemars::schema::Schema;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A terminal's size in columns and rows.
fn size(_gen: &mut SchemaGenerator) -> Schema {
    serde_json::from_value(serde_json::json!({
        "type": "integer", "minimum": 1, "maximum": 65535,
    }))
    .expect("a size schema")
}

/// The object schema of `Start` and `Run`: the fields of `props` plus `pty`, and
/// the rule that a `command` is given unless the job runs on a terminal (the
/// engine's `anyOf` of `required` lists), so a call naming neither is refused by
/// the engine.
fn command_or_pty(gen: &mut SchemaGenerator, description: &str, mut props: Map<String, Value>) -> Schema {
    let mut pty = serde_json::to_value(gen.subschema_for::<Terminal>()).expect("a terminal schema");
    if let Some(o) = pty.as_object_mut() {
        o.insert("description".into(), Value::from("Run it on a terminal of this size."));
    }
    props.insert("pty".into(), pty);
    serde_json::from_value(serde_json::json!({
        "type": "object",
        "description": description,
        "properties": props,
        "anyOf": [{"required": ["command"]}, {"required": ["pty"]}],
        "additionalProperties": false,
    }))
    .expect("a start schema")
}

/// A terminal to run a job on.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Terminal {
    /// Width in columns.
    #[schemars(schema_with = "size")]
    pub cols: u16,
    /// Height in rows.
    #[schemars(schema_with = "size")]
    pub rows: u16,
}

/// Which stream of a job's output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Stream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
    /// Both (a PTY job has one stream).
    Both,
}

impl Stream {
    /// The stream's name as a job row spells it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Stream::Stdout => "stdout",
            Stream::Stderr => "stderr",
            Stream::Both => "both",
        }
    }
}

/// The image formats a pasted image may be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
pub enum Mime {
    /// PNG.
    #[serde(rename = "image/png")]
    Png,
    /// JPEG.
    #[serde(rename = "image/jpeg")]
    Jpeg,
    /// GIF.
    #[serde(rename = "image/gif")]
    Gif,
    /// WebP.
    #[serde(rename = "image/webp")]
    Webp,
}

impl Mime {
    /// The file extension its scratch file carries.
    #[must_use]
    pub fn ext(self) -> &'static str {
        match self {
            Mime::Png => "png",
            Mime::Jpeg => "jpg",
            Mime::Gif => "gif",
            Mime::Webp => "webp",
        }
    }
}

fn sixty() -> f64 {
    60.0
}
fn two_hundred() -> u64 {
    200
}
fn twenty() -> u64 {
    20
}
fn fifty() -> u64 {
    50
}
fn hundred() -> u64 {
    100
}
fn both() -> Stream {
    Stream::Both
}

/// Tear down every job of this agent before the kernel stops; sent by the kernel at shutdown.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Shutdown {}

/// Spawn a command in the BACKGROUND and return at once with {job_id, status, pty}. The FULL line (command + args) must fullmatch one of the record's `allowed` regexes, or be the shell line the record's `shell` permits. Where it runs and with what environment are the record's, never the call's.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Start {
    /// The executable ALONE (e.g. `grep`). On a PTY start, absent = the child's $SHELL, and with no `args` the record's whole shell line (`$SHELL -l` under shell="login").
    pub command: Option<String>,
    /// Its arguments, one item each.
    pub args: Option<Vec<String>>,
    /// Run it on a terminal of this size.
    pub pty: Option<Terminal>,
}

/// `start`'s declaration: its three fields and the rule that a `command` is given
/// unless the job runs on a terminal (the engine's `anyOf` of `required` lists).
impl JsonSchema for Start {
    fn schema_name() -> String {
        "Start".to_string()
    }

    fn json_schema(gen: &mut SchemaGenerator) -> Schema {
        let mut props = Map::new();
        props.insert("command".into(), serde_json::json!({
            "type": "string", "pattern": "\\S",
            "description": "The executable ALONE (e.g. `grep`). On a PTY start, absent = the child's $SHELL, and with no `args` the record's whole shell line (`$SHELL -l` under shell=\"login\").",
        }));
        props.insert("args".into(), serde_json::json!({
            "type": "array", "items": {"type": "string"}, "description": "Its arguments, one item each.",
        }));
        command_or_pty(gen, "Spawn a command in the BACKGROUND and return at once with {job_id, status, pty}. The FULL line (command + args) must fullmatch one of the record's `allowed` regexes, or be the shell line the record's `shell` permits. Where it runs and with what environment are the record's, never the call's.", props)
    }
}

/// Start, wait, and return the output in ONE reply: {job_id, status, exit_code, lines}. For short commands; a job still running at `wait_s` comes back running (continue with status wait_s / tail). Read files with files.read, not run.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Run {
    /// The executable ALONE.
    pub command: Option<String>,
    /// Its arguments, one item each.
    pub args: Option<Vec<String>>,
    /// Run it on a terminal of this size.
    pub pty: Option<Terminal>,
    /// How many seconds to wait for it to finish.
    #[serde(default = "sixty")]
    pub wait_s: f64,
    /// How many of its last lines come back.
    #[serde(default = "two_hundred")]
    pub tail: u64,
}

/// `run`'s declaration: `start`'s fields, the two waits, and the same rule that a
/// `command` is given unless the job runs on a terminal.
impl JsonSchema for Run {
    fn schema_name() -> String {
        "Run".to_string()
    }

    fn json_schema(gen: &mut SchemaGenerator) -> Schema {
        let mut props = Map::new();
        props.insert("command".into(), serde_json::json!({
            "type": "string", "pattern": "\\S", "description": "The executable ALONE.",
        }));
        props.insert("args".into(), serde_json::json!({
            "type": "array", "items": {"type": "string"}, "description": "Its arguments, one item each.",
        }));
        props.insert("wait_s".into(), serde_json::json!({
            "type": "number", "minimum": 0, "maximum": 600, "default": 60,
            "description": "How many seconds to wait for it to finish.",
        }));
        props.insert("tail".into(), serde_json::json!({
            "type": "integer", "minimum": 1, "maximum": 500, "default": 200,
            "description": "How many of its last lines come back.",
        }));
        command_or_pty(gen, "Start, wait, and return the output in ONE reply: {job_id, status, exit_code, lines}. For short commands; a job still running at `wait_s` comes back running (continue with status wait_s / tail). Read files with files.read, not run.", props)
    }
}

/// One job (with its `cwd`; read its output with `tail`/`grep`), or all of this agent's jobs (summary). A job's id is its kernel task id: `ps` lists it, `cancel task_id=` kills it. Returns {jobs:[…]}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Status {
    /// The job; absent, every job of this agent.
    pub job_id: Option<i64>,
    /// With `job_id`: return as soon as the job is no longer running, or when this many seconds end; use it instead of polling.
    #[schemars(range(min = 0, max = 120))]
    pub wait_s: Option<f64>,
}

/// SIGKILL one job (its whole session: the command and everything it launched) or ALL running jobs on this agent. Returns {killed}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Stop {
    /// The job; absent, every running job.
    pub job_id: Option<i64>,
}

/// SIGINT one job or ALL running jobs on this agent. Returns {interrupted}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Interrupt {
    /// The job; absent, every running job.
    pub job_id: Option<i64>,
}

/// Drop finished jobs (one, or all non-running) from the RAM. Returns {cleared}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Clear {
    /// The job; absent, every finished job.
    pub job_id: Option<i64>,
}

/// Keystrokes or paste into a PTY job, written IN FULL and serialized per job (a bracketed paste is never truncated or interleaved). Exactly one of `data` or `bytes`; `bytes` is written VERBATIM, a trailing newline (Enter) survives. Returns {written}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Write {
    /// The PTY job.
    pub job_id: i64,
    /// Text to type.
    pub data: Option<String>,
    /// Bytes to write verbatim (the binary channel carries them as the request body).
    pub bytes: Option<String>,
}

/// Set a PTY job's window size (the tty sends SIGWINCH, so a TUI redraws). Returns {resized, cols, rows}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Resize {
    /// The PTY job.
    pub job_id: i64,
    /// Width in columns.
    #[schemars(schema_with = "size")]
    pub cols: u16,
    /// Height in rows.
    #[schemars(schema_with = "size")]
    pub rows: u16,
}

/// Flow control from a streaming consumer (an xterm acks each `output` chunk after parsing it): the reader pauses past 100K unacked chars and resumes under 5K. After an `output` replay the reader does not pause for the backlog until this viewer's first ack or its stream ends. Returns {unacked, paused}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Ack {
    /// The PTY job.
    pub job_id: i64,
    /// How many chars the consumer has parsed.
    pub chars: u64,
}

/// The PTY job's recent output — what a client re-attaching replays before the live `output` events; it clears the unacked backlog and opens a re-attach grace. Returns {job_id, output}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Output {
    /// The PTY job.
    pub job_id: i64,
    /// The newest this many bytes; absent, the whole 256 KiB ring.
    pub max_bytes: Option<u64>,
}

/// The last lines a job has printed SO FAR — running or ended, pipe or PTY. Returns {job_id, status, lines:[{n, stream, line}], printed, dropped}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Tail {
    /// The job.
    pub job_id: i64,
    /// How many lines.
    #[serde(default = "twenty")]
    #[schemars(range(min = 1, max = 500))]
    pub lines: u64,
    /// Which stream.
    #[serde(default = "both")]
    pub stream: Stream,
}

/// Search what a job has printed SO FAR — running or ended, pipe or PTY; lines older than the 256 KiB ring are gone (`dropped` says how many). Returns {job_id, status, matches:[{n, stream, line}], total, scanned, dropped}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Grep {
    /// The job.
    pub job_id: i64,
    /// A regex, searched in each line.
    #[serde(rename = "match")]
    #[schemars(length(min = 1))]
    pub pattern: String,
    /// The most matches returned (the LAST ones).
    #[serde(default = "fifty")]
    #[schemars(range(min = 1, max = 500))]
    pub lines: u64,
    /// Which stream.
    #[serde(default = "both")]
    pub stream: Stream,
}

/// Save a pasted image to a per-job scratch file and type its path into the PTY (a drag-drop — a program like claude in a browser xterm cannot reach the browser clipboard). The image is the request body at `data`. Returns {path, bytes}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PasteImage {
    /// The PTY job.
    pub job_id: i64,
    /// The image (the binary channel carries it as the request body).
    pub data: Option<String>,
    /// The image's format.
    pub mime: Mime,
}

/// The reference wire client SOURCE (python): paste `source` verbatim at the top of your program and it gets a `kernel` global (send/emit/watch/on_message). Returns {source}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Connector {}

/// Watch a running job's output AS IT ARRIVES and SEND what matches to `notify` as a turn — it polls nothing. Returns {monitor_id, job_id, notify, match, every, tail, label}; kernel `cancel` (or `unmonitor`) ends it, and it ends with its job.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Monitor {
    /// A RUNNING job of this agent.
    pub job_id: i64,
    /// The agent told.
    pub notify: String,
    /// A regex; absent = every line.
    #[serde(rename = "match")]
    pub pattern: Option<String>,
    /// Seconds: at most one delivery per window; required without `match`.
    #[serde(default)]
    pub every: Option<serde_json::Number>,
    /// The newest lines per delivery.
    #[serde(default = "hundred")]
    pub tail: u64,
    /// A label for the deliveries.
    pub label: Option<String>,
}

/// `monitor`'s declaration: its fields, and the rule that `match` or `every` is
/// given (the engine's `anyOf` of `required` lists) — without `match` every line
/// counts, so the window its deliveries are gathered in must be named.
impl JsonSchema for Monitor {
    fn schema_name() -> String {
        "Monitor".to_string()
    }

    fn json_schema(_gen: &mut SchemaGenerator) -> Schema {
        serde_json::from_value(serde_json::json!({
            "type": "object",
            "description": "Watch a running job's output AS IT ARRIVES and SEND what matches to `notify` as a turn — it polls nothing. Returns {monitor_id, job_id, notify, match, every, tail, label}; kernel `cancel` (or `unmonitor`) ends it, and it ends with its job.",
            "properties": {
                "job_id": {"type": "integer", "description": "A RUNNING job of this agent."},
                "notify": {"type": "string", "minLength": 1, "description": "The agent told."},
                "match": {"type": "string", "minLength": 1, "description": "A regex; absent = every line."},
                "every": {"type": "number", "exclusiveMinimum": 0, "description": "Seconds: at most one delivery per window; required without `match`."},
                "tail": {"type": "integer", "minimum": 1, "default": 100, "description": "The newest lines per delivery."},
                "label": {"type": "string", "minLength": 1, "description": "A label for the deliveries."},
            },
            "required": ["job_id", "notify"],
            "anyOf": [{"required": ["match"]}, {"required": ["every"]}],
            "additionalProperties": false,
        }))
        .expect("a monitor schema")
    }
}

/// This agent's monitors: {monitors:[{monitor_id, job_id, notify, match, every, tail, label, delivered, pending, status, error}]}. `status` is running, or dead once `notify` REFUSED a delivery (`error` says why; nothing more is sent) — `unmonitor` clears it.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Monitors {}

/// End one of this agent's monitors (kernel `cancel` on its id does the same). Returns {unmonitored, monitor_id}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Unmonitor {
    /// The id `monitor` returned.
    pub monitor_id: i64,
}

/// One verb a running program declares.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Declared {
    /// The verb's name.
    #[schemars(length(min = 1))]
    pub name: String,
    /// What the verb does.
    pub description: Option<String>,
    /// The JSON schema of its arguments (`type: object`, `properties`, `required`).
    #[serde(rename = "inputSchema")]
    pub input_schema: Option<Map<String, Value>>,
}

/// The running program declares the verbs of its own: mail to this agent is then a call of one of them, checked against the schema declared here (a verb the agent already answers cannot be redeclared). Replaces the program's earlier declaration; it ends with the program. Sent by the program itself, to its own agent, over its control fd.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeclareVerbs {
    /// The verbs, each `{name, description?, inputSchema?}`.
    pub verbs: Vec<Declared>,
}

fantastic_kernel::verbs! {
    dynamic;
    "shutdown" => Shutdown,
    "start" => Start,
    "run" => Run,
    "status" => Status,
    "stop" => Stop,
    "interrupt" => Interrupt,
    "clear" => Clear,
    "write" => Write,
    "resize" => Resize,
    "ack" => Ack,
    "output" => Output,
    "tail" => Tail,
    "grep" => Grep,
    "paste_image" => PasteImage,
    "connector" => Connector,
    "monitor" => Monitor,
    "monitors" => Monitors,
    "unmonitor" => Unmonitor,
    "declare_verbs" => DeclareVerbs,
}
