//! The SMTP side: one submission per `send`.

use lettre::address::Address;
use lettre::message::{header::ContentType, Mailbox, Message};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};

use crate::imap::{scrub, Fail};
use fantastic_kernel::refusal::{BAD_ARG, TRANSPORT_ERROR, UNAUTHORIZED};

/// What is sent.
pub(crate) struct Outgoing<'a> {
    pub from: &'a str,
    pub to: &'a [String],
    pub subject: &'a str,
    pub text: &'a str,
    pub message_id: &'a str,
    pub in_reply_to: Option<(&'a str, &'a [String])>,
}

/// Submit the message; Ok when the server accepted it.
pub(crate) async fn send(host: &str, port: u16, tls: bool, user: &str, pass: &str, m: Outgoing<'_>) -> Result<(), Fail> {
    let bad = |what: &str, e: &dyn std::fmt::Display| Fail::new(BAD_ARG, format!("mail: {what}: {e}"));
    let from: Address = m.from.parse().map_err(|e| bad("the stored username is not a mail address", &e))?;
    let mut b = Message::builder()
        .from(Mailbox::new(None, from))
        .subject(m.subject)
        .message_id(Some(format!("<{}>", m.message_id)))
        .header(ContentType::TEXT_PLAIN);
    for t in m.to {
        let a: Address = t.parse().map_err(|e| bad("recipient", &e))?;
        b = b.to(Mailbox::new(None, a));
    }
    if let Some((id, refs)) = m.in_reply_to {
        if !id.is_empty() {
            b = b.in_reply_to(format!("<{id}>"));
            let mut all: Vec<String> = refs.iter().map(|r| format!("<{r}>")).collect();
            all.push(format!("<{id}>"));
            b = b.references(all.join(" "));
        }
    }
    let msg = b.body(m.text.to_string()).map_err(|e| bad("message", &e))?;
    let builder = if !tls {
        AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host)
    } else if port == 465 {
        AsyncSmtpTransport::<Tokio1Executor>::relay(host).map_err(|e| bad("smtp host", &e))?
    } else {
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host).map_err(|e| bad("smtp host", &e))?
    };
    let transport = builder
        .port(port)
        .credentials(Credentials::new(user.to_string(), pass.to_string()))
        .build();
    transport.send(msg).await.map(|_| ()).map_err(|e| {
        let text = scrub(&e.to_string(), pass);
        if e.is_permanent() && text.contains("535") {
            Fail::new(UNAUTHORIZED, format!("mail: SMTP login refused by {host} — check the stored username and password"))
        } else {
            Fail::new(TRANSPORT_ERROR, format!("mail: SMTP send via {host}: {text}"))
        }
    })
}
