//! The fields a mail agent's record carries, beside the ones every record has
//! (issue #697).

use schemars::JsonSchema;
use serde::Deserialize;

fn default_tls() -> Option<bool> {
    Some(true)
}

/// One server: where to connect and whether the connection is TLS from the first byte.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    /// The server's host name.
    #[schemars(length(min = 1), regex(pattern = r"^[^\s/?#]+$"))]
    pub host: String,
    /// The server's port.
    #[schemars(range(min = 1, max = 65535))]
    pub port: u16,
    /// `true` (the default) encrypts the connection: IMAP over TLS from the first byte, SMTP over TLS on port 465 or STARTTLS on any other port. `false` sends in the clear: only for a server on this machine.
    #[schemars(default = "default_tls")]
    pub tls: Option<bool>,
}

/// The fields a mail record carries.
#[derive(Debug, JsonSchema)]
pub struct Record {
    /// The IMAP server mail is read from.
    pub imap: Option<Endpoint>,
    /// The SMTP server mail is sent through.
    pub smtp: Option<Endpoint>,
    /// The credential store and ref holding the login: the fields `username` and `password` (an app password where the provider needs one).
    pub credentials: Option<fantastic_ai_core::record::Credentials>,
    /// The folders the agent may read or mark, by name; none listed, none allowed.
    #[schemars(inner(length(min = 1)))]
    pub mailboxes: Option<Vec<String>>,
    /// The recipients `send` may mail: a full address, or a domain (`example.com`, which allows its addresses and not its subdomains); none listed, none allowed.
    #[schemars(inner(length(min = 1), regex(pattern = r"^[^\s<>,;]+$")))]
    pub send_allow: Option<Vec<String>>,
    /// The sends waiting for a person's answer (the kernel's inquiry list).
    pub open_inquiries: Option<Vec<fantastic_kernel::inquiry::Held>>,
}

fantastic_kernel::record!(Record);
