//! The mail agent against in-process fake IMAP and SMTP servers, through a real
//! credential store (file backend under a temp dir): list, fetch, mark, the mailbox
//! and recipient allowlists, the approval of every send, and the login never shown.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fantastic_credential_store::test_support as creds;
use fantastic_kernel::{Agent, AgentId, Item, Kernel, Outcome, Status};
use serde_json::{json, Map, Value};

use crate::test_servers::{self as fake, ImapState, Msg, SmtpState};
use crate::{MailBundle, HANDLER_MODULE};

struct Rig {
    kernel: Arc<Kernel>,
    imap: Arc<Mutex<ImapState>>,
    smtp: Arc<Mutex<SmtpState>>,
    _tmp: tempfile::TempDir,
}

fn msg(uid: u32, flags: &[&str], raw: Vec<u8>) -> Msg {
    Msg { uid, flags: flags.iter().map(|f| (*f).to_string()).collect(), raw }
}

async fn rig() -> Rig {
    rig_with(fake::PASS, |_| {}).await
}

/// A rig whose stored password is `password` and whose record `tweak` may change.
async fn rig_with(password: &str, tweak: impl FnOnce(&mut Map<String, Value>)) -> Rig {
    let imap = Arc::new(Mutex::new(ImapState::default()));
    {
        let mut st = imap.lock().unwrap();
        st.boxes.insert(
            "INBOX".into(),
            vec![
                msg(1, &["\\Seen"], fake::message("ann@example.org", "Hello", "m1@x", "first body")),
                msg(2, &[], fake::message("bob@example.org", "Lunch?", "m2@x", "noon at the usual")),
                msg(3, &[], fake::with_attachment("cy@example.org", "Plan", "m3@x")),
            ],
        );
        st.boxes.insert("Archive".into(), vec![msg(7, &["\\Seen"], fake::message("old@example.org", "Old", "m7@x", "old"))]);
        st.boxes.insert("Secret".into(), vec![msg(9, &[], fake::message("x@example.org", "Hidden", "m9@x", "nope"))]);
    }
    let smtp = Arc::new(Mutex::new(SmtpState::default()));
    let (iport, sport) = (fake::imap(Arc::clone(&imap)).await, fake::smtp(Arc::clone(&smtp)).await);

    let tmp = tempfile::TempDir::new().unwrap();
    let mut kernel = Kernel::new();
    kernel.bundles.register(fantastic_credential_store::HANDLER_MODULE, creds::bundle());
    kernel.bundles.register(HANDLER_MODULE, MailBundle::default());
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        json!({"guid": "g1"}).as_object().cloned().unwrap(),
        tmp.path().join(".fantastic"),
        false,
    );
    fantastic_kernel::test_support::register(&kernel, Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));

    // the login: a ref bound to the local host, holding username and password
    let store = creds::ensure_store(&kernel, tmp.path(), "mail").await;
    let declared = kernel.operator_send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "update_agent", "id": store.0, "refs": {"main": {"host": ["127.0.0.1", "localhost"]}}}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(declared["updated"], json!(true), "{declared}");
    let set = kernel.operator_send(vec![Item::new(store, json!({"type": "set", "ref": "main", "fields": {"username": fake::USER, "password": password}}))])
        .await
        .remove(0)
        .result;
    assert_eq!(set["stored"], json!(true), "{set}");

    let mut meta = Map::new();
    meta.insert("imap".into(), json!({"host": "127.0.0.1", "port": iport, "tls": false}));
    // one ref serves both servers: the SMTP one is named by another host
    meta.insert("smtp".into(), json!({"host": "localhost", "port": sport, "tls": false}));
    meta.insert("mailboxes".into(), json!(["INBOX", "Archive"]));
    meta.insert("send_allow".into(), json!(["friend@example.org", "corp.test"]));
    creds::add_credentials(&mut meta, "main");
    tweak(&mut meta);
    fantastic_kernel::test_support::register(&kernel, Agent::new(
        AgentId::from("mail"),
        Some(HANDLER_MODULE.to_string()),
        Some(root.id.clone()),
        meta,
        PathBuf::new(),
        false,
    ));
    Rig { kernel, imap, smtp, _tmp: tmp }
}

async fn send(k: &Arc<Kernel>, payload: Value) -> Outcome {
    k.operator_send(vec![Item::new(AgentId::from("mail"), payload)]).await.remove(0)
}

fn reason(o: &Outcome) -> String {
    o.result["reason"].as_str().unwrap_or("").to_string()
}

fn no_secret(v: &Value) {
    let text = v.to_string();
    assert!(!text.contains(fake::PASS), "the password is in {text}");
    assert!(!text.contains("invalid login"), "a server's words are repeated in {text}");
}

/// Wait for the inquiry `send` raised, check the SMTP server was not touched, answer it.
async fn approve(r: &Rig, answer: Value) -> String {
    for _ in 0..500 {
        let q = send(&r.kernel, json!({"type": "inquiries"})).await.result;
        if let Some(first) = q["inquiries"].get(0) {
            let text = first["text"].as_str().unwrap().to_string();
            assert_eq!(first["kind"], json!("confirm"));
            assert!(r.smtp.lock().unwrap().mails.is_empty(), "sent before the answer");
            let out = send(&r.kernel, json!({"type": "answer_inquiry", "re": first["re"], "answer": answer})).await;
            // an operator's inquiry has no asker to tell; the send goes on regardless
            assert!(out.result.get("answered").is_some() || out.result.get("error").is_some(), "{}", out.result);
            return text;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no inquiry was raised");
}

#[tokio::test]
async fn list_fetch_and_mark() {
    let r = rig().await;
    let all = send(&r.kernel, json!({"type": "list", "mailbox": "INBOX"})).await;
    assert_eq!(all.status, Status::Completed, "{}", all.result);
    let msgs = all.result["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[1]["uid"], json!(2));
    assert_eq!(msgs[1]["from"], json!(["bob@example.org"]));
    assert_eq!(msgs[1]["to"], json!(["me@corp.test"]));
    assert_eq!(msgs[1]["subject"], json!("Lunch?"));
    assert_eq!(msgs[1]["flags"], json!([]));
    assert_eq!(msgs[0]["flags"], json!(["\\Seen"]));
    assert!(msgs[1]["date"].as_str().unwrap().starts_with("2026-11-16T10:00:00"));

    let unseen = send(&r.kernel, json!({"type": "list", "mailbox": "INBOX", "unseen": true})).await;
    assert_eq!(unseen.result["messages"].as_array().unwrap().len(), 2);
    let limited = send(&r.kernel, json!({"type": "list", "mailbox": "INBOX", "limit": 1})).await;
    assert_eq!(limited.result["messages"][0]["uid"], json!(3), "the newest");

    let f = send(&r.kernel, json!({"type": "fetch", "mailbox": "INBOX", "uid": 3})).await.result;
    assert_eq!(f["subject"], json!("Plan"));
    assert_eq!(f["text"].as_str().unwrap().trim(), "see attached");
    assert_eq!(f["attachments"], json!([{"name": "plan.pdf", "size": 8}]));
    let missing = send(&r.kernel, json!({"type": "fetch", "mailbox": "INBOX", "uid": 99})).await;
    assert_eq!(reason(&missing), "not_found", "{}", missing.result);

    let m = send(&r.kernel, json!({"type": "mark", "mailbox": "INBOX", "uid": 2, "seen": true, "flag": true})).await;
    assert_eq!(m.status, Status::Completed, "{}", m.result);
    let flags = m.result["flags"].as_array().unwrap().clone();
    assert!(flags.contains(&json!("\\Seen")) && flags.contains(&json!("\\Flagged")), "{flags:?}");
    let unseen = send(&r.kernel, json!({"type": "list", "mailbox": "INBOX", "unseen": true})).await;
    assert_eq!(unseen.result["messages"].as_array().unwrap().len(), 1);
    let back = send(&r.kernel, json!({"type": "mark", "mailbox": "INBOX", "uid": 2, "seen": false})).await;
    assert!(!back.result["flags"].as_array().unwrap().contains(&json!("\\Seen")));
    let none = send(&r.kernel, json!({"type": "mark", "mailbox": "INBOX", "uid": 2})).await;
    assert_eq!(reason(&none), "missing_arg", "{}", none.result);
    // fetch reads without marking seen
    send(&r.kernel, json!({"type": "fetch", "mailbox": "INBOX", "uid": 2})).await;
    let st = r.imap.lock().unwrap();
    assert!(!st.boxes["INBOX"][1].flags.contains(&"\\Seen".to_string()));
}

#[tokio::test]
async fn a_mailbox_outside_the_list_is_refused() {
    let r = rig().await;
    for verb in [
        json!({"type": "list", "mailbox": "Secret"}),
        json!({"type": "fetch", "mailbox": "Secret", "uid": 9}),
        json!({"type": "mark", "mailbox": "Secret", "uid": 9, "seen": true}),
    ] {
        let o = send(&r.kernel, verb).await;
        assert_eq!(reason(&o), "not_allowed", "{}", o.result);
    }
    assert!(r.imap.lock().unwrap().commands.is_empty(), "the server was not even asked");
    // an agent with no list allows nothing
    let r = rig_with(fake::PASS, |m| {
        m.remove("mailboxes");
    })
    .await;
    assert_eq!(reason(&send(&r.kernel, json!({"type": "list", "mailbox": "INBOX"})).await), "not_allowed");
}

#[tokio::test]
async fn send_waits_for_the_inquiry_and_sends_on_yes() {
    let r = rig().await;
    let payload = json!({"type": "send", "to": ["friend@example.org", "Pat@Corp.Test"], "subject": "Re: Lunch?", "text": "Noon works.\nSee you.", "reply_to_uid": 2});
    let (out, asked) = tokio::join!(send(&r.kernel, payload), approve(&r, json!(true)));
    assert_eq!(out.status, Status::Completed, "{}", out.result);
    assert_eq!(out.result["sent"], json!(true));
    let id = out.result["message_id"].as_str().unwrap().to_string();
    // the person saw recipients, subject and text
    assert!(asked.contains("friend@example.org") && asked.contains("Pat@Corp.Test"), "{asked}");
    assert!(asked.contains("Re: Lunch?") && asked.contains("Noon works."), "{asked}");
    {
        let st = r.smtp.lock().unwrap();
        assert_eq!(st.mails.len(), 1);
        let m = &st.mails[0];
        assert_eq!(m.from, fake::USER);
        assert_eq!(m.rcpt, vec!["friend@example.org", "Pat@Corp.Test"]);
        assert!(m.data.contains("Subject: Re: Lunch?") && m.data.contains("Noon works."), "{}", m.data);
        assert!(m.data.contains(&format!("Message-ID: <{id}>")), "{}", m.data);
        assert!(m.data.contains("In-Reply-To: <m2@x>"), "{}", m.data);
    }
    // the answered question is closed, and the send is on the record's recent list
    let q = send(&r.kernel, json!({"type": "inquiries"})).await.result;
    assert_eq!(q["inquiries"], json!([]));
    let refl = send(&r.kernel, json!({"type": "reflect"})).await.result;
    assert_eq!(refl["recent_sends"][0]["subject"], json!("Re: Lunch?"));
    assert_eq!(refl["recent_sends"][0]["approved_by"], json!("operator"));
    no_secret(&refl);
}

#[tokio::test]
async fn send_sends_nothing_on_no() {
    let r = rig().await;
    let payload = json!({"type": "send", "to": ["friend@example.org"], "subject": "Hi", "text": "x"});
    let (out, _) = tokio::join!(send(&r.kernel, payload), approve(&r, json!(false)));
    assert_eq!(out.status, Status::Completed, "{}", out.result);
    assert_eq!(out.result, json!({"sent": false}));
    assert!(r.smtp.lock().unwrap().mails.is_empty());
    let refl = send(&r.kernel, json!({"type": "reflect"})).await.result;
    assert_eq!(refl["recent_sends"], json!([]));
    // words other than yes are no, too
    let payload = json!({"type": "send", "to": ["friend@example.org"], "subject": "Hi", "text": "x"});
    let (out, _) = tokio::join!(send(&r.kernel, payload), approve(&r, json!("maybe later")));
    assert_eq!(out.result, json!({"sent": false}));
    assert!(r.smtp.lock().unwrap().mails.is_empty());
}

#[tokio::test]
async fn send_refuses_a_recipient_outside_send_allow_without_asking() {
    let r = rig().await;
    for to in [json!(["friend@example.org", "stranger@evil.test"]), json!(["x@sub.corp.test"]), json!(["other@example.org"])] {
        let o = send(&r.kernel, json!({"type": "send", "to": to, "subject": "s", "text": "t"})).await;
        assert_eq!(reason(&o), "not_allowed", "{}", o.result);
    }
    let q = send(&r.kernel, json!({"type": "inquiries"})).await.result;
    assert_eq!(q["inquiries"], json!([]), "no one was asked");
    assert!(r.smtp.lock().unwrap().mails.is_empty());
    // a malformed address never reaches the agent
    let o = send(&r.kernel, json!({"type": "send", "to": ["not an address"], "subject": "s", "text": "t"})).await;
    assert_eq!(reason(&o), "bad_arg", "{}", o.result);
    // no list allows nobody
    let r = rig_with(fake::PASS, |m| {
        m.remove("send_allow");
    })
    .await;
    let o = send(&r.kernel, json!({"type": "send", "to": ["friend@example.org"], "subject": "s", "text": "t"})).await;
    assert_eq!(reason(&o), "not_allowed");
}

#[tokio::test]
async fn the_login_is_never_echoed() {
    // a wrong password: the servers repeat it, the agent does not
    let r = rig_with("wrong-pass-5521", |_| {}).await;
    let o = send(&r.kernel, json!({"type": "list", "mailbox": "INBOX"})).await;
    assert_eq!(reason(&o), "unauthorized", "{}", o.result);
    assert!(!o.result.to_string().contains("wrong-pass-5521"), "{}", o.result);
    assert!(!o.result.to_string().contains("invalid login"), "{}", o.result);
    let payload = json!({"type": "send", "to": ["friend@example.org"], "subject": "s", "text": "t"});
    let (o, _) = tokio::join!(send(&r.kernel, payload), approve(&r, json!(true)));
    assert_eq!(reason(&o), "unauthorized", "{}", o.result);
    assert!(!o.result.to_string().contains("wrong-pass-5521"), "{}", o.result);
    assert!(!o.result.to_string().contains("bad credentials"), "{}", o.result);
    assert_eq!(r.smtp.lock().unwrap().auth_failures, 1);
    // and a good one appears in no reply
    let r = rig().await;
    for v in [
        send(&r.kernel, json!({"type": "list", "mailbox": "INBOX"})).await.result,
        send(&r.kernel, json!({"type": "fetch", "mailbox": "INBOX", "uid": 1})).await.result,
        send(&r.kernel, json!({"type": "reflect"})).await.result,
    ] {
        no_secret(&v);
    }
}

#[tokio::test]
async fn reflect_says_how_it_is_set_up_and_carries_an_html_view() {
    let r = rig().await;
    let v = send(&r.kernel, json!({"type": "reflect"})).await.result;
    assert_eq!(v["account"], json!(fake::USER));
    assert_eq!(v["login"], json!("password"));
    assert_eq!(v["mailboxes"], json!(["INBOX", "Archive"]));
    assert_eq!(v["send_allow"], json!(["friend@example.org", "corp.test"]));
    assert_eq!(v["view"]["kind"], json!("html"));
    assert!(v["view"]["bytes"].as_str().unwrap().contains("var AGENT = \"mail\""));
    // the view reads, and watches what the agent publishes
    assert_eq!(v["view"]["calls"]["mail"], json!(["reflect", "list", "inquiries", "watch"]));
}

#[tokio::test]
async fn only_a_person_approves_a_send() {
    use fantastic_kernel::test_support::as_agent;
    // each caller holds its own line on `mail`; the mail agent keeps no list
    let r = rig_with(fake::PASS, |_| {}).await;
    let root = r.kernel.root().unwrap().id.clone();
    for name in ["asker", "other"] {
        fantastic_kernel::test_support::register(&r.kernel, Agent::new(
            AgentId::from(name),
            None,
            Some(root.clone()),
            json!({"can": {"mail": {"call": ["*"]}}}).as_object().cloned().unwrap(),
            PathBuf::new(),
            false,
        ));
    }
    let payload = json!({"type": "send", "to": ["friend@example.org"], "subject": "Hi", "text": "x"});
    let asker_send = as_agent(AgentId::from("asker"), send(&r.kernel, payload));
    let answers = async {
        let mut re = Value::Null;
        for _ in 0..500 {
            let q = send(&r.kernel, json!({"type": "inquiries"})).await.result;
            if let Some(first) = q["inquiries"].get(0) {
                re = first["re"].clone();
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!re.is_null(), "no inquiry was raised");
        // the asker and another agent answering yes: refused, nothing sent, still open
        for who in ["asker", "other"] {
            let o = as_agent(AgentId::from(who), send(&r.kernel, json!({"type": "answer_inquiry", "re": re, "answer": true}))).await;
            assert_eq!(reason(&o), "not_allowed", "{who}: {}", o.result);
        }
        assert!(r.smtp.lock().unwrap().mails.is_empty());
        let q = send(&r.kernel, json!({"type": "inquiries"})).await.result;
        assert_eq!(q["inquiries"].as_array().unwrap().len(), 1, "the question stays open");
        // a person's no ends it
        send(&r.kernel, json!({"type": "answer_inquiry", "re": re, "answer": false})).await;
    };
    let (out, ()) = tokio::join!(asker_send, answers);
    assert_eq!(out.result, json!({"sent": false}), "{}", out.result);
    assert!(r.smtp.lock().unwrap().mails.is_empty());
}

#[tokio::test]
async fn fetch_returns_one_attachment_when_asked_and_refuses_one_over_the_cap() {
    use base64::Engine;
    let r = rig().await;
    let o = send(&r.kernel, json!({"type": "fetch", "mailbox": "INBOX", "uid": 3, "attachment": "plan.pdf"})).await.result;
    assert_eq!(o["attachment"]["name"], json!("plan.pdf"), "{o}");
    let bytes = base64::engine::general_purpose::STANDARD.decode(o["attachment"]["base64"].as_str().unwrap()).unwrap();
    assert_eq!(bytes, b"ABCDEFGH");
    assert_eq!(o["attachment"]["size"], json!(8));
    // without the argument no bytes come
    let o = send(&r.kernel, json!({"type": "fetch", "mailbox": "INBOX", "uid": 3})).await.result;
    assert!(o.get("attachment").is_none(), "{o}");
    let o = send(&r.kernel, json!({"type": "fetch", "mailbox": "INBOX", "uid": 3, "attachment": "nope.pdf"})).await;
    assert_eq!(reason(&o), "not_found", "{}", o.result);
    // over the cap (1 MiB)
    let big = base64::engine::general_purpose::STANDARD.encode(vec![b'z'; 1024 * 1024 + 1]);
    let raw = format!(
        "From: a@example.org\r\nTo: me@corp.test\r\nSubject: Big\r\nMessage-ID: <big@x>\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=XX\r\n\r\n--XX\r\nContent-Type: text/plain\r\n\r\nbig\r\n--XX\r\nContent-Type: application/octet-stream; name=\"big.bin\"\r\nContent-Disposition: attachment; filename=\"big.bin\"\r\nContent-Transfer-Encoding: base64\r\n\r\n{big}\r\n--XX--\r\n"
    );
    r.imap.lock().unwrap().boxes.get_mut("INBOX").unwrap().push(msg(4, &[], raw.into_bytes()));
    let o = send(&r.kernel, json!({"type": "fetch", "mailbox": "INBOX", "uid": 4, "attachment": "big.bin"})).await;
    assert_eq!(reason(&o), "limit", "{}", o.result);
    assert!(!o.result.to_string().contains("eno="), "{}", o.result);
    // the listing of names and sizes still works for it
    let o = send(&r.kernel, json!({"type": "fetch", "mailbox": "INBOX", "uid": 4})).await.result;
    assert_eq!(o["attachments"][0]["size"], json!(1024 * 1024 + 1), "{o}");
}

#[tokio::test]
async fn tls_false_is_for_a_loopback_host_only() {
    for field in ["imap", "smtp"] {
        let r = rig_with(fake::PASS, |m| {
            m.insert(field.into(), json!({"host": "mail.example.com", "port": 143, "tls": false}));
        })
        .await;
        let hit = if field == "imap" {
            send(&r.kernel, json!({"type": "list", "mailbox": "INBOX"})).await
        } else {
            send(&r.kernel, json!({"type": "send", "to": ["friend@example.org"], "subject": "s", "text": "t"})).await
        };
        let hit = &hit;
        assert_eq!(reason(hit), "bad_arg", "{field}: {}", hit.result);
        assert_eq!(hit.result["arg"], json!(field));
        no_secret(&hit.result);
    }
    // loopback spellings pass the check (the connection itself is not attempted for ::1 here)
    for h in ["127.0.0.1", "localhost", "LOCALHOST", "::1", "[::1]"] {
        let m = json!({"imap": {"host": h, "port": 143, "tls": false}}).as_object().cloned().unwrap();
        assert!(super::endpoint(&m, "imap", &AgentId::from("mail")).is_ok(), "{h}");
    }
}

/// WHAT CHANGED, PUBLISHED: a mark is `marked {mailbox, uid}` and a sent mail is
/// `sent {message_id, to}` on the agent's own inbox, so a view watching it re-reads on
/// those only; a refused mark and a send answered no publish nothing.
#[tokio::test]
async fn a_mark_and_a_sent_mail_are_published_on_the_agents_inbox() {
    let r = rig().await;
    let mut rx = r.kernel.ensure_inbox(&AgentId::from("mail_view"));
    assert!(fantastic_kernel::send::as_operator(r.kernel.watch(&AgentId::from("mail"), AgentId::from("mail_view"))).await.is_none());
    send(&r.kernel, json!({"type": "mark", "mailbox": "INBOX", "uid": 2, "seen": true})).await;
    send(&r.kernel, json!({"type": "mark", "mailbox": "INBOX", "uid": 2})).await;
    let no = json!({"type": "send", "to": ["friend@example.org"], "subject": "No", "text": "x"});
    let _ = tokio::join!(send(&r.kernel, no), approve(&r, json!(false)));
    let yes = json!({"type": "send", "to": ["friend@example.org"], "subject": "Yes", "text": "x"});
    let (out, _) = tokio::join!(send(&r.kernel, yes), approve(&r, json!(true)));
    let mut got = Vec::new();
    while let Ok(Some(it)) = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await {
        if matches!(it.payload["type"].as_str(), Some("marked" | "sent")) {
            got.push(it.payload.clone());
        }
    }
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(got[0], json!({"type": "marked", "mailbox": "INBOX", "uid": 2}));
    assert_eq!(got[1]["type"], json!("sent"));
    assert_eq!(got[1]["message_id"], out.result["message_id"]);
    assert_eq!(got[1]["to"], json!(["friend@example.org"]));
}

/// THE APPROVAL IS A MESSAGE (#795): a `send` defers its settle and its handler
/// returns; nothing waits on the person. A `send` whose call ended first (its
/// deadline) sends nothing when the person answers yes later.
#[tokio::test]
async fn a_send_whose_call_ended_sends_nothing_on_a_later_yes() {
    let r = rig().await;
    let payload = json!({"type": "send", "to": ["friend@example.org"], "subject": "Hi", "text": "x"});
    let out = r
        .kernel
        .operator_send(vec![Item::new(AgentId::from("mail"), payload).with_timeout(0.3)])
        .await
        .remove(0);
    assert_eq!(out.status, Status::TimedOut, "{}", out.result);
    approve(&r, json!(true)).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(r.smtp.lock().unwrap().mails.is_empty(), "a cancelled send was sent");
}
