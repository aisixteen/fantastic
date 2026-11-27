//! THE AI AGENT'S SAVED SET-UPS, through the one agent that holds them.
//!
//! `home` is the user's home store under `user` (the blueprint's record). The
//! binary fixes where its files live (`FANTASTIC_HOME`, else `~/.fantastic`;
//! `kern/host` registry). The shell calls it as the operator.
//!
//! A saved set-up is `handler_module` + that bundle's config: for an AI agent the
//! backend, model and endpoint. It never carries an id, a memory store or
//! lines — whoever creates an agent from it adds those. Nothing is a default
//! until the human makes one (`prefer`).
//!
//! An AI set-up is born as TWO records (a provider and its chat, `make_ai`), so it is
//! SAVED as two: `<name>` holds the provider's share (judged by the backend) and
//! `<name>__chat` the chat's share (its settings and request knobs, judged by the chat),
//! split as the pair is (`kern::backend::split`). Each record is checked by its own
//! bundle, as every saved record is. [`shelf`] shows them as one set-up again.

use serde_json::{json, Map, Value};

use crate::kern::cps::Done;
use crate::kern::host::Host;

/// The home store's agent id (the blueprint's).
pub const HOME_ID: &str = "home";

/// The suffix of the record that holds an AI set-up's chat share.
pub const CHAT_SUFFIX: &str = "__chat";

fn chat_name(name: &str) -> String {
    format!("{name}{CHAT_SUFFIX}")
}

/// One saved set-up.
#[derive(Debug, Clone, PartialEq)]
pub struct Saved {
    pub name: String,
    pub version: String,
    pub record: Map<String, Value>,
}

impl Saved {
    fn field(&self, k: &str) -> String {
        self.record.get(k).and_then(Value::as_str).unwrap_or("").to_string()
    }
    pub fn handler(&self) -> String {
        self.field("handler_module")
    }
    pub fn model(&self) -> String {
        self.field("model")
    }
    pub fn endpoint(&self) -> String {
        self.field("endpoint")
    }
}

/// Everything the store holds, and where.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Shelf {
    pub saved: Vec<Saved>,
    pub preferred: Option<String>,
    pub dir: String,
}

fn err_of(v: &Value) -> Option<String> {
    v.get("error").and_then(Value::as_str).map(|e| match v.get("hint").and_then(Value::as_str) {
        Some(h) => format!("{e} — {h}"),
        None => e.to_string(),
    })
}

/// What is saved: the home's `load_tree`, a call (#795) — `done` gets the shelf with its settle.
pub async fn shelf(host: &Host, done: Done<Result<Shelf, String>>) {
    host.operator_then(HOME_ID, json!({"type": "load_tree"}), move |out| async move {
        done(shelf_of(&out.result)).await;
    })
    .await;
}

/// The shelf a `load_tree` reply holds.
fn shelf_of(r: &Value) -> Result<Shelf, String> {
    if let Some(e) = err_of(r) {
        return Err(e);
    }
    let saved = r["saved"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|t| {
            Some(Saved {
                name: t.get("name")?.as_str()?.to_string(),
                version: t.get("version").and_then(Value::as_str).unwrap_or("").to_string(),
                record: t.get("record")?.as_object()?.clone(),
            })
        })
        .collect::<Vec<Saved>>();
    // an AI set-up's chat share is shown on its set-up, never as one of its own
    let (chats, mut saved): (Vec<Saved>, Vec<Saved>) = saved.into_iter().partition(|s| s.name.ends_with(CHAT_SUFFIX));
    for s in &mut saved {
        if let Some(c) = chats.iter().find(|c| c.name == chat_name(&s.name)) {
            for (k, v) in &c.record {
                if k != "handler_module" {
                    s.record.insert(k.clone(), v.clone());
                }
            }
        }
    }
    Ok(Shelf {
        saved,
        preferred: r["preferred"].as_str().map(str::to_string),
        dir: r["dir"].as_str().unwrap_or("").to_string(),
    })
}

/// Save `record` as saved set-up `name`; `done` gets where it was written. An AI set-up (a
/// backend's) is written as its two records, the second when the first settled; a set-up
/// with no chat share leaves none.
pub async fn save(host: &Host, name: &str, record: Value, done: Done<Result<String, String>>) {
    if name.ends_with(CHAT_SUFFIX) {
        return done(Err(format!("a set-up name may not end in {CHAT_SUFFIX:?}"))).await;
    }
    let Value::Object(mut record) = record else {
        return persist(host, name, record, done).await;
    };
    let handler = record.get("handler_module").and_then(Value::as_str).unwrap_or("").to_string();
    let is_ai = host.kernel.bundles.catalog().any(|(m, b)| m == handler && crate::kern::setup::is_backend(b));
    if !is_ai {
        return persist(host, name, Value::Object(record), done).await;
    }
    record.remove("handler_module");
    let (mut provider, mut chat) = crate::kern::backend::split(&record);
    let has_chat = chat.keys().any(|k| !provider.contains_key(k));
    provider.insert("handler_module".into(), json!(handler));
    let (h, chat_name) = (host.clone(), chat_name(name));
    persist(
        host,
        name,
        Value::Object(provider),
        crate::kern::cps::done(move |r: Result<String, String>| async move {
            let path = match r {
                Ok(p) => p,
                Err(e) => return done(Err(e)).await,
            };
            if has_chat {
                chat.insert("handler_module".into(), json!(fantastic_ai_core::chat_bundle::HANDLER_MODULE));
                persist(&h, &chat_name, Value::Object(chat), crate::kern::cps::done(move |r: Result<String, String>| async move {
                    done(r.map(|_| path)).await;
                }))
                .await;
            } else {
                forget_one(&h, &chat_name, true, crate::kern::cps::done(move |r: Result<(), String>| async move {
                    done(r.map(|()| path)).await;
                }))
                .await;
            }
        }),
    )
    .await;
}

async fn persist(host: &Host, name: &str, record: Value, done: Done<Result<String, String>>) {
    host.operator_then(HOME_ID, json!({"type": "persist_record", "name": name, "record": record}), move |out| async move {
        let r = out.result;
        done(match err_of(&r) {
            Some(e) => Err(e),
            None => Ok(r["path"].as_str().unwrap_or("").to_string()),
        })
        .await;
    })
    .await;
}

/// Forget saved set-up `name`, its chat share with it (the chat's forgotten when the set-up's was).
pub async fn forget(host: &Host, name: &str, done: Done<Result<(), String>>) {
    let (h, chat) = (host.clone(), chat_name(name));
    forget_one(
        host,
        name,
        false,
        crate::kern::cps::done(move |r: Result<(), String>| async move {
            match r {
                Ok(()) => forget_one(&h, &chat, true, done).await,
                Err(e) => done(Err(e)).await,
            }
        }),
    )
    .await;
}

async fn forget_one(host: &Host, name: &str, if_any: bool, done: Done<Result<(), String>>) {
    host.operator_then(HOME_ID, json!({"type": "forget_record", "name": name}), move |out| async move {
        let r = out.result;
        done(match err_of(&r) {
            Some(_) if if_any && r.get("reason").and_then(Value::as_str) == Some(fantastic_kernel::refusal::NOT_FOUND) => Ok(()),
            Some(e) => Err(e),
            None => Ok(()),
        })
        .await;
    })
    .await;
}

/// Make saved set-up `name` the default for new AI agents; `None` clears it.
pub async fn prefer(host: &Host, name: Option<&str>, done: Done<Result<Option<String>, String>>) {
    host.operator_then(HOME_ID, json!({"type": "prefer", "name": name}), move |out| async move {
        let r = out.result;
        done(match err_of(&r) {
            Some(e) => Err(e),
            None => Ok(r["preferred"].as_str().map(str::to_string)),
        })
        .await;
    })
    .await;
}
