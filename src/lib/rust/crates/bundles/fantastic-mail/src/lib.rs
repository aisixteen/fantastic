//! mail — a mailbox as an agent (issue #697): IMAP read, SMTP send, over allowlists.
//!
//! The record names the servers (`imap`, `smtp`), where the login lives
//! (`credentials: {store, ref}`, fields `username` and `password`; read from the
//! store at every use, never kept) and what the agent may touch: the folders it may
//! read (`mailboxes`) and the recipients it may mail (`send_allow`); an empty list
//! allows nothing.
//!
//! `tls: false` (the clear text of a test or local server) is allowed only for a loopback
//! host (`127.0.0.1`, `::1`, `localhost`): a remote host with it is refused before any
//! connection. One `credentials` ref may be bound to both server hosts in the store.
//!
//! `fetch` returns an attachment's bytes (base64) when its `attachment` argument names
//! one, up to 1 MiB (1048576 bytes); a larger one is refused.
//!
//! `list`, `fetch` read; `mark` and `send` write (the kernel's call rows record them).
//! `send` never sends on its own say: it holds an inquiry (confirm) showing the
//! recipients, subject and text, and sends only when a person answers yes through
//! `answer_inquiry`; on no it sends nothing and replies `{sent: false}`. A person is the
//! operator: an answer made by any agent (the asker included) is refused and the
//! inquiry stays open. The recent sends are kept in memory only: they are gone at restart.
//!
//! Provider sign-in (OAuth, XOAUTH2) is not built here: it waits for the OAuth piece
//! shared with MCP authorization (#690). The login is read as one [`Login`]; a token
//! becomes a second variant beside the password, with no change to the record.

#![deny(missing_docs)]

mod imap;
pub mod record;
mod smtp;
#[cfg(test)]
mod test_servers;
pub mod verbs;

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use fantastic_ai_core::credentials::key_reader;
use fantastic_kernel::bundle::{BundleError, Handler, Reply};
use fantastic_kernel::inquiry::{self, Answer, Inquiry, Kind};
use fantastic_kernel::refusal::{self as rf, refusal};
use fantastic_kernel::{current_origin, AgentId, Kernel};
use serde_json::{json, Map, Value};
use tokio::sync::oneshot;

use crate::imap::Fail;
use crate::record::Endpoint;

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "mail.tools";

const NAME: &str = "mail";
const MAN: &str = "A mailbox: it reads mail over IMAP (list, fetch, mark) in the folders its record allows, and sends mail over SMTP only to the recipients its record allows and only after a person answers yes to an inquiry showing what would be sent.";

/// The most recent sends the view shows (kept in memory, since start).
const RECENT: usize = 20;
const VIEW: &str = include_str!("mail_view.html");
/// The verbs the view calls on its own agent: reads, and `watch` (it re-reads on what the agent publishes).
const VIEW_CALLS: [&str; 4] = ["reflect", "list", "inquiries", "watch"];

/// How an account signs in. Only the password (or app password) is built; a provider
/// token (XOAUTH2) is the second variant, once #690's OAuth piece exists.
enum Login {
    Password { username: String, password: String },
}

impl Login {
    fn username(&self) -> &str {
        match self {
            Login::Password { username, .. } => username,
        }
    }
    fn secret(&self) -> &str {
        match self {
            Login::Password { password, .. } => password,
        }
    }
}

/// One mail that was sent.
#[derive(Clone)]
struct Sent {
    to: Vec<String>,
    subject: String,
    ts: f64,
    approved_by: Option<String>,
    message_id: String,
}

/// What an answered inquiry tells the `send` that waits on it.
enum Slot {
    Waiting(oneshot::Sender<Decision>),
    Answered(Decision),
}

#[derive(Clone)]
struct Decision {
    yes: bool,
}

/// The mail bundle: the sends waiting for an answer, and the recent sends, per agent.
#[derive(Default)]
pub struct MailBundle {
    slots: Arc<Mutex<HashMap<(String, i64), Slot>>>,
    sent: Mutex<HashMap<String, VecDeque<Sent>>>,
    counter: AtomicU64,
}

/// Takes a waiting `send`'s slot out when the send ends, however it ends.
struct SlotGuard {
    slots: Arc<Mutex<HashMap<(String, i64), Slot>>>,
    key: (String, i64),
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.slots.lock().expect("slots poisoned").remove(&self.key);
    }
}

fn meta(kernel: &Kernel, id: &AgentId) -> Map<String, Value> {
    kernel.resolve_target(id.as_str())
        .map(|e| e.record().meta.clone())
        .unwrap_or_default()
}

fn failed(f: Fail) -> Value {
    refusal(f.reason, f.message)
}

fn endpoint(m: &Map<String, Value>, field: &str, id: &AgentId) -> Result<Endpoint, Value> {
    let Some(v) = m.get(field).filter(|v| !v.is_null()) else {
        let mut r = refusal(rf::MISSING_ARG, format!("{id}: no `{field}` on the record — name the server (host, port, tls)"));
        rf::with(&mut r, "arg", json!(field));
        return Err(r);
    };
    let ep: Endpoint = serde_json::from_value(v.clone()).map_err(|e| {
        let mut r = refusal(rf::BAD_ARG, format!("{id}: the record's `{field}` is malformed: {e}"));
        rf::with(&mut r, "arg", json!(field));
        r
    })?;
    let host = ep.host.trim_matches(|c| c == '[' || c == ']').to_ascii_lowercase();
    if ep.tls == Some(false) && !["127.0.0.1", "::1", "localhost"].contains(&host.as_str()) {
        let mut r = refusal(rf::BAD_ARG, format!("{id}: the record's `{field}` has tls false for a host that is not this machine — refused, nothing connected"));
        rf::with(&mut r, "arg", json!(field));
        return Err(r);
    }
    Ok(ep)
}

fn list_of(m: &Map<String, Value>, field: &str) -> Vec<String> {
    m.get(field)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

/// The login for `host`, read from the credential store now.
async fn read_login(id: &AgentId, kernel: &Arc<Kernel>, host: &str) -> Result<Login, Value> {
    match key_reader(id, kernel, NAME, true).read_fields(host, &["username", "password"]).await {
        Ok(Some(mut v)) if v.len() == 2 => {
            let password = v.remove(1);
            Ok(Login::Password { username: v.remove(0), password })
        }
        Ok(_) => Err(refusal(rf::UNAUTHORIZED, format!("{id}: no login"))),
        Err(e) => Err(refusal(rf::UNAUTHORIZED, e.0)),
    }
}

fn mailbox_allowed(allowed: &[String], mailbox: &str) -> bool {
    allowed
        .iter()
        .any(|a| a == mailbox || (a.eq_ignore_ascii_case("INBOX") && mailbox.eq_ignore_ascii_case("INBOX")))
}

fn recipient_allowed(allowed: &[String], to: &str) -> bool {
    let to = to.to_ascii_lowercase();
    allowed.iter().any(|a| {
        let a = a.to_ascii_lowercase();
        let domain = a.trim_start_matches('@');
        if a.contains('@') && !a.starts_with('@') {
            to == a
        } else {
            to.rsplit_once('@').is_some_and(|(_, d)| d == domain)
        }
    })
}

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

fn gate_mailbox(id: &AgentId, m: &Map<String, Value>, mailbox: &str) -> Result<(), Value> {
    if mailbox_allowed(&list_of(m, "mailboxes"), mailbox) {
        return Ok(());
    }
    let mut r = refusal(rf::NOT_ALLOWED, format!("{id}: mailbox {mailbox:?} is not in this agent's `mailboxes`"));
    rf::with(&mut r, "arg", json!("mailbox"));
    Err(r)
}

/// Whether the inquiry `re` is still open on `id`.
fn is_open(kernel: &Arc<Kernel>, id: &AgentId, re: i64) -> bool {
    inquiry::list(id, &inquiry::Inquiries { asker: None }, kernel)["inquiries"]
        .as_array()
        .is_some_and(|l| l.iter().any(|q| q["re"] == json!(re)))
}

/// Take inquiry `re` off the agent's open list.
async fn drop_open(kernel: &Arc<Kernel>, id: &AgentId, re: i64) {
    let mut open = meta(kernel, id).remove(inquiry::OPEN_FIELD).and_then(|v| v.as_array().cloned()).unwrap_or_default();
    open.retain(|q| q["re"] != json!(re));
    let mut patch = Map::new();
    patch.insert(inquiry::OPEN_FIELD.to_string(), Value::Array(open));
    let _ = kernel.write_own(id, patch).await;
}



impl MailBundle {
    async fn read(&self, id: &AgentId, kernel: &Arc<Kernel>, op: Read) -> Result<Value, Value> {
        let m = meta(kernel, id);
        let mailbox = match &op {
            Read::List(a) => &a.mailbox,
            Read::Fetch(a) => &a.mailbox,
            Read::Mark(a) => &a.mailbox,
        };
        gate_mailbox(id, &m, mailbox)?;
        let ep = endpoint(&m, "imap", id)?;
        let login = read_login(id, kernel, &ep.host).await?;
        let pass = login.secret();
        let mut s = imap::open(&ep.host, ep.port, ep.tls.unwrap_or(true), login.username(), pass)
            .await
            .map_err(failed)?;
        let out = match op {
            Read::List(a) => {
                imap::list(&mut s, &ep.host, pass, &a.mailbox, a.unseen.unwrap_or(false), a.since.as_deref(), a.limit.unwrap_or(20) as usize).await
            }
            Read::Fetch(a) => imap::fetch(&mut s, &ep.host, pass, &a.mailbox, a.uid, a.attachment.as_deref()).await,
            Read::Mark(a) => {
                if a.seen.is_none() && a.flag.is_none() {
                    Err(Fail::new(rf::MISSING_ARG, "mail: mark needs `seen` or `flag`"))
                } else {
                    imap::mark(&mut s, &ep.host, pass, &a.mailbox, a.uid, a.seen, a.flag).await
                }
            }
        };
        imap::close(s).await;
        out.map_err(failed)
    }

    async fn send(&self, id: &AgentId, kernel: &Arc<Kernel>, a: verbs::Send) -> Result<Value, Value> {
        let m = meta(kernel, id);
        let allow = list_of(&m, "send_allow");
        if let Some(bad) = a.to.iter().find(|t| !recipient_allowed(&allow, t)) {
            let mut r = refusal(rf::NOT_ALLOWED, format!("{id}: recipient {bad:?} is not in this agent's `send_allow` — nothing was asked or sent"));
            rf::with(&mut r, "arg", json!("to"));
            return Err(r);
        }
        let smtp = endpoint(&m, "smtp", id)?;
        // the login must be readable before a person is asked
        read_login(id, kernel, &smtp.host).await?;
        // ask, and hold until the answer
        let text = format!("Send this mail?\nTo: {}\nSubject: {}\n\n{}", a.to.join(", "), a.subject, a.text);
        let asked = inquiry::ask(
            id,
            Inquiry { text, kind: Kind::Confirm, options: None, request_id: None, reply_client_id: inquiry::default_client_id() },
            kernel,
        )
        .await;
        let Some(re) = asked.get("re").and_then(Value::as_i64) else { return Err(asked) };
        let key = (id.0.clone(), re);
        let _guard = SlotGuard { slots: Arc::clone(&self.slots), key: key.clone() };
        let rx = {
            let mut slots = self.slots.lock().expect("slots poisoned");
            match slots.remove(&key) {
                Some(Slot::Answered(d)) => {
                    let (tx, rx) = oneshot::channel();
                    let _ = tx.send(d);
                    rx
                }
                _ => {
                    let (tx, rx) = oneshot::channel();
                    slots.insert(key.clone(), Slot::Waiting(tx));
                    rx
                }
            }
        };
        let d = rx.await.map_err(|_| refusal(rf::TRANSPORT_CLOSED, format!("{id}: the approval was lost — nothing was sent")))?;
        if !d.yes {
            return Ok(json!({"sent": false}));
        }
        // approved: sign in now, and thread the reply
        let login = read_login(id, kernel, &smtp.host).await?;
        let mut thread = None;
        if let Some(uid) = a.reply_to_uid {
            let Some(first) = list_of(&m, "mailboxes").into_iter().next() else {
                return Err(refusal(rf::NOT_ALLOWED, format!("{id}: reply_to_uid needs a mailbox in `mailboxes`")));
            };
            let ep = endpoint(&m, "imap", id)?;
            let il = read_login(id, kernel, &ep.host).await?;
            let mut s = imap::open(&ep.host, ep.port, ep.tls.unwrap_or(true), il.username(), il.secret()).await.map_err(failed)?;
            let t = imap::thread_of(&mut s, &ep.host, il.secret(), &first, uid).await;
            imap::close(s).await;
            thread = Some(t.map_err(failed)?);
        }
        let domain = login.username().rsplit_once('@').map_or("localhost", |(_, d)| d);
        let message_id = format!(
            "{:x}.{}.{}@{domain}",
            (now() * 1e6) as u64,
            std::process::id(),
            self.counter.fetch_add(1, Ordering::Relaxed)
        );
        let out = smtp::Outgoing {
            from: login.username(),
            to: &a.to,
            subject: &a.subject,
            text: &a.text,
            message_id: &message_id,
            in_reply_to: thread.as_ref().map(|(i, r)| (i.as_str(), r.as_slice())),
        };
        smtp::send(&smtp.host, smtp.port, smtp.tls.unwrap_or(true), login.username(), login.secret(), out)
            .await
            .map_err(failed)?;
        let mut sent = self.sent.lock().expect("sent poisoned");
        let q = sent.entry(id.0.clone()).or_default();
        q.push_back(Sent { to: a.to.clone(), subject: a.subject.clone(), ts: now(), approved_by: Some("operator".to_string()), message_id: message_id.clone() });
        while q.len() > RECENT {
            q.pop_front();
        }
        Ok(json!({"sent": true, "message_id": message_id, "to": a.to}))
    }

    async fn answer(&self, id: &AgentId, kernel: &Arc<Kernel>, a: verbs::AnswerInquiry) -> Value {
        // Approval is a person's: an operator send has no origin agent. Any agent (the
        // asker, another AI) answering is refused, and the inquiry stays open.
        if let Some(who) = current_origin() {
            let mut r = refusal(rf::NOT_ALLOWED, format!("{id}: answer_inquiry: only a person answers a send's approval, not the agent {who}"));
            rf::with(&mut r, "arg", json!("re"));
            return r;
        }
        let re = a.re;
        let yes = match &a.answer {
            Answer::Flag(b) => *b,
            other => other.text().eq_ignore_ascii_case("yes"),
        };
        let was_open = is_open(kernel, id, re);
        let out = inquiry::answer(id, a, kernel).await;
        if !was_open {
            return out;
        }
        // The person has answered: the waiting send goes on even when the asker could
        // not be told (the question is then closed here).
        if is_open(kernel, id, re) {
            drop_open(kernel, id, re).await;
        }
        let d = Decision { yes };
        let mut slots = self.slots.lock().expect("slots poisoned");
        match slots.remove(&(id.0.clone(), re)) {
            Some(Slot::Waiting(tx)) => {
                let _ = tx.send(d);
            }
            _ => {
                slots.insert((id.0.clone(), re), Slot::Answered(d));
            }
        }
        out
    }

    fn reflect(&self, id: &AgentId, kernel: &Arc<Kernel>, account: Option<String>) -> Value {
        let m = meta(kernel, id);
        let recent: Vec<Value> = self
            .sent
            .lock()
            .expect("sent poisoned")
            .get(&id.0)
            .map(|q| {
                q.iter()
                    .map(|s| json!({"to": s.to, "subject": s.subject, "ts": s.ts, "approved_by": s.approved_by, "message_id": s.message_id}))
                    .collect()
            })
            .unwrap_or_default();
        let has_login = m.get("credentials").is_some_and(|c| !c.is_null());
        json!({
            "id": id.0,
            "sentence": MAN,
            "account": account,
            "login": if has_login { json!("password") } else { Value::Null },
            "imap": m.get("imap").cloned().unwrap_or(Value::Null),
            "smtp": m.get("smtp").cloned().unwrap_or(Value::Null),
            "mailboxes": list_of(&m, "mailboxes"),
            "send_allow": list_of(&m, "send_allow"),
            "open": inquiry::open_count(kernel, id),
            "recent_sends": recent,
            "view": {
                "kind": "html",
                "bytes": VIEW.replace("{{agent}}", id.as_str()),
                "calls": {id.as_str(): VIEW_CALLS},
            },
        })
    }
}

enum Read {
    List(verbs::List),
    Fetch(verbs::Fetch),
    Mark(verbs::Mark),
}

#[async_trait]
impl Handler for MailBundle {
    type Call = verbs::Call;

    fn name(&self) -> &str {
        NAME
    }

    fn man(&self) -> &'static str {
        MAN
    }

    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        verbs::VERBS
    }

    fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
        verbs::dynamic_verbs(agent)
    }

    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        &record::RECORD
    }

    async fn handle(&self, agent_id: &AgentId, call: Self::Call, kernel: &Arc<Kernel>) -> Result<Reply, BundleError> {
        use verbs::Call;
        let out = match call {
            Call::Reflect(_) => {
                let m = meta(kernel, agent_id);
                let account = match (m.get("imap"), m.get("credentials")) {
                    (Some(_), Some(_)) => match endpoint(&m, "imap", agent_id) {
                        Ok(ep) => read_login(agent_id, kernel, &ep.host).await.ok().map(|l| l.username().to_string()),
                        Err(_) => None,
                    },
                    _ => None,
                };
                self.reflect(agent_id, kernel, account)
            }
            Call::List(a) => self.read(agent_id, kernel, Read::List(a)).await.unwrap_or_else(|e| e),
            Call::Fetch(a) => self.read(agent_id, kernel, Read::Fetch(a)).await.unwrap_or_else(|e| e),
            // WHAT CHANGED, PUBLISHED on the agent's own inbox: `marked {mailbox, uid}` after a
            // mark, `sent {message_id, to}` after a sent mail — its view re-reads that box's
            // count and the recent sends on these, never on a copy of a call
            Call::Mark(a) => {
                let (mailbox, uid) = (a.mailbox.clone(), a.uid);
                let out = self.read(agent_id, kernel, Read::Mark(a)).await.unwrap_or_else(|e| e);
                if out.get("error").is_none() {
                    kernel.emit(agent_id, json!({"type": "marked", "mailbox": mailbox, "uid": uid})).await;
                }
                out
            }
            Call::Send(a) => {
                let out = self.send(agent_id, kernel, a).await.unwrap_or_else(|e| e);
                if out["sent"] == json!(true) {
                    kernel.emit(agent_id, json!({"type": "sent", "message_id": out["message_id"], "to": out["to"]})).await;
                }
                out
            }
            Call::Inquiries(a) => inquiry::list(agent_id, &a, kernel),
            Call::AnswerInquiry(a) => self.answer(agent_id, kernel, a).await,
        };
        Ok(Some(out))
    }
}

#[cfg(test)]
mod tests;
