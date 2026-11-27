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
use fantastic_ai_core::steps::Settle;
use fantastic_kernel::bundle::{BundleError, Handler, Reply};
use fantastic_kernel::inquiry::{self, Answer, Inquiry, Kind};
use fantastic_kernel::refusal::{self as rf, refusal};
use fantastic_kernel::{current_origin, AgentId, Kernel, TaskId};
use serde_json::{json, Map, Value};

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

/// A `send`'s approval: the deferred `send` the person's answer settles, or an
/// answer that came before the `send` deferred.
enum Slot {
    Waiting(Pending),
    Answered(Decision),
}

/// A `send` that asked and deferred its settle (#795): nothing waits on the
/// person; the answer settles it, as a message to its caller.
struct Pending {
    pid: TaskId,
    args: verbs::Send,
}

#[derive(Clone)]
struct Decision {
    yes: bool,
}

/// The mail bundle: the sends waiting for an answer, and the recent sends, per agent.
#[derive(Default, Clone)]
pub struct MailBundle {
    slots: Arc<Mutex<HashMap<(String, i64), Slot>>>,
    sent: Arc<Mutex<HashMap<String, VecDeque<Sent>>>>,
    counter: Arc<AtomicU64>,
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

/// The login for `host`, read from the credential store now — one call to the store,
/// A STEP (#795): `next` gets it, or the refusal.
async fn read_login<R, F, Fut>(id: &AgentId, kernel: &Arc<Kernel>, host: &str, rec: R, next: F)
where
    R: Send + 'static,
    F: FnOnce(R, Result<Login, Value>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let me = id.clone();
    key_reader(id, kernel, NAME, true)
        .read_fields(host, &["username", "password"], rec, move |rec, r| {
            let login = match r {
                Ok(Some(mut v)) if v.len() == 2 => {
                    let password = v.remove(1);
                    Ok(Login::Password { username: v.remove(0), password })
                }
                Ok(_) => Err(refusal(rf::UNAUTHORIZED, format!("{me}: no login"))),
                Err(e) => Err(refusal(rf::UNAUTHORIZED, e.0)),
            };
            next(rec, login)
        })
        .await;
}

/// A step's continuation that takes the outcome of a mail verb.
type Done = Box<dyn FnOnce(Value) -> futures_util::future::BoxFuture<'static, ()> + Send>;

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
    /// A read (`list`, `fetch`, `mark`): the login read (a step), then the IMAP session,
    /// the agent's own I/O under this execution; `settle` settles it. A mark that took
    /// is published on the agent's inbox (`marked`).
    async fn read(&self, id: &AgentId, kernel: &Arc<Kernel>, op: Read, settle: Settle) {
        let m = meta(kernel, id);
        let mailbox = match &op {
            Read::List(a) => &a.mailbox,
            Read::Fetch(a) => &a.mailbox,
            Read::Mark(a) => &a.mailbox,
        };
        if let Err(r) = gate_mailbox(id, &m, mailbox) {
            return settle.reply(r);
        }
        let ep = match endpoint(&m, "imap", id) {
            Ok(ep) => ep,
            Err(r) => return settle.reply(r),
        };
        let (owner, k) = (id.clone(), Arc::clone(kernel));
        let host = ep.host.clone();
        read_login(id, kernel, &host, (op, ep, settle), move |(op, ep, settle), login| async move {
            let login = match login {
                Ok(l) => l,
                Err(r) => return settle.reply(r),
            };
            let forker = settle.forker();
            forker.run(&owner.clone(), "read", async move {
                let out = imap_read(&ep, &login, &op).await.unwrap_or_else(|e| e);
                if let Read::Mark(a) = &op {
                    if out.get("error").is_none() {
                        k.emit(&owner, json!({"type": "marked", "mailbox": a.mailbox, "uid": a.uid})).await;
                    }
                }
                settle.reply(out);
            });
        })
        .await;
    }

    /// `send`: the allowlist, the login read before a person is asked (a step), then
    /// the inquiry; the person's answer settles the call later (nothing waits on them).
    async fn send(&self, id: &AgentId, kernel: &Arc<Kernel>, a: verbs::Send, settle: Settle) {
        let m = meta(kernel, id);
        let allow = list_of(&m, "send_allow");
        if let Some(bad) = a.to.iter().find(|t| !recipient_allowed(&allow, t)) {
            let mut r = refusal(rf::NOT_ALLOWED, format!("{id}: recipient {bad:?} is not in this agent's `send_allow` — nothing was asked or sent"));
            rf::with(&mut r, "arg", json!("to"));
            return settle.reply(r);
        }
        let smtp = match endpoint(&m, "smtp", id) {
            Ok(ep) => ep,
            Err(r) => return settle.reply(r),
        };
        let (me, aid, k) = (self.clone(), id.clone(), Arc::clone(kernel));
        // the login must be readable before a person is asked
        read_login(id, kernel, &smtp.host, (a, settle), move |(a, settle), login| async move {
            if let Err(r) = login {
                return settle.reply(r);
            }
            // ask; the answer settles this call later (nothing waits on the person)
            let text = format!("Send this mail?\nTo: {}\nSubject: {}\n\n{}", a.to.join(", "), a.subject, a.text);
            // the inquiry is held under this call's execution: its pid is the `re`
            let ask = inquiry::ask(
                &aid,
                Inquiry { text, kind: Kind::Confirm, options: None, request_id: None, reply_client_id: inquiry::default_client_id() },
                &k,
            );
            let asked = match settle.pid() {
                Some(pid) => fantastic_kernel::CURRENT_TASK.scope(pid, ask).await,
                None => ask.await,
            };
            let Some(re) = asked.get("re").and_then(Value::as_i64) else { return settle.reply(asked) };
            let key = (aid.0.clone(), re);
            let answered = {
                let mut slots = me.slots.lock().expect("slots poisoned");
                match slots.remove(&key) {
                    Some(Slot::Answered(d)) => Some((d, a)),
                    _ => {
                        if let Some(pid) = settle.pid() {
                            slots.insert(key, Slot::Waiting(Pending { pid, args: a }));
                        }
                        None
                    }
                }
            };
            // answered before this call waited: go on now
            if let Some((d, a)) = answered {
                me.decided(aid, k, a, d.yes, Box::new(move |v| Box::pin(async move { settle.reply(v) }))).await;
            }
        })
        .await;
    }

    /// The person answered a `send`: on yes, sign in (the logins read, steps), send and
    /// record it (the agent's inbox is told `sent`); on no, `{sent: false}`. `done` gets
    /// the outcome.
    async fn decided(&self, id: AgentId, kernel: Arc<Kernel>, a: verbs::Send, yes: bool, done: Done) {
        if !yes {
            return done(json!({"sent": false})).await;
        }
        let m = meta(&kernel, &id);
        let smtp = match endpoint(&m, "smtp", &id) {
            Ok(ep) => ep,
            Err(r) => return done(r).await,
        };
        let me = self.clone();
        let (aid, k) = (id.clone(), Arc::clone(&kernel));
        // approved: sign in now, and thread the reply
        let host = smtp.host.clone();
        read_login(&id, &kernel, &host, (a, done), move |(a, done), login| async move {
            let login = match login {
                Ok(l) => l,
                Err(r) => return done(r).await,
            };
            let Some(uid) = a.reply_to_uid else {
                return me.deliver(&aid, &k, &smtp, &login, a, None, done).await;
            };
            let m = meta(&k, &aid);
            let Some(first) = list_of(&m, "mailboxes").into_iter().next() else {
                return done(refusal(rf::NOT_ALLOWED, format!("{aid}: reply_to_uid needs a mailbox in `mailboxes`"))).await;
            };
            let ep = match endpoint(&m, "imap", &aid) {
                Ok(ep) => ep,
                Err(r) => return done(r).await,
            };
            let (aid2, k2) = (aid.clone(), Arc::clone(&k));
            read_login(&aid, &k, &ep.host.clone(), (a, done, login), move |(a, done, login), il| async move {
                let il = match il {
                    Ok(l) => l,
                    Err(r) => return done(r).await,
                };
                let thread = async {
                    let mut s = imap::open(&ep.host, ep.port, ep.tls.unwrap_or(true), il.username(), il.secret()).await.map_err(failed)?;
                    let t = imap::thread_of(&mut s, &ep.host, il.secret(), &first, uid).await;
                    imap::close(s).await;
                    t.map_err(failed)
                }
                .await;
                match thread {
                    Ok(t) => me.deliver(&aid2, &k2, &smtp, &login, a, Some(t), done).await,
                    Err(r) => done(r).await,
                }
            })
            .await;
        })
        .await;
    }

    /// The approved mail sent over SMTP (its own I/O), recorded and published.
    #[allow(clippy::too_many_arguments)]
    async fn deliver(
        &self,
        id: &AgentId,
        kernel: &Arc<Kernel>,
        smtp: &Endpoint,
        login: &Login,
        a: verbs::Send,
        thread: Option<(String, Vec<String>)>,
        done: Done,
    ) {
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
        if let Err(f) = smtp::send(&smtp.host, smtp.port, smtp.tls.unwrap_or(true), login.username(), login.secret(), out).await {
            return done(failed(f)).await;
        }
        {
            let mut sent = self.sent.lock().expect("sent poisoned");
            let q = sent.entry(id.0.clone()).or_default();
            q.push_back(Sent { to: a.to.clone(), subject: a.subject.clone(), ts: now(), approved_by: Some("operator".to_string()), message_id: message_id.clone() });
            while q.len() > RECENT {
                q.pop_front();
            }
        }
        kernel.emit(id, json!({"type": "sent", "message_id": message_id, "to": a.to})).await;
        done(json!({"sent": true, "message_id": message_id, "to": a.to})).await;
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
        let waiting = {
            let mut slots = self.slots.lock().expect("slots poisoned");
            match slots.remove(&(id.0.clone(), re)) {
                Some(Slot::Waiting(p)) => Some(p),
                _ => {
                    slots.insert((id.0.clone(), re), Slot::Answered(Decision { yes }));
                    None
                }
            }
        };
        // the deferred `send` goes on as the mail agent's own work, and settles
        if let Some(p) = waiting {
            let (me, k, aid) = (self.clone(), Arc::clone(kernel), id.clone());
            kernel.spawn_task(id, "send", async move {
                let k2 = Arc::clone(&k);
                let done: Done = Box::new(move |res| {
                    Box::pin(async move {
                        let status = if fantastic_kernel::is_refused(&res) {
                            fantastic_kernel::Status::Errored
                        } else {
                            fantastic_kernel::Status::Completed
                        };
                        k2.settle_deferred(p.pid, fantastic_kernel::Outcome::new(Some(p.pid), status, res));
                    })
                });
                me.decided(aid, k, p.args, yes, done).await;
            });
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

/// One IMAP session for a read: sign in, the read, sign out.
async fn imap_read(ep: &Endpoint, login: &Login, op: &Read) -> Result<Value, Value> {
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

    /// A deferred `send` cancelled (its deadline, `cancel`): the person's later
    /// answer finds nothing to send.
    async fn on_cancel(&self, agent_id: &AgentId, task_id: &TaskId, _kernel: &Arc<Kernel>) -> Result<(), BundleError> {
        self.slots
            .lock()
            .expect("slots poisoned")
            .retain(|(owner, _), slot| !(owner == &agent_id.0 && matches!(slot, Slot::Waiting(p) if p.pid == *task_id)));
        Ok(())
    }

    async fn handle(&self, agent_id: &AgentId, call: Self::Call, kernel: &Arc<Kernel>) -> Result<Reply, BundleError> {
        use verbs::Call;
        // THE VERBS THAT READ THE LOGIN settle by their steps (#795): the read is a call
        // to the credential store
        let out = match call {
            Call::Reflect(_) => {
                let m = meta(kernel, agent_id);
                let ep = match (m.get("imap"), m.get("credentials")) {
                    (Some(_), Some(_)) => endpoint(&m, "imap", agent_id).ok(),
                    _ => None,
                };
                let Some(ep) = ep else {
                    return Ok(Some(self.reflect(agent_id, kernel, None)));
                };
                let settle = fantastic_ai_core::steps::deferred(kernel);
                let (me, aid, k) = (self.clone(), agent_id.clone(), Arc::clone(kernel));
                read_login(agent_id, kernel, &ep.host, settle, move |settle, login| async move {
                    let account = login.ok().map(|l| l.username().to_string());
                    settle.reply(me.reflect(&aid, &k, account));
                })
                .await;
                return Ok(None);
            }
            // WHAT CHANGED, PUBLISHED on the agent's own inbox: `marked {mailbox, uid}` after a
            // mark, `sent {message_id, to}` after a sent mail — its view re-reads that box's
            // count and the recent sends on these, never on a copy of a call
            Call::List(a) => Read::List(a),
            Call::Fetch(a) => Read::Fetch(a),
            Call::Mark(a) => Read::Mark(a),
            Call::Send(a) => {
                self.send(agent_id, kernel, a, fantastic_ai_core::steps::deferred(kernel)).await;
                return Ok(None);
            }
            Call::Inquiries(a) => return Ok(Some(inquiry::list(agent_id, &a, kernel))),
            Call::AnswerInquiry(a) => return Ok(Some(self.answer(agent_id, kernel, a).await)),
        };
        self.read(agent_id, kernel, out, fantastic_ai_core::steps::deferred(kernel)).await;
        Ok(None)
    }
}

#[cfg(test)]
mod tests;
