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
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));

    // the login: a ref bound to the local host, holding username and password
    let store = creds::ensure_store(&kernel, tmp.path(), "mail").await;
    let declared = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "update_agent", "id": store.0, "refs": {"main": {"host": "127.0.0.1"}}}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(declared["updated"], json!(true), "{declared}");
    let set = kernel
        .send(vec![Item::new(store, json!({"type": "set", "ref": "main", "fields": {"username": fake::USER, "password": password}}))])
        .await
        .remove(0)
        .result;
    assert_eq!(set["stored"], json!(true), "{set}");

    let mut meta = Map::new();
    meta.insert("imap".into(), json!({"host": "127.0.0.1", "port": iport, "tls": false}));
    meta.insert("smtp".into(), json!({"host": "127.0.0.1", "port": sport, "tls": false}));
    meta.insert("mailboxes".into(), json!(["INBOX", "Archive"]));
    meta.insert("send_allow".into(), json!(["friend@example.org", "corp.test"]));
    creds::add_credentials(&mut meta, "main");
    tweak(&mut meta);
    kernel.register(Agent::new(
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
    k.send(vec![Item::new(AgentId::from("mail"), payload)]).await.remove(0)
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
    let st = r.smtp.lock().unwrap();
    assert_eq!(st.mails.len(), 1);
    let m = &st.mails[0];
    assert_eq!(m.from, fake::USER);
    assert_eq!(m.rcpt, vec!["friend@example.org", "Pat@Corp.Test"]);
    assert!(m.data.contains("Subject: Re: Lunch?") && m.data.contains("Noon works."), "{}", m.data);
    assert!(m.data.contains(&format!("Message-ID: <{id}>")), "{}", m.data);
    assert!(m.data.contains("In-Reply-To: <m2@x>"), "{}", m.data);
    drop(st);
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
    assert_eq!(out.result["sent"], json!(false));
    assert!(r.smtp.lock().unwrap().mails.is_empty());
    let refl = send(&r.kernel, json!({"type": "reflect"})).await.result;
    assert_eq!(refl["recent_sends"], json!([]));
    // words other than yes are no, too
    let payload = json!({"type": "send", "to": ["friend@example.org"], "subject": "Hi", "text": "x"});
    let (out, _) = tokio::join!(send(&r.kernel, payload), approve(&r, json!("maybe later")));
    assert_eq!(out.result["sent"], json!(false));
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
    // the view reads only
    assert_eq!(v["view"]["calls"]["mail"], json!(["reflect", "list", "inquiries"]));
}
