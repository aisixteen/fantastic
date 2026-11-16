//! The verbs a mail agent declares (issue #642): one struct per verb, then `verbs!`.
//! `inquiries` and `answer_inquiry` are the kernel's own shapes: a `send` waits on a
//! person's answer to an inquiry this agent holds.

use schemars::JsonSchema;
use serde::Deserialize;

pub use fantastic_kernel::inquiry::{AnswerInquiry, Inquiries};

/// The headers of the messages in a mailbox, newest last. Returns {mailbox, messages:[{uid, from, to, subject, date, flags}]}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct List {
    /// The mailbox; it must be in the record's `mailboxes`.
    #[schemars(length(min = 1))]
    pub mailbox: String,
    /// `true` lists only messages not yet seen.
    pub unseen: Option<bool>,
    /// Only messages on or after this day, `YYYY-MM-DD`.
    #[schemars(regex(pattern = r"^\d{4}-(0[1-9]|1[0-2])-(0[1-9]|[12]\d|3[01])$"))]
    pub since: Option<String>,
    /// At most this many (the newest); default 20.
    #[schemars(range(min = 1, max = 100))]
    pub limit: Option<u32>,
}

/// One message: headers, text body, and the names and sizes of its attachments (their content is not returned). Returns {mailbox, uid, from, to, subject, date, flags, text, attachments:[{name, size}]}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Fetch {
    /// The mailbox; it must be in the record's `mailboxes`.
    #[schemars(length(min = 1))]
    pub mailbox: String,
    /// The message's uid, as `list` gave it.
    #[schemars(range(min = 1))]
    pub uid: u32,
}

/// Mark a message seen or unseen, flagged or unflagged. A write. Returns {mailbox, uid, flags}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Mark {
    /// The mailbox; it must be in the record's `mailboxes`.
    #[schemars(length(min = 1))]
    pub mailbox: String,
    /// The message's uid.
    #[schemars(range(min = 1))]
    pub uid: u32,
    /// `true` marks it seen, `false` unseen.
    pub seen: Option<bool>,
    /// `true` flags it, `false` removes the flag.
    pub flag: Option<bool>,
}

/// Send a mail, only to recipients in the record's `send_allow`, and only after a person says yes to an inquiry showing the recipients, subject and text. A write; a sent mail cannot be taken back. Waits for the answer. Returns {sent: true, message_id, to} (nothing is sent on no: {sent: false}).
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Send {
    /// The recipients, plain addresses.
    #[schemars(length(min = 1), inner(length(min = 1), regex(pattern = r"^[^\s<>,;]+@[^\s<>,;@]+$")))]
    pub to: Vec<String>,
    /// The subject line.
    #[schemars(regex(pattern = r"^[^\r\n]*$"))]
    pub subject: String,
    /// The plain-text body.
    pub text: String,
    /// The uid of a message in the first allowed mailbox this one answers: its Message-ID becomes In-Reply-To.
    #[schemars(range(min = 1))]
    pub reply_to_uid: Option<u32>,
}

fantastic_kernel::verbs! {
    "list" => List,
    "fetch" => Fetch,
    "mark" => Mark,
    "send" => Send,
    "inquiries" => Inquiries,
    "answer_inquiry" => AnswerInquiry,
}
