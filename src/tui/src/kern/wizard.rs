//! THE AI SET-UP WIZARD's host side (owner 2026-10-12: the terminal's set-up page
//! is the canvas's two-page wizard). Each act is ONE call, as the human:
//!
//! - [`create`]: a NEW provider's details → its key (when pasted: where keys are
//!   kept, if not chosen yet, then the key itself), the record typed by the
//!   backend's schema, saved under its name (the first saved one becomes the
//!   default), and an AI agent written from it — its id the name, or the next
//!   free `<name>-<n>`;
//! - [`from_saved`]: an AI agent from a saved set-up, as it is;
//! - [`save_edit`]: a saved set-up's fields rewritten under its name.
//!
//! The same steps the canvas's set-up cell takes through `kern/setup_agent`.

use serde_json::{json, Map, Value};

use crate::kern::cps::{self, Done};
use crate::kern::host::Host;
use crate::kern::saved::Shelf;

/// A key pasted on page 2: its value, and where keys are kept when that is not
/// chosen yet (`plain` | `secure`).
#[derive(Debug, Clone, PartialEq)]
pub struct KeyIn {
    pub secret: String,
    pub mode: Option<String>,
}

/// The first free agent id for `name`: itself, else `<name>-2`, `<name>-3`…
pub fn free_id(host: &Host, name: &str) -> String {
    let taken = |id: &str| host.kernel.exists(id);
    if !taken(name) {
        return name.to_string();
    }
    (2..).map(|n| format!("{name}-{n}")).find(|c| !taken(c)).expect("a free id")
}

/// The key pasted on page 2: where keys are kept first (when chosen here), then the key.
async fn key(host: &Host, module: &str, endpoint: &str, k: &KeyIn, done: Done<Result<(), String>>) {
    let (h, module, endpoint, secret) = (host.clone(), module.to_string(), endpoint.to_string(), k.secret.clone());
    let Some(m) = &k.mode else {
        return store_key(h, module, endpoint, secret, done).await;
    };
    let Some(m) = fantastic_user_home_store::secrets::Mode::parse(m) else {
        return done(Err(format!("no key mode {m:?} — plain or secure"))).await;
    };
    // the mode's own call settles before the key is stored
    crate::kern::keys::set_mode(
        host,
        m,
        cps::done(move |r: Result<String, String>| async move {
            match r {
                Ok(_) => store_key(h, module, endpoint, secret, done).await,
                Err(e) => done(Err(e)).await,
            }
        }),
    )
    .await;
}

async fn store_key(h: Host, module: String, endpoint: String, secret: String, done: Done<Result<(), String>>) {
    crate::kern::keys::store(&h, &module, &endpoint, &secret, cps::done(move |r: Result<String, String>| async move { done(r.map(|_| ())).await })).await;
}

/// Save `texts` for `module` under `name`: typed, its stored key NAMED (a ref); `done` gets
/// the typed fields.
async fn save(host: &Host, name: &str, module: &str, texts: &Map<String, Value>, done: Done<Result<Map<String, Value>, String>>) {
    let fields = match crate::kern::setup::typed(host, module, texts) {
        Ok(f) => f,
        Err(e) => return done(Err(e)).await,
    };
    let mut record = fields.clone();
    if let Err(e) = crate::kern::keys::attach(host, module, &mut record) {
        return done(Err(e)).await;
    }
    record.insert("handler_module".into(), json!(module));
    crate::kern::saved::save(host, name, Value::Object(record), cps::done(move |r: Result<String, String>| async move { done(r.map(|_| fields)).await })).await;
}

async fn write(host: &Host, name: &str, module: &str, fields: Map<String, Value>, done: Done<Result<String, String>>) {
    let id = free_id(host, name);
    let rec = crate::kern::setup::Record { id: id.clone(), handler: module.to_string(), fields, may_call: Vec::new() };
    crate::kern::setup::write(host, &rec, cps::done(move |r: Result<String, String>| async move { done(r.map(|_| id)).await })).await;
}

/// The step after the key (or with none): the set-up saved, made the default when no default
/// is set yet, the agent written. Each call is sent when the one before settled.
async fn saved_then_written(host: Host, name: String, module: String, texts: Map<String, Value>, done: Done<Result<String, String>>) {
    let (h, n, m) = (host.clone(), name.clone(), module.clone());
    save(
        &host,
        &name,
        &module,
        &texts,
        cps::done(move |r: Result<Map<String, Value>, String>| async move {
            let fields = match r {
                Ok(f) => f,
                Err(e) => return done(Err(e)).await,
            };
            let h2 = h.clone();
            crate::kern::saved::shelf(
                &h,
                cps::done(move |r: Result<Shelf, String>| async move {
                    let shelf = match r {
                        Ok(s) => s,
                        Err(e) => return done(Err(e)).await,
                    };
                    if shelf.preferred.is_some() {
                        return write(&h2, &n, &m, fields, done).await;
                    }
                    let (h3, n3, m3) = (h2.clone(), n.clone(), m.clone());
                    crate::kern::saved::prefer(
                        &h2,
                        Some(&n),
                        cps::done(move |r: Result<Option<String>, String>| async move {
                            match r {
                                Ok(_) => write(&h3, &n3, &m3, fields, done).await,
                                Err(e) => done(Err(e)).await,
                            }
                        }),
                    )
                    .await;
                }),
            )
            .await;
        }),
    )
    .await;
}

/// A NEW provider's details → key, saved set-up (the first is the default), agent; `done` gets
/// the agent's id.
pub async fn create(host: &Host, name: &str, module: &str, texts: &Map<String, Value>, k: Option<&KeyIn>, done: Done<Result<String, String>>) {
    let endpoint = texts.get("endpoint").and_then(Value::as_str).unwrap_or("").to_string();
    let Some(k) = k else {
        return saved_then_written(host.clone(), name.to_string(), module.to_string(), texts.clone(), done).await;
    };
    let (h, name2, module2, texts2) = (host.clone(), name.to_string(), module.to_string(), texts.clone());
    key(
        host,
        module,
        &endpoint,
        k,
        cps::done(move |r: Result<(), String>| async move {
            match r {
                Ok(()) => saved_then_written(h, name2, module2, texts2, done).await,
                Err(e) => done(Err(e)).await,
            }
        }),
    )
    .await;
}

/// An AI agent from saved set-up `name`; `done` gets its id.
pub async fn from_saved(host: &Host, name: &str, done: Done<Result<String, String>>) {
    let (h, name) = (host.clone(), name.to_string());
    crate::kern::saved::shelf(
        host,
        cps::done(move |r: Result<Shelf, String>| async move {
            let shelf = match r {
                Ok(s) => s,
                Err(e) => return done(Err(e)).await,
            };
            let Some(s) = shelf.saved.iter().find(|s| s.name == name) else {
                return done(Err(format!("no saved set-up `{name}`"))).await;
            };
            let module = s.handler();
            // its key is named again by the write, for its endpoint (`kern/keys`)
            let fields: Map<String, Value> =
                s.record.iter().filter(|(k, _)| !matches!(k.as_str(), "handler_module" | "credentials")).map(|(k, v)| (k.clone(), v.clone())).collect();
            write(&h, &name, &module, fields, done).await;
        }),
    )
    .await;
}

/// A saved set-up's fields rewritten under its name (and a key, when pasted).
pub async fn save_edit(host: &Host, name: &str, module: &str, texts: &Map<String, Value>, k: Option<&KeyIn>, done: Done<Result<(), String>>) {
    let endpoint = texts.get("endpoint").and_then(Value::as_str).unwrap_or("").to_string();
    let saved = |h: Host, name: String, module: String, texts: Map<String, Value>, done: Done<Result<(), String>>| async move {
        save(&h, &name, &module, &texts, cps::done(move |r: Result<Map<String, Value>, String>| async move { done(r.map(|_| ())).await })).await;
    };
    let Some(k) = k else {
        return saved(host.clone(), name.to_string(), module.to_string(), texts.clone(), done).await;
    };
    let (h, name2, module2, texts2) = (host.clone(), name.to_string(), module.to_string(), texts.clone());
    key(
        host,
        module,
        &endpoint,
        k,
        cps::done(move |r: Result<(), String>| async move {
            match r {
                Ok(()) => saved(h, name2, module2, texts2, done).await,
                Err(e) => done(Err(e)).await,
            }
        }),
    )
    .await;
}

#[cfg(all(test, feature = "vllm"))]
mod tests {
    use std::sync::Arc;

    use fantastic_kernel::{Agent, AgentId, Kernel};
    use fantastic_user_home_store::user_store::{self, UserStore};
    use serde_json::{json, Map, Value};

    use crate::env::ClientCfg;
    use crate::kern::host::Host;

    const VLLM: &str = fantastic_vllm_backend::HANDLER_MODULE;

    /// A host with the real vLLM backend, the real chat, and the user's home (`home` under
    /// `user`) in `dir`.
    fn host(dir: &std::path::Path) -> Host {
        let mut k = Kernel::new();
        k.bundles.register(
            user_store::HANDLER_MODULE,
            UserStore::new(Ok(dir.join("home")), dir.join("project")),
        );
        k.bundles.register(VLLM, fantastic_vllm_backend::VllmBundle);
        k.bundles.register(
            fantastic_ai_core::chat_bundle::HANDLER_MODULE,
            fantastic_ai_core::chat_bundle::AiChatBundle,
        );
        let k = Arc::new(k);
        let agent = |id: &str, hm: Option<&str>, parent: Option<&str>| {
            Agent::new(
                AgentId::from(id),
                hm.map(str::to_string),
                parent.map(AgentId::from),
                Map::new(),
                dir.to_path_buf(),
                false,
            )
        };
        let root = agent("kernel_state", None, None);
        fantastic_kernel::test_support::register(&k, Arc::clone(&root));
        k.set_root(root);
        fantastic_kernel::test_support::register(
            &k,
            agent(crate::env::HUMAN, None, Some("kernel_state")),
        );
        fantastic_kernel::test_support::register(
            &k,
            agent(
                crate::kern::saved::HOME_ID,
                Some(user_store::HANDLER_MODULE),
                Some(crate::env::HUMAN),
            ),
        );
        Host {
            kernel: k,
            cfg: ClientCfg::here(),
            root: "kernel_state".to_string(),
        }
    }

    /// E17: the wizard's page-2 texts with request knobs set (#632: the chat's) are saved as
    /// TWO records, each judged by its own bundle: the provider's share under the name, the
    /// chat's under `<name>__chat`. The shelf shows them as one set-up, every knob on the
    /// chat's record and none on the provider's; forgetting it forgets both.
    #[tokio::test]
    async fn the_wizard_saves_a_set_up_with_knobs_as_two_records() {
        let dir = std::env::temp_dir().join(format!("fc-wizard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let h = host(&dir);
        let texts: Map<String, Value> = json!({
            "endpoint": "http://box.example:8000/v1",
            "model": "m1",
            "kv_overflow": "evict",
            "reasoning_effort": "medium",
            "min_p": "0.05",
            "max_tokens": "4096",
        })
        .as_object()
        .unwrap()
        .clone();
        crate::kern::cps::run(|d| super::save_edit(&h, "vllm-1", VLLM, &texts, None, d))
            .await
            .expect("the set-up is saved");
        let read = |n: &str| {
            let p = dir.join("home").join("user").join("setups").join("ai_agent").join(format!("{n}.yaml"));
            std::fs::read_to_string(&p).unwrap_or_default()
        };
        let (provider, chat) = (read("vllm-1"), read("vllm-1__chat"));
        assert!(provider.contains(VLLM), "the provider's record: {provider}");
        assert!(chat.contains(fantastic_ai_core::chat_bundle::HANDLER_MODULE), "the chat's record: {chat}");
        for k in ["reasoning_effort", "min_p", "max_tokens"] {
            assert!(chat.contains(k), "{k} on the chat's record: {chat}");
            assert!(!provider.contains(k), "{k} leaked to the provider: {provider}");
        }
        let shelf = crate::kern::cps::run(|d| crate::kern::saved::shelf(&h, d)).await.unwrap();
        assert_eq!(shelf.saved.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), vec!["vllm-1"], "one set-up");
        let saved = &shelf.saved[0].record;
        assert_eq!(
            (saved["reasoning_effort"].clone(), saved["min_p"].clone(), saved["max_tokens"].clone(), saved["handler_module"].clone()),
            (json!("medium"), json!(0.05), json!(4096), json!(VLLM))
        );
        crate::kern::cps::run(|d| crate::kern::saved::forget(&h, "vllm-1", d)).await.expect("forgotten");
        assert!(read("vllm-1").is_empty() && read("vllm-1__chat").is_empty(), "both records forgotten");
        assert!(crate::kern::cps::run(|d| crate::kern::saved::save(&h, "x__chat", json!({"handler_module": VLLM}), d)).await.is_err(), "the suffix is reserved");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
