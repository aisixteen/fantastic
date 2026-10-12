//! THE AI SET-UP WIZARD's pure part (owner 2026-10-12: the terminal's set-up page
//! is the canvas's two-page wizard, `web/canvas/…/setup_view/wizard.ts`): the
//! four providers, a new agent's auto name, which stored key fits an endpoint,
//! and page 2's details — what it starts at, what is missing, what it sends. No
//! terminal, so the units test it.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::kern::saved::Saved;
use crate::kern::setup::{text_of, Backend, Field};
use crate::kern::wizard::KeyIn;

/// A provider page 1 offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Provider {
    pub module: &'static str,
    pub label: &'static str,
    /// the URL page 2 starts at
    pub endpoint: &'static str,
}

/// The providers page 1 offers, in its order (the canvas's).
pub const PROVIDERS: [Provider; 4] = [
    Provider { module: "responses_backend.tools", label: "responses", endpoint: "https://api.openai.com/v1/responses" },
    Provider { module: "anthropic_backend.tools", label: "anthropic", endpoint: "https://api.anthropic.com/v1/messages" },
    Provider { module: "ollama_backend.tools", label: "ollama", endpoint: "http://127.0.0.1:11434" },
    Provider { module: "vllm_backend.tools", label: "vllm", endpoint: "http://127.0.0.1:8000/v1" },
];

/// A provider's short name: `vllm_backend.tools` → `vllm`.
pub fn provider_of(module: &str) -> String {
    module.strip_suffix("_backend.tools").or_else(|| module.strip_suffix(".tools")).unwrap_or(module).to_string()
}

/// The first free `<provider>-<n>` among the names already taken.
pub fn auto_name(module: &str, taken: &[String]) -> String {
    let base = provider_of(module);
    (1..).map(|n| format!("{base}-{n}")).find(|c| !taken.contains(c)).expect("a free name")
}

/// A name a saved set-up (and its agent) may take.
pub fn name_ok(n: &str) -> bool {
    !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The fields page 2 shows on their own lines (the rest are the backend's own).
pub const MAIN: [&str; 2] = ["endpoint", "model"];

/// Where page 2's key comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    /// the stored key for this provider at the URL's host
    Saved,
    /// pasted now
    New,
    /// none
    NoKey,
}

/// Page 2: one provider's details — a NEW one, or a saved set-up being edited.
#[derive(Debug, Clone, PartialEq)]
pub struct Details {
    pub module: String,
    /// editing saved set-up `name` (saved back under it); `None`: a new one
    pub edit: Option<String>,
    pub name: String,
    pub url: String,
    pub model: String,
    /// the backend's own fields, as typed
    pub extra: BTreeMap<String, String>,
    /// the human's pick; `None` follows the URL (a stored key for its host, when one fits)
    pub source: Option<KeySource>,
    pub secret: String,
    /// where keys go, when that is not chosen yet: `secure` | `plain`
    pub keep: &'static str,
    pub advanced: bool,
}

/// The backend's own fields: not [`MAIN`], not the key.
pub fn own(b: &Backend) -> Vec<&Field> {
    b.fields.iter().filter(|f| !MAIN.contains(&f.name.as_str()) && f.ty != "credentials").collect()
}

impl Details {
    /// Page 2 for backend `b`: a new one (its name auto among `taken`, its URL the
    /// provider's), or saved set-up `current` (its own values).
    pub fn new(b: &Backend, current: Option<&Saved>, taken: &[String]) -> Self {
        let provider = PROVIDERS.iter().find(|p| p.module == b.module);
        let default_of = |n: &str| b.fields.iter().find(|f| f.name == n).and_then(|f| f.default.as_ref()).map(text_of);
        let start = |n: &str| -> String {
            current
                .and_then(|s| s.record.get(n))
                .map(text_of)
                .or_else(|| if n == "endpoint" { provider.map(|p| p.endpoint.to_string()) } else { None })
                .or_else(|| default_of(n))
                .unwrap_or_default()
        };
        Self {
            module: b.module.clone(),
            edit: current.map(|s| s.name.clone()),
            name: current.map_or_else(|| auto_name(&b.module, taken), |s| s.name.clone()),
            url: start("endpoint"),
            model: start("model"),
            extra: own(b).iter().map(|f| (f.name.clone(), start(&f.name))).collect(),
            source: None,
            secret: String::new(),
            keep: "secure",
            advanced: false,
        }
    }

    /// The stored key's ref that fits this provider at the URL's host, if any.
    pub fn fit(&self, keys: &[(String, String)]) -> Option<String> {
        let r = crate::kern::keys::ref_for(&self.module, self.url.trim()).ok()?;
        keys.iter().any(|(k, _)| *k == r).then_some(r)
    }

    /// The key sources offered (none when the backend takes no key).
    pub fn sources(&self, b: &Backend, keys: &[(String, String)]) -> Vec<KeySource> {
        if b.key.is_none() {
            return Vec::new();
        }
        let mut v = Vec::new();
        if self.fit(keys).is_some() {
            v.push(KeySource::Saved);
        }
        v.extend([KeySource::New, KeySource::NoKey]);
        v
    }

    /// The key source in force: the human's pick while offered, else the stored
    /// key for the URL's host, else a new one when the backend requires one.
    pub fn key_source(&self, b: &Backend, keys: &[(String, String)]) -> Option<KeySource> {
        let offered = self.sources(b, keys);
        if offered.is_empty() {
            return None;
        }
        if let Some(s) = self.source.filter(|s| offered.contains(s)) {
            return Some(s);
        }
        Some(if offered.contains(&KeySource::Saved) {
            KeySource::Saved
        } else if b.key == Some(true) {
            KeySource::New
        } else {
            KeySource::NoKey
        })
    }

    /// What stops page 2 from going ahead, in words.
    pub fn missing(&self, b: &Backend, keys: &[(String, String)], taken: &[String]) -> Vec<String> {
        let mut m = Vec::new();
        let name = self.name.trim();
        if self.edit.is_none() {
            if !name_ok(name) {
                m.push("a name (letters, digits, - and _)".to_string());
            } else if taken.iter().any(|t| t == name) {
                m.push(format!("a new name — `{name}` is saved already"));
            }
        }
        let url = self.url.trim();
        if url.is_empty() {
            m.push("the URL".into());
        } else if !(url.starts_with("http://") || url.starts_with("https://")) {
            m.push("a URL starting with http:// or https://".into());
        }
        if b.fields.iter().any(|f| f.name == "model" && f.required) && self.model.trim().is_empty() {
            m.push("the model".into());
        }
        for f in own(b).into_iter().filter(|f| f.required) {
            if self.extra.get(&f.name).is_none_or(|v| v.trim().is_empty()) {
                m.push(f.name.replace('_', " "));
            }
        }
        match self.key_source(b, keys) {
            Some(KeySource::NoKey) if b.key == Some(true) => m.push("a key".into()),
            Some(KeySource::New) if self.secret.is_empty() => m.push("the key — paste it, or pick no key".into()),
            _ => {}
        }
        m
    }

    /// The field texts sent: the URL, the model, and each own field set away
    /// from the backend's default (an untouched default stays the backend's).
    pub fn texts(&self, b: &Backend) -> Map<String, Value> {
        let mut out = Map::new();
        out.insert("endpoint".into(), Value::String(self.url.trim().to_string()));
        if !self.model.trim().is_empty() {
            out.insert("model".into(), Value::String(self.model.trim().to_string()));
        }
        for f in own(b) {
            let v = self.extra.get(&f.name).map_or("", |v| v.trim());
            let d = f.default.as_ref().map(text_of).unwrap_or_default();
            if !v.is_empty() && v != d {
                out.insert(f.name.clone(), Value::String(v.to_string()));
            }
        }
        out
    }

    /// The key to store with the act: only a NEW one, and where keys go when
    /// `mode_chosen` is false.
    pub fn key_in(&self, b: &Backend, keys: &[(String, String)], mode_chosen: bool) -> Option<KeyIn> {
        (self.key_source(b, keys) == Some(KeySource::New)).then(|| KeyIn {
            secret: self.secret.clone(),
            mode: (!mode_chosen).then(|| self.keep.to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn vllm() -> Backend {
        Backend::from_schema(
            "vllm_backend.tools",
            &json!({"fields": [
                {"name": "endpoint", "type": "str", "required": true},
                {"name": "model", "type": "str", "required": true},
                {"name": "max_tokens", "type": "int", "required": true},
                {"name": "kv_overflow", "type": "enum", "required": false, "enum": ["evict", "reject"]},
                {"name": "temperature", "type": "float", "required": false, "default": 0.2},
                {"name": "credentials", "type": "credentials", "required": false},
                {"name": "memory_store_id", "type": "str", "required": true},
            ]}),
        )
    }

    fn keyed() -> Backend {
        Backend::from_schema(
            "responses_backend.tools",
            &json!({"fields": [
                {"name": "endpoint", "type": "str", "required": true},
                {"name": "model", "type": "str", "required": true},
                {"name": "credentials", "type": "credentials", "required": true},
            ]}),
        )
    }

    #[test]
    fn provider_names_and_the_first_free_auto_name() {
        assert_eq!(provider_of("vllm_backend.tools"), "vllm");
        assert_eq!(provider_of("x.tools"), "x");
        assert_eq!(auto_name("vllm_backend.tools", &[]), "vllm-1");
        assert_eq!(auto_name("vllm_backend.tools", &["vllm-1".into(), "vllm-2".into()]), "vllm-3");
        assert!(name_ok("vllm-3090_a") && !name_ok("a b") && !name_ok(""));
        assert_eq!(PROVIDERS.map(|p| p.label), ["responses", "anthropic", "ollama", "vllm"], "the canvas's order");
    }

    #[test]
    fn a_new_provider_starts_at_its_url_and_the_backends_defaults() {
        let d = Details::new(&vllm(), None, &["vllm-1".into()]);
        assert_eq!((d.name.as_str(), d.url.as_str(), d.model.as_str()), ("vllm-2", "http://127.0.0.1:8000/v1", ""));
        assert_eq!(d.extra.keys().collect::<Vec<_>>(), ["kv_overflow", "max_tokens", "temperature"], "not the URL, model, key or store");
        assert_eq!(d.extra["temperature"], "0.2");
        assert_eq!(d.edit, None);
    }

    #[test]
    fn a_saved_set_up_opens_with_its_own_values_and_name() {
        let s = Saved {
            name: "box".into(),
            version: String::new(),
            record: json!({"handler_module": "vllm_backend.tools", "endpoint": "http://box:8000/v1", "model": "q", "max_tokens": 4096})
                .as_object()
                .unwrap()
                .clone(),
        };
        let d = Details::new(&vllm(), Some(&s), &["box".into()]);
        assert_eq!((d.edit.as_deref(), d.name.as_str(), d.url.as_str(), d.model.as_str()), (Some("box"), "box", "http://box:8000/v1", "q"));
        assert_eq!(d.extra["max_tokens"], "4096");
        assert!(d.missing(&vllm(), &[], &["box".into()]).is_empty(), "its own name is no clash when editing");
    }

    #[test]
    fn missing_names_each_gap_like_the_canvas() {
        let b = vllm();
        let mut d = Details::new(&b, None, &[]);
        d.url = "127.0.0.1:8000/v1".into();
        assert_eq!(d.missing(&b, &[], &[]), ["a URL starting with http:// or https://", "the model", "max tokens"]);
        d.url = "http://h:1/v1".into();
        d.model = "m".into();
        d.extra.insert("max_tokens".into(), "262144".into());
        assert!(d.missing(&b, &[], &[]).is_empty());
        d.name = "bad name".into();
        assert_eq!(d.missing(&b, &[], &[]), ["a name (letters, digits, - and _)"]);
        d.name = "vllm-1".into();
        assert_eq!(d.missing(&b, &[], &["vllm-1".into()]), ["a new name — `vllm-1` is saved already"]);
    }

    #[test]
    fn the_key_follows_the_url_a_stored_one_for_its_host_else_new_when_required() {
        let b = keyed();
        let keys = vec![("responses/api.openai.com".to_string(), "api.openai.com".to_string())];
        let mut d = Details::new(&b, None, &[]);
        assert_eq!(d.sources(&b, &keys), [KeySource::Saved, KeySource::New, KeySource::NoKey]);
        assert_eq!(d.key_source(&b, &keys), Some(KeySource::Saved), "the stored key for the URL's host");
        d.url = "https://other.example/v1/responses".into();
        assert_eq!(d.key_source(&b, &keys), Some(KeySource::New), "another host: a key is bound to its host");
        assert!(d.missing(&b, &keys, &[]).contains(&"the key — paste it, or pick no key".to_string()));
        d.source = Some(KeySource::NoKey);
        assert!(d.missing(&b, &keys, &[]).contains(&"a key".to_string()), "a backend that requires one");
        assert_eq!(vllm().key, Some(false));
        assert_eq!(Details::new(&vllm(), None, &[]).key_source(&vllm(), &[]), Some(KeySource::NoKey), "optional: none by default");
    }

    #[test]
    fn it_sends_the_url_model_and_changed_fields_and_a_new_key_with_its_mode() {
        let b = vllm();
        let mut d = Details::new(&b, None, &[]);
        d.model = "m".into();
        d.extra.insert("max_tokens".into(), "512".into());
        d.extra.insert("kv_overflow".into(), "reject".into());
        assert_eq!(
            Value::Object(d.texts(&b)),
            json!({"endpoint": "http://127.0.0.1:8000/v1", "model": "m", "max_tokens": "512", "kv_overflow": "reject"}),
            "the untouched default temperature stays the backend's"
        );
        assert_eq!(d.key_in(&b, &[], false), None, "no key picked: none stored");
        d.source = Some(KeySource::New);
        d.secret = "sk-1".into();
        assert_eq!(d.key_in(&b, &[], false), Some(KeyIn { secret: "sk-1".into(), mode: Some("secure".into()) }));
        assert_eq!(d.key_in(&b, &[], true), Some(KeyIn { secret: "sk-1".into(), mode: None }), "the mode chosen already");
    }
}
