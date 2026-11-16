//! In-process fake IMAP and SMTP servers: just enough of each protocol for the agent's
//! calls, recording what they were asked. Plain TCP on a local port.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use base64::Engine;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

pub const USER: &str = "me@corp.test";
pub const PASS: &str = "app-pass-8472";

#[derive(Clone)]
pub struct Msg {
    pub uid: u32,
    pub flags: Vec<String>,
    pub raw: Vec<u8>,
}

#[derive(Default)]
pub struct ImapState {
    pub boxes: HashMap<String, Vec<Msg>>,
    /// Every command line received (the login's secret never appears here).
    pub commands: Vec<String>,
}

pub fn message(from: &str, subject: &str, id: &str, body: &str) -> Vec<u8> {
    format!(
        "From: {from}\r\nTo: me@corp.test\r\nSubject: {subject}\r\nDate: Mon, 16 Nov 2026 10:00:00 +0000\r\nMessage-ID: <{id}>\r\nMIME-Version: 1.0\r\nContent-Type: text/plain\r\n\r\n{body}\r\n"
    )
    .into_bytes()
}

/// A message with a text part and one attachment.
pub fn with_attachment(from: &str, subject: &str, id: &str) -> Vec<u8> {
    format!(
        "From: {from}\r\nTo: me@corp.test\r\nSubject: {subject}\r\nDate: Mon, 16 Nov 2026 10:00:00 +0000\r\nMessage-ID: <{id}>\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=XX\r\n\r\n--XX\r\nContent-Type: text/plain\r\n\r\nsee attached\r\n--XX\r\nContent-Type: application/pdf; name=\"plan.pdf\"\r\nContent-Disposition: attachment; filename=\"plan.pdf\"\r\nContent-Transfer-Encoding: base64\r\n\r\nQUJDREVGR0g=\r\n--XX--\r\n"
    )
    .into_bytes()
}

fn quoted(arg: &str) -> String {
    arg.trim().trim_matches('"').to_string()
}

pub async fn imap(state: Arc<Mutex<ImapState>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else { return };
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                let (r, mut w) = sock.into_split();
                let mut lines = BufReader::new(r).lines();
                w.write_all(b"* OK fake imap ready\r\n").await.unwrap();
                let mut selected: Option<String> = None;
                while let Ok(Some(line)) = lines.next_line().await {
                    let (tag, rest) = line.split_once(' ').unwrap_or((&line, ""));
                    let (cmd, args) = rest.split_once(' ').unwrap_or((rest, ""));
                    let cmd = cmd.to_ascii_uppercase();
                    if cmd != "LOGIN" {
                        state.lock().unwrap().commands.push(rest.to_string());
                    }
                    let mut out: Vec<u8> = Vec::new();
                    match cmd.as_str() {
                        "LOGIN" => {
                            let mut it = args.splitn(2, ' ');
                            let (u, p) = (quoted(it.next().unwrap_or("")), quoted(it.next().unwrap_or("")));
                            if u == USER && p == PASS {
                                out.extend(format!("{tag} OK LOGIN completed\r\n").bytes());
                            } else {
                                // the server repeats what it was given: the agent must not
                                out.extend(format!("{tag} NO [AUTHENTICATIONFAILED] invalid login {u}/{p}\r\n").bytes());
                            }
                        }
                        "SELECT" | "EXAMINE" => {
                            let name = quoted(args);
                            let st = state.lock().unwrap();
                            if let Some(b) = st.boxes.get(&name) {
                                selected = Some(name);
                                out.extend(
                                    format!(
                                        "* {} EXISTS\r\n* 0 RECENT\r\n* OK [UIDVALIDITY 1] ok\r\n* FLAGS (\\Seen \\Flagged)\r\n{tag} OK [READ-WRITE] done\r\n",
                                        b.len()
                                    )
                                    .bytes(),
                                );
                            } else {
                                out.extend(format!("{tag} NO no such mailbox\r\n").bytes());
                            }
                        }
                        "UID" => {
                            let Some(name) = selected.clone() else {
                                out.extend(format!("{tag} BAD no mailbox selected\r\n").bytes());
                                w.write_all(&out).await.unwrap();
                                continue;
                            };
                            let (sub, a) = args.split_once(' ').unwrap_or((args, ""));
                            let mut st = state.lock().unwrap();
                            let msgs = st.boxes.get_mut(&name).unwrap();
                            match sub.to_ascii_uppercase().as_str() {
                                "SEARCH" => {
                                    let unseen = a.to_ascii_uppercase().contains("UNSEEN");
                                    let ids: Vec<String> = msgs
                                        .iter()
                                        .filter(|m| !unseen || !m.flags.iter().any(|f| f == "\\Seen"))
                                        .map(|m| m.uid.to_string())
                                        .collect();
                                    out.extend(format!("* SEARCH {}\r\n{tag} OK done\r\n", ids.join(" ")).bytes());
                                }
                                "FETCH" => {
                                    let (set, items) = a.split_once(' ').unwrap_or((a, ""));
                                    let want: Vec<u32> = set.split(',').filter_map(|s| s.parse().ok()).collect();
                                    for (i, m) in msgs.iter().enumerate().filter(|(_, m)| want.contains(&m.uid)) {
                                        let mut parts = vec![format!("UID {}", m.uid)];
                                        if items.contains("FLAGS") {
                                            parts.push(format!("FLAGS ({})", m.flags.join(" ")));
                                        }
                                        if items.contains("RFC822.SIZE") {
                                            parts.push(format!("RFC822.SIZE {}", m.raw.len()));
                                        }
                                        let mut lit: Option<(String, Vec<u8>)> = None;
                                        if items.contains("BODY.PEEK[HEADER]") {
                                            let end = m.raw.windows(4).position(|w| w == b"\r\n\r\n").map_or(m.raw.len(), |p| p + 4);
                                            lit = Some(("BODY[HEADER]".into(), m.raw[..end].to_vec()));
                                        } else if items.contains("BODY.PEEK[]") {
                                            lit = Some(("BODY[]".into(), m.raw.clone()));
                                        }
                                        out.extend(format!("* {} FETCH ({}", i + 1, parts.join(" ")).bytes());
                                        if let Some((n, bytes)) = lit {
                                            out.extend(format!(" {n} {{{}}}\r\n", bytes.len()).bytes());
                                            out.extend(bytes);
                                        }
                                        out.extend(b")\r\n");
                                    }
                                    out.extend(format!("{tag} OK done\r\n").bytes());
                                }
                                "STORE" => {
                                    let (set, spec) = a.split_once(' ').unwrap_or((a, ""));
                                    let uid: u32 = set.parse().unwrap_or(0);
                                    let add = spec.starts_with('+');
                                    let flag = spec.split(['(', ')']).nth(1).unwrap_or("").to_string();
                                    if let Some(m) = msgs.iter_mut().find(|m| m.uid == uid) {
                                        m.flags.retain(|f| *f != flag);
                                        if add {
                                            m.flags.push(flag);
                                        }
                                    }
                                    out.extend(format!("{tag} OK done\r\n").bytes());
                                }
                                _ => out.extend(format!("{tag} BAD unknown\r\n").bytes()),
                            }
                        }
                        "LOGOUT" => {
                            out.extend(format!("* BYE bye\r\n{tag} OK LOGOUT completed\r\n").bytes());
                            w.write_all(&out).await.unwrap();
                            return;
                        }
                        _ => out.extend(format!("{tag} BAD unknown\r\n").bytes()),
                    }
                    w.write_all(&out).await.unwrap();
                }
            });
        }
    });
    port
}

#[derive(Clone, Debug)]
pub struct Mail {
    pub from: String,
    pub rcpt: Vec<String>,
    pub data: String,
}

#[derive(Default)]
pub struct SmtpState {
    pub mails: Vec<Mail>,
    pub auth_failures: usize,
}

pub async fn smtp(state: Arc<Mutex<SmtpState>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else { return };
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                let (r, mut w) = sock.into_split();
                let mut r = BufReader::new(r);
                w.write_all(b"220 fake smtp\r\n").await.unwrap();
                let mut cur = Mail { from: String::new(), rcpt: vec![], data: String::new() };
                let b64 = base64::engine::general_purpose::STANDARD;
                let mut line = String::new();
                loop {
                    line.clear();
                    if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let l = line.trim_end().to_string();
                    let up = l.to_ascii_uppercase();
                    let reply: String = if up.starts_with("EHLO") || up.starts_with("HELO") {
                        "250-fake\r\n250 AUTH PLAIN LOGIN\r\n".into()
                    } else if up.starts_with("AUTH PLAIN") {
                        let enc = l.split_whitespace().nth(2).unwrap_or("");
                        let dec = b64.decode(enc).unwrap_or_default();
                        let mut p = dec.split(|b| *b == 0).skip(1);
                        let (u, pw) = (p.next().unwrap_or_default().to_vec(), p.next().unwrap_or_default().to_vec());
                        if u == USER.as_bytes() && pw == PASS.as_bytes() {
                            "235 ok\r\n".into()
                        } else {
                            state.lock().unwrap().auth_failures += 1;
                            format!("535 5.7.8 bad credentials {}\r\n", String::from_utf8_lossy(&pw))
                        }
                    } else if up.starts_with("MAIL FROM:") {
                        cur = Mail { from: l[10..].trim().trim_matches(['<', '>']).to_string(), rcpt: vec![], data: String::new() };
                        "250 ok\r\n".into()
                    } else if up.starts_with("RCPT TO:") {
                        cur.rcpt.push(l[8..].trim().trim_matches(['<', '>']).to_string());
                        "250 ok\r\n".into()
                    } else if up == "DATA" {
                        w.write_all(b"354 go\r\n").await.unwrap();
                        let mut data = String::new();
                        loop {
                            line.clear();
                            if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                                return;
                            }
                            if line == ".\r\n" {
                                break;
                            }
                            data.push_str(&line);
                        }
                        cur.data = data;
                        state.lock().unwrap().mails.push(cur.clone());
                        "250 queued\r\n".into()
                    } else if up == "QUIT" {
                        w.write_all(b"221 bye\r\n").await.unwrap();
                        return;
                    } else {
                        "250 ok\r\n".into()
                    };
                    w.write_all(reply.as_bytes()).await.unwrap();
                }
            });
        }
    });

    port
}
