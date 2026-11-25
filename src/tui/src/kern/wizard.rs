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

use crate::kern::host::Host;

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

async fn key(host: &Host, module: &str, endpoint: &str, k: &KeyIn) -> Result<(), String> {
    if let Some(m) = &k.mode {
        let m = fantastic_user_home_store::secrets::Mode::parse(m).ok_or_else(|| format!("no key mode {m:?} — plain or secure"))?;
        crate::kern::keys::set_mode(host, m).await?;
    }
    crate::kern::keys::store(host, module, endpoint, &k.secret).await.map(|_| ())
}

/// Save `texts` for `module` under `name`: typed, its stored key NAMED (a ref).
async fn save(host: &Host, name: &str, module: &str, texts: &Map<String, Value>) -> Result<Map<String, Value>, String> {
    let fields = crate::kern::setup::typed(host, module, texts)?;
    let mut record = fields.clone();
    crate::kern::keys::attach(host, module, &mut record)?;
    record.insert("handler_module".into(), json!(module));
    crate::kern::saved::save(host, name, Value::Object(record)).await?;
    Ok(fields)
}

async fn write(host: &Host, name: &str, module: &str, fields: Map<String, Value>) -> Result<String, String> {
    let id = free_id(host, name);
    let rec = crate::kern::setup::Record { id: id.clone(), handler: module.to_string(), fields, may_call: Vec::new() };
    crate::kern::setup::write(host, &rec).await?;
    Ok(id)
}

/// A NEW provider's details → key, saved set-up (the first is the default), agent.
/// Returns the agent's id.
pub async fn create(host: &Host, name: &str, module: &str, texts: &Map<String, Value>, k: Option<&KeyIn>) -> Result<String, String> {
    let endpoint = texts.get("endpoint").and_then(Value::as_str).unwrap_or("").to_string();
    if let Some(k) = k {
        key(host, module, &endpoint, k).await?;
    }
    let fields = save(host, name, module, texts).await?;
    if crate::kern::saved::shelf(host).await?.preferred.is_none() {
        crate::kern::saved::prefer(host, Some(name)).await?;
    }
    write(host, name, module, fields).await
}

/// An AI agent from saved set-up `name`. Returns its id.
pub async fn from_saved(host: &Host, name: &str) -> Result<String, String> {
    let shelf = crate::kern::saved::shelf(host).await?;
    let s = shelf.saved.iter().find(|s| s.name == name).ok_or_else(|| format!("no saved set-up `{name}`"))?;
    let module = s.handler();
    // its key is named again by the write, for its endpoint (`kern/keys`)
    let fields: Map<String, Value> =
        s.record.iter().filter(|(k, _)| !matches!(k.as_str(), "handler_module" | "credentials")).map(|(k, v)| (k.clone(), v.clone())).collect();
    write(host, name, &module, fields).await
}

/// A saved set-up's fields rewritten under its name (and a key, when pasted).
pub async fn save_edit(host: &Host, name: &str, module: &str, texts: &Map<String, Value>, k: Option<&KeyIn>) -> Result<(), String> {
    let endpoint = texts.get("endpoint").and_then(Value::as_str).unwrap_or("").to_string();
    if let Some(k) = k {
        key(host, module, &endpoint, k).await?;
    }
    save(host, name, module, texts).await.map(|_| ())
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
        super::save_edit(&h, "vllm-1", VLLM, &texts, None)
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
        let shelf = crate::kern::saved::shelf(&h).await.unwrap();
        assert_eq!(shelf.saved.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), vec!["vllm-1"], "one set-up");
        let saved = &shelf.saved[0].record;
        assert_eq!(
            (saved["reasoning_effort"].clone(), saved["min_p"].clone(), saved["max_tokens"].clone(), saved["handler_module"].clone()),
            (json!("medium"), json!(0.05), json!(4096), json!(VLLM))
        );
        crate::kern::saved::forget(&h, "vllm-1").await.expect("forgotten");
        assert!(read("vllm-1").is_empty() && read("vllm-1__chat").is_empty(), "both records forgotten");
        assert!(crate::kern::saved::save(&h, "x__chat", json!({"handler_module": VLLM})).await.is_err(), "the suffix is reserved");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
