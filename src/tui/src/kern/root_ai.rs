//! THE ROOT AI: `fantastic [canvas] ai BACKEND …` (`recipe.rs`). ONE chat agent, `ai`
//! under `user`, on the project's memory store: the terminal UI opens it; on a
//! canvas, a chat cell shows it wherever one is placed (no main chat).
//!
//! - `ai` is a chat on a PROVIDER (`ai_provider`, the backend). On the SAME backend the provider
//!   is RE-POINTED (its endpoint, its own fields) and
//!   what the server decides is CLEARED, so the backend reads it from the server
//!   before its first pass (and writes it back);
//! - on a DIFFERENT backend it is remade: deleted and created again under the
//!   same id on the same store (the dialog's rows are keyed by the id, so the
//!   conversation carries over; its own grants are written again);
//! - none yet: made.
//!
//! Nothing here is guessed: the port and model are the caller's; the host is `127.0.0.1`, the Responses endpoint OpenAI's and the
//! Anthropic one its Messages API, unless named.

use serde_json::{json, Map, Value};

use crate::kern::cps::Done;
use crate::kern::host::Host;

/// The root AI's id.
pub const ROOT_AI: &str = "ai";
/// `ai responses` without `--endpoint`.
pub const RESPONSES_ENDPOINT: &str = "https://api.openai.com/v1/responses";
/// `ai anthropic` without `--endpoint` (the Messages API).
pub const ANTHROPIC_ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
const VIEW: &str = "view_root";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    Vllm { host: String, port: u16, kv_overflow: String },
    Ollama { host: String, port: u16 },
    Responses { endpoint: String, model: String },
    OpenAi { endpoint: String, model: String },
    Anthropic { endpoint: String, model: String },
}

impl Backend {
    pub fn handler(&self) -> &'static str {
        match self {
            Backend::Vllm { .. } => "vllm_backend.tools",
            Backend::Ollama { .. } => "ollama_backend.tools",
            Backend::Responses { .. } => "responses_backend.tools",
            Backend::OpenAi { .. } => "openai_backend.tools",
            Backend::Anthropic { .. } => "anthropic_backend.tools",
        }
    }

    /// The cargo feature that links it.
    fn feature(&self) -> &'static str {
        match self {
            Backend::Vllm { .. } => "vllm",
            Backend::Ollama { .. } => "ollama",
            Backend::Responses { .. } => "responses",
            Backend::OpenAi { .. } => "openai",
            Backend::Anthropic { .. } => "anthropic",
        }
    }

    pub fn endpoint(&self) -> String {
        match self {
            Backend::Vllm { host, port, .. } => format!("http://{host}:{port}/v1"),
            Backend::Ollama { host, port } => format!("http://{host}:{port}"),
            Backend::Responses { endpoint, .. } | Backend::OpenAi { endpoint, .. } | Backend::Anthropic { endpoint, .. } => {
                endpoint.clone()
            }
        }
    }

    /// The record fields the caller names.
    fn fields(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("endpoint".into(), json!(self.endpoint()));
        match self {
            Backend::Vllm { kv_overflow, .. } => {
                m.insert("kv_overflow".into(), json!(kv_overflow));
            }
            Backend::Ollama { .. } => {}
            Backend::Responses { model, .. } | Backend::OpenAi { model, .. } | Backend::Anthropic { model, .. } => {
                m.insert("model".into(), json!(model));
            }
        }
        m
    }

    /// What the server decides — cleared on a re-point, so the new server is read.
    fn server_fields(&self) -> &'static [&'static str] {
        match self {
            Backend::Vllm { .. } => &["model", "context_window", "kv_slots", "metrics_url"],
            // only the model: ollama's loaded window is not in its API
            Backend::Ollama { .. } => &["model"],
            Backend::Responses { .. } | Backend::OpenAi { .. } => &[],
            Backend::Anthropic { .. } => &["context_window"],
        }
    }
}

/// The chat cell's own grant onto the AI it shows — `ai_view.ts`'s `grants`.
fn err_of(r: &Value) -> Option<String> {
    r.get("error").and_then(Value::as_str).map(str::to_string)
}

fn handler_of(h: &Host, id: &str) -> Option<String> {
    h.kernel.resolve_target(id)?.record().handler_module
}

/// The agent `id`'s record, as a `create_agent` to put it back.
fn record_of(h: &Host, id: &str) -> Option<Map<String, Value>> {
    let rec = h.kernel.resolve_target(id).and_then(|a| serde_json::to_value(a.record()).ok())?;
    let mut rec = rec.as_object().cloned()?;
    rec.insert("type".into(), json!("create_agent"));
    Some(rec)
}

/// Make, re-point or remake the root AI on `b`; `done` gets what it did. The root AI is a chat (`ai`)
/// and its provider (the chat's `provider_id`, a backend): a re-point changes the provider's
/// record, a remake deletes and makes both. Every call is sent when the one before settled (#795).
pub async fn run(h: &Host, b: &Backend, done: Done<Result<String, String>>) {
    // a build without the backend would skip the record at the next boot
    if h.kernel.bundles.get(b.handler()).is_none() {
        return done(Err(format!("this build has no {} backend — build with `--features {}`", b.handler(), b.feature()))).await;
    }
    let had_chat = handler_of(h, ROOT_AI);
    let provider = crate::kern::backend::provider_of(h, ROOT_AI);
    let had = provider.as_deref().and_then(|p| handler_of(h, p));
    let (handler, endpoint) = (b.handler(), b.endpoint());
    let said = move |did: &str| format!("the root AI `{ROOT_AI}` ({did}, {handler}) speaks to {endpoint}");
    if let (Some(p), true) = (&provider, had.as_deref() == Some(b.handler())) {
        let mut up = b.fields();
        for f in b.server_fields() {
            up.entry(f.to_string()).or_insert(Value::Null);
        }
        // the model and the window are on the chat too: cleared with the provider's, the new
        // server is read
        let chat_up = crate::kern::backend::chat_part(&up);
        up.insert("type".into(), json!("update_agent"));
        up.insert("id".into(), json!(p));
        let (host, p) = (h.clone(), p.clone());
        h.operator_then("kernel", Value::Object(up), move |out| async move {
            if let Some(e) = err_of(&out.result) {
                return done(Err(format!("re-pointing `{p}`: {e}"))).await;
            }
            if chat_up.is_empty() {
                return done(Ok(said("re-pointed"))).await;
            }
            let mut up = chat_up;
            up.insert("type".into(), json!("update_agent"));
            up.insert("id".into(), json!(ROOT_AI));
            host.operator_then("kernel", Value::Object(up), move |out| async move {
                done(match err_of(&out.result) {
                    Some(e) => Err(format!("re-pointing `{ROOT_AI}`: {e}")),
                    None => Ok(said("re-pointed")),
                })
                .await;
            })
            .await;
        })
        .await;
        return;
    }
    // the OLD records (chat and provider), to put back if the new ones are refused (a missing
    // key, a field the new backend requires): a failed switch never loses the AI
    let old_provider = provider.as_deref().and_then(|p| record_of(h, p));
    let old_chat = record_of(h, ROOT_AI);
    // the deletes of the old pair, the chat first, then the provider
    let mut deletes: Vec<(Value, String)> = Vec::new();
    if had_chat.is_some() {
        deletes.push((json!({"type": "delete_agent", "id": ROOT_AI}), format!("removing the old `{ROOT_AI}`")));
    }
    if let (Some(p), Some(_)) = (&provider, &old_provider) {
        deletes.push((json!({"type": "delete_agent", "id": p}), format!("removing the old `{p}`")));
    }
    let remake = Remake { h: h.clone(), b: b.clone(), old_provider, old_chat, had, had_chat: had_chat.clone() };
    let made = if had_chat.is_some() { "remade" } else { "made" };
    remake.delete_then_make(deletes.into_iter().collect(), Box::new(move || said(made)), done).await;
}

/// A switch of the root AI to another backend, in flight.
struct Remake {
    h: Host,
    b: Backend,
    old_provider: Option<Map<String, Value>>,
    old_chat: Option<Map<String, Value>>,
    had: Option<String>,
    had_chat: Option<String>,
}

impl Remake {
    /// The old pair's deletes, each when the one before settled; then the new pair.
    fn delete_then_make(
        self,
        mut deletes: std::collections::VecDeque<(Value, String)>,
        said: Box<dyn FnOnce() -> String + Send>,
        done: Done<Result<String, String>>,
    ) -> futures_util::future::BoxFuture<'static, ()> {
        Box::pin(async move {
            let Some((call, what)) = deletes.pop_front() else {
                return self.make(said, done).await;
            };
            let h = self.h.clone();
            h.operator_then("kernel", call, move |out| async move {
                if let Some(e) = err_of(&out.result) {
                    return done(Err(format!("{what}: {e}"))).await;
                }
                self.delete_then_make(deletes, said, done).await;
            })
            .await;
        })
    }

    /// The new pair; a refusal removes what the failed make left and puts the old pair back.
    async fn make(self, said: Box<dyn FnOnce() -> String + Send>, done: Done<Result<String, String>>) {
        let mut spec = self.b.fields();
        spec.insert("handler_module".into(), json!(self.b.handler()));
        let user = fantastic_kernel::AgentId::from(self.h.cfg.user.as_str());
        let h = self.h.clone();
        crate::kern::backend::BackendBundle::make_ai(
            &h,
            &crate::kern::backend::By::Operator,
            &user,
            ROOT_AI,
            spec,
            Map::new(),
            crate::kern::cps::done(move |r: Result<(), String>| async move {
                match r {
                    Ok(()) => done(Ok(said())).await,
                    Err(e) => self.restore(e, done).await,
                }
            }),
        )
        .await;
    }

    /// Whatever the failed make left is removed, then the old pair is put back (provider first).
    async fn restore(self, e: String, done: Done<Result<String, String>>) {
        let mut calls: Vec<Value> = Vec::new();
        for id in [ROOT_AI.to_string(), crate::kern::backend::provider_id(ROOT_AI)] {
            if self.h.kernel.exists(&id) {
                calls.push(json!({"type": "delete_agent", "id": id}));
            }
        }
        let cleaned = calls.len();
        let Some(chat) = self.old_chat else {
            // no old AI to put back: the cleanup, then the refusal
            return crate::kern::cps::chain(&self.h, calls, crate::kern::cps::done(move |_| async move { done(Err(e)).await })).await;
        };
        for rec in self.old_provider.into_iter().chain([chat]) {
            calls.push(Value::Object(rec));
        }
        let left = self.had.unwrap_or_else(|| self.had_chat.unwrap_or_default());
        crate::kern::cps::chain(
            &self.h,
            calls,
            crate::kern::cps::done(move |replies: Vec<Value>| async move {
                // the cleanup's own answers are let go; the put-back's first failure is said
                let failed = replies.iter().skip(cleaned).find_map(err_of);
                done(Err(match failed {
                    None => format!("{e} — `{ROOT_AI}` is left on {left}"),
                    Some(b2) => format!("{e} — and the old `{ROOT_AI}` could not be restored: {b2}"),
                }))
                .await;
            }),
        )
        .await;
    }
}

/// The page's records (`load_tree` on its loader), through `done`.
async fn page_records(h: &Host, done: Done<Result<Vec<Map<String, Value>>, String>>) {
    h.operator_then(crate::kern::web::CANVAS_LOADER_ID, json!({"type": "load_tree"}), move |out| async move {
        let r = out.result;
        done(match err_of(&r) {
            Some(e) => Err(format!("the page's records: {e}")),
            None => Ok(r.get("records").and_then(Value::as_array).into_iter().flatten().filter_map(|v| v.as_object().cloned()).collect()),
        })
        .await;
    })
    .await;
}

pub(crate) async fn persist(h: &Host, rec: Map<String, Value>, done: Done<Result<(), String>>) {
    let id = rec.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    h.operator_then(crate::kern::web::CANVAS_LOADER_ID, json!({"type": "persist_record", "record": rec}), move |out| async move {
        done(err_of(&out.result).map_or(Ok(()), |e| Err(format!("writing the page record `{id}`: {e}")))).await;
    })
    .await;
}

/// Whether the page has its view yet (it is made at the page's first load).
pub(crate) async fn has_view(h: &Host, done: Done<Result<bool, String>>) {
    page_records(
        h,
        crate::kern::cps::done(move |r: Result<Vec<Map<String, Value>>, String>| async move {
            done(r.map(|recs| recs.iter().any(|r| r.get("id").and_then(Value::as_str) == Some(VIEW)))).await;
        }),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kern::backend::create_tests::{host, record_of};

    fn vllm(port: u16) -> Backend {
        Backend::Vllm {
            host: "box.example".into(),
            port,
            kv_overflow: "evict".into(),
        }
    }

    #[tokio::test]
    async fn a_repoint_updates_the_provider_and_a_failed_remake_puts_both_records_back() {
        let (h, dir) = host("root-ai").await;
        assert_eq!(
            {
                let b = vllm(8000);
                crate::kern::cps::run(|d| run(&h, &b, d)).await.unwrap()
            },
            "the root AI `ai` (made, vllm_backend.tools) speaks to http://box.example:8000/v1"
        );
        // the same backend: the provider found through the chat is re-pointed, the chat keeps its provider
        let b = vllm(8001);
        let out = crate::kern::cps::run(|d| run(&h, &b, d)).await.unwrap();
        assert!(out.contains("re-pointed"), "{out}");
        let (chat, prov) = (record_of(&h, ROOT_AI), record_of(&h, "ai_provider"));
        assert_eq!(prov["endpoint"], json!("http://box.example:8001/v1"));
        assert_eq!(chat["provider_id"], json!("ai_provider"));
        assert!(
            chat.get("endpoint").is_none(),
            "the endpoint is the provider's alone"
        );
        // another backend whose record is refused: the old chat AND provider are put back
        let ollama = Backend::Ollama {
            host: "box.example".into(),
            port: 11434,
        };
        let err = crate::kern::cps::run(|d| run(&h, &ollama, d)).await.unwrap_err();
        assert!(err.contains("is left on vllm_backend.tools"), "{err}");
        assert_eq!(
            record_of(&h, "ai_provider")["endpoint"],
            json!("http://box.example:8001/v1"),
            "the provider is back"
        );
        assert_eq!(
            record_of(&h, ROOT_AI)["provider_id"],
            json!("ai_provider"),
            "and the chat"
        );
        let handler = |id: &str| {
            h.kernel.resolve_target(id)
                .unwrap()
                .record()
                .handler_module
        };
        assert_eq!(
            handler("ai_provider").as_deref(),
            Some("vllm_backend.tools")
        );
        assert_eq!(handler(ROOT_AI).as_deref(), Some("ai_chat.tools"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
