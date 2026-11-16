//! The IMAP side: one short session per call (connect, log in, select, act, log out).
//! Nothing is kept between calls, so a changed password is used by the next one.

use std::fmt;
use std::sync::Arc;

use async_imap::types::Flag;
use base64::Engine;
use futures_util::TryStreamExt;
use mail_parser::{MessageParser, MimeHeaders};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

/// The largest message `fetch` reads whole.
pub(crate) const MAX_MESSAGE: usize = 10 * 1024 * 1024;
/// The most bytes of one attachment `fetch` returns.
pub(crate) const MAX_ATTACHMENT: usize = 1024 * 1024;

/// Any stream an IMAP session runs over.
pub(crate) trait Io: AsyncRead + AsyncWrite + Unpin + Send + fmt::Debug {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + fmt::Debug> Io for T {}
type Conn = Box<dyn Io>;
pub(crate) type Session = async_imap::Session<Conn>;

/// A failure, as the kernel's refusal reason it settles with and its text.
#[derive(Debug)]
pub(crate) struct Fail {
    pub reason: &'static str,
    pub message: String,
}

impl Fail {
    pub fn new(reason: &'static str, message: impl Into<String>) -> Self {
        Self { reason, message: message.into() }
    }
}

/// An error's text with the secret taken out of it.
pub(crate) fn scrub(text: &str, secret: &str) -> String {
    if secret.is_empty() {
        text.to_string()
    } else {
        text.replace(secret, "[secret]")
    }
}

pub(crate) fn tls_config() -> Arc<ClientConfig> {
    let roots = RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    Arc::new(
        ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("ring supports the default protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

/// Connect and log in. `secret` is what error text is scrubbed of.
pub(crate) async fn open(host: &str, port: u16, tls: bool, user: &str, pass: &str) -> Result<Session, Fail> {
    let tcp = TcpStream::connect((host, port))
        .await
        .map_err(|e| Fail::new(fantastic_kernel::refusal::TRANSPORT_ERROR, format!("mail: IMAP connect to {host}:{port}: {e}")))?;
    let conn: Conn = if tls {
        let name = ServerName::try_from(host.to_string())
            .map_err(|e| Fail::new(fantastic_kernel::refusal::BAD_ARG, format!("mail: imap host {host}: {e}")))?;
        let s = tokio_rustls::TlsConnector::from(tls_config())
            .connect(name, tcp)
            .await
            .map_err(|e| Fail::new(fantastic_kernel::refusal::TRANSPORT_ERROR, format!("mail: IMAP TLS to {host}:{port}: {e}")))?;
        Box::new(s)
    } else {
        Box::new(tcp)
    };
    let mut client = async_imap::Client::new(conn);
    client
        .read_response()
        .await
        .map_err(|e| Fail::new(fantastic_kernel::refusal::TRANSPORT_ERROR, scrub(&format!("mail: IMAP greeting from {host}: {e}"), pass)))?;
    match client.login(user, pass).await {
        Ok(s) => Ok(s),
        // the server's words are not repeated: they may echo the login
        Err(_) => Err(Fail::new(
            fantastic_kernel::refusal::UNAUTHORIZED,
            format!("mail: IMAP login refused by {host} — check the stored username and password"),
        )),
    }
}

fn imap_err(host: &str, what: &str, e: impl fmt::Display, pass: &str) -> Fail {
    Fail::new(
        fantastic_kernel::refusal::TRANSPORT_ERROR,
        scrub(&format!("mail: IMAP {what} on {host}: {e}"), pass),
    )
}

fn flag_name(f: &Flag<'_>) -> String {
    match f {
        Flag::Seen => "\\Seen".into(),
        Flag::Answered => "\\Answered".into(),
        Flag::Flagged => "\\Flagged".into(),
        Flag::Deleted => "\\Deleted".into(),
        Flag::Draft => "\\Draft".into(),
        Flag::Recent => "\\Recent".into(),
        Flag::MayCreate => "\\*".into(),
        Flag::Custom(c) => c.to_string(),
    }
}

fn addresses(a: Option<&mail_parser::Address<'_>>) -> Vec<String> {
    a.map(|a| {
        a.iter()
            .filter_map(|x| x.address().map(str::to_string))
            .collect()
    })
    .unwrap_or_default()
}

fn headers(raw: &[u8]) -> (Vec<String>, Vec<String>, String, Option<String>) {
    match MessageParser::default().parse(raw) {
        Some(m) => (
            addresses(m.from()),
            addresses(m.to()),
            m.subject().unwrap_or_default().to_string(),
            m.date().map(mail_parser::DateTime::to_rfc3339),
        ),
        None => (vec![], vec![], String::new(), None),
    }
}

/// `list`: the headers of the newest `limit` messages that match.
pub(crate) async fn list(
    s: &mut Session,
    host: &str,
    pass: &str,
    mailbox: &str,
    unseen: bool,
    since: Option<&str>,
    limit: usize,
) -> Result<Value, Fail> {
    s.examine(mailbox).await.map_err(|e| imap_err(host, "select", e, pass))?;
    let mut q = Vec::new();
    if unseen {
        q.push("UNSEEN".to_string());
    }
    if let Some(d) = since {
        q.push(format!("SINCE {}", imap_date(d)));
    }
    let query = if q.is_empty() { "ALL".to_string() } else { q.join(" ") };
    let mut uids: Vec<u32> = s
        .uid_search(&query)
        .await
        .map_err(|e| imap_err(host, "search", e, pass))?
        .into_iter()
        .collect();
    uids.sort_unstable();
    let from = uids.len().saturating_sub(limit);
    let uids = &uids[from..];
    let mut messages = Vec::new();
    if !uids.is_empty() {
        let set = uids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
        let fetched: Vec<_> = s
            .uid_fetch(&set, "(UID FLAGS BODY.PEEK[HEADER])")
            .await
            .map_err(|e| imap_err(host, "fetch", e, pass))?
            .try_collect()
            .await
            .map_err(|e| imap_err(host, "fetch", e, pass))?;
        for f in &fetched {
            let (from, to, subject, date) = headers(f.header().unwrap_or_default());
            messages.push(json!({
                "uid": f.uid, "from": from, "to": to, "subject": subject, "date": date,
                "flags": f.flags().map(|x| flag_name(&x)).collect::<Vec<_>>(),
            }));
        }
        messages.sort_by_key(|m| m["uid"].as_u64());
    }
    Ok(json!({"mailbox": mailbox, "messages": messages}))
}

/// `YYYY-MM-DD` as IMAP's `D-Mon-YYYY` (the verb's schema checked the shape).
fn imap_date(d: &str) -> String {
    const MON: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let (y, rest) = d.split_at(4);
    let m: usize = rest[1..3].parse().unwrap_or(1);
    let day: u32 = rest[4..6].parse().unwrap_or(1);
    format!("{day}-{}-{y}", MON[m.clamp(1, 12) - 1])
}

/// The whole message `uid`, raw, with its flags.
async fn raw(s: &mut Session, host: &str, pass: &str, mailbox: &str, uid: u32, examine: bool) -> Result<(Vec<u8>, Vec<String>), Fail> {
    if examine {
        s.examine(mailbox).await.map_err(|e| imap_err(host, "select", e, pass))?;
    } else {
        s.select(mailbox).await.map_err(|e| imap_err(host, "select", e, pass))?;
    }
    let size: Vec<_> = s
        .uid_fetch(uid.to_string(), "(UID RFC822.SIZE)")
        .await
        .map_err(|e| imap_err(host, "fetch", e, pass))?
        .try_collect()
        .await
        .map_err(|e| imap_err(host, "fetch", e, pass))?;
    let Some(first) = size.first() else {
        return Err(Fail::new(fantastic_kernel::refusal::NOT_FOUND, format!("mail: no message {uid} in {mailbox}")));
    };
    if first.size.is_some_and(|n| n as usize > MAX_MESSAGE) {
        return Err(Fail::new(
            fantastic_kernel::refusal::LIMIT,
            format!("mail: message {uid} is larger than {MAX_MESSAGE} bytes — not fetched"),
        ));
    }
    let got: Vec<_> = s
        .uid_fetch(uid.to_string(), "(UID FLAGS BODY.PEEK[])")
        .await
        .map_err(|e| imap_err(host, "fetch", e, pass))?
        .try_collect()
        .await
        .map_err(|e| imap_err(host, "fetch", e, pass))?;
    let Some(f) = got.first() else {
        return Err(Fail::new(fantastic_kernel::refusal::NOT_FOUND, format!("mail: no message {uid} in {mailbox}")));
    };
    let flags = f.flags().map(|x| flag_name(&x)).collect();
    Ok((f.body().unwrap_or_default().to_vec(), flags))
}

/// `fetch`: one message.
pub(crate) async fn fetch(s: &mut Session, host: &str, pass: &str, mailbox: &str, uid: u32, attachment: Option<&str>) -> Result<Value, Fail> {
    let (bytes, flags) = raw(s, host, pass, mailbox, uid, true).await?;
    let Some(m) = MessageParser::default().parse(&bytes) else {
        return Err(Fail::new(fantastic_kernel::refusal::TRANSPORT_ERROR, format!("mail: message {uid} could not be read")));
    };
    let attachments: Vec<Value> = m
        .attachments()
        .map(|a| json!({"name": a.attachment_name().unwrap_or("(unnamed)"), "size": a.contents().len()}))
        .collect();
    let mut out = json!({
        "mailbox": mailbox, "uid": uid,
        "from": addresses(m.from()), "to": addresses(m.to()),
        "subject": m.subject().unwrap_or_default(),
        "date": m.date().map(mail_parser::DateTime::to_rfc3339),
        "flags": flags,
        "text": m.body_text(0).map(|t| t.into_owned()).unwrap_or_default(),
        "attachments": attachments,
    });
    if let Some(name) = attachment {
        let Some(a) = m.attachments().find(|a| a.attachment_name().unwrap_or("(unnamed)") == name) else {
            return Err(Fail::new(fantastic_kernel::refusal::NOT_FOUND, format!("mail: message {uid} has no attachment named {name:?}")));
        };
        let bytes = a.contents();
        if bytes.len() > MAX_ATTACHMENT {
            return Err(Fail::new(
                fantastic_kernel::refusal::LIMIT,
                format!("mail: attachment {name:?} is {} bytes, over the {MAX_ATTACHMENT}-byte cap — not returned", bytes.len()),
            ));
        }
        out["attachment"] = json!({"name": name, "size": bytes.len(), "base64": base64::engine::general_purpose::STANDARD.encode(bytes)});
    }
    Ok(out)
}

/// The Message-ID and References of message `uid`, for a reply's threading.
pub(crate) async fn thread_of(s: &mut Session, host: &str, pass: &str, mailbox: &str, uid: u32) -> Result<(String, Vec<String>), Fail> {
    s.examine(mailbox).await.map_err(|e| imap_err(host, "select", e, pass))?;
    let got: Vec<_> = s
        .uid_fetch(uid.to_string(), "(UID BODY.PEEK[HEADER])")
        .await
        .map_err(|e| imap_err(host, "fetch", e, pass))?
        .try_collect()
        .await
        .map_err(|e| imap_err(host, "fetch", e, pass))?;
    let Some(m) = got.first().and_then(|f| f.header()).and_then(|h| MessageParser::default().parse(h)) else {
        return Err(Fail::new(fantastic_kernel::refusal::NOT_FOUND, format!("mail: no message {uid} in {mailbox} to reply to")));
    };
    let id = m.message_id().unwrap_or_default().to_string();
    let refs = m.references().as_text_list().map(|l| l.iter().map(ToString::to_string).collect()).unwrap_or_default();
    Ok((id, refs))
}

/// `mark`: set or clear `\Seen` and `\Flagged`; the flags after.
pub(crate) async fn mark(
    s: &mut Session,
    host: &str,
    pass: &str,
    mailbox: &str,
    uid: u32,
    seen: Option<bool>,
    flag: Option<bool>,
) -> Result<Value, Fail> {
    s.select(mailbox).await.map_err(|e| imap_err(host, "select", e, pass))?;
    for (on, name) in [(seen, "\\Seen"), (flag, "\\Flagged")] {
        let Some(on) = on else { continue };
        let sign = if on { '+' } else { '-' };
        let _: Vec<_> = s
            .uid_store(uid.to_string(), format!("{sign}FLAGS.SILENT ({name})"))
            .await
            .map_err(|e| imap_err(host, "store", e, pass))?
            .try_collect()
            .await
            .map_err(|e| imap_err(host, "store", e, pass))?;
    }
    let got: Vec<_> = s
        .uid_fetch(uid.to_string(), "(UID FLAGS)")
        .await
        .map_err(|e| imap_err(host, "fetch", e, pass))?
        .try_collect()
        .await
        .map_err(|e| imap_err(host, "fetch", e, pass))?;
    let Some(f) = got.first() else {
        return Err(Fail::new(fantastic_kernel::refusal::NOT_FOUND, format!("mail: no message {uid} in {mailbox}")));
    };
    Ok(json!({"mailbox": mailbox, "uid": uid, "flags": f.flags().map(|x| flag_name(&x)).collect::<Vec<_>>()}))
}

/// Log out, ignoring what the server answers.
pub(crate) async fn close(mut s: Session) {
    let _ = s.logout().await;
}
