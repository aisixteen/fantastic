//! The **dry stand-in brain** — what answers `@ai` on first runs, before a real
//! connector is configured (or when the configured model is unreachable). It
//! occupies the brain slot and, for now, returns canned guidance ("no model — run
//! /setup"). The [`DryBrain`] trait is the **protocol seam** the bundled lite-LLM
//! fills later; for now [`CannedDryBrain`] is the dumb module.

/// The configuration state of the AI connector, resolved from the (hydrated) env.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Backend + model set (+ key if the provider needs one) — the real brain runs.
    Ready { backend: String, model: String },
    /// No (valid) backend configured.
    NoBackend,
    /// Backend set, but no model.
    NoModel { backend: String },
    /// A key-requiring backend (nvidia/anthropic) with no key.
    NoKey { backend: String },
}

/// Pure classifier (unit-tested): given backend, model, and whether a key is
/// present, what's the connector status?
fn status_for(backend: Option<&str>, model: Option<&str>, key_present: bool) -> Status {
    let backend = match backend.map(str::trim).filter(|b| !b.is_empty()) {
        Some(b) if matches!(b, "ollama" | "nvidia" | "anthropic") => b,
        _ => return Status::NoBackend,
    };
    let model = match model.map(str::trim).filter(|m| !m.is_empty()) {
        Some(m) => m,
        None => {
            return Status::NoModel {
                backend: backend.to_string(),
            }
        }
    };
    if matches!(backend, "nvidia" | "anthropic") && !key_present {
        return Status::NoKey {
            backend: backend.to_string(),
        };
    }
    Status::Ready {
        backend: backend.to_string(),
        model: model.to_string(),
    }
}

/// Resolve the connector status from the current (hydrated) environment.
pub fn config_status() -> Status {
    let backend = std::env::var("FANTASTIC_AI_BACKEND").ok();
    let model = std::env::var("FANTASTIC_AI_MODEL").ok();
    let present = |k: &str| {
        std::env::var(k)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .is_some()
    };
    let key_present =
        present("FANTASTIC_AI_KEY") || present("ANTHROPIC_API_KEY") || present("NVIDIA_API_KEY");
    status_for(backend.as_deref(), model.as_deref(), key_present)
}

/// The dry brain interface. The bundled lite-LLM will implement this later; today
/// [`CannedDryBrain`] returns fixed guidance.
pub trait DryBrain: Send + Sync {
    fn reply(&self, user_text: &str, status: &Status, last_error: Option<&str>) -> String;
}

/// The dumb stand-in: canned setup guidance, ignoring the user's text.
pub struct CannedDryBrain;

impl DryBrain for CannedDryBrain {
    fn reply(&self, _user_text: &str, status: &Status, last_error: Option<&str>) -> String {
        guidance(status, last_error)
    }
}

/// The canned guidance line for a status. Empty string = no guidance needed
/// (Ready with no error).
pub fn guidance(status: &Status, last_error: Option<&str>) -> String {
    match status {
        Status::Ready { model, .. } => match last_error {
            Some(e) => format!(
                "the model “{model}” is unreachable ({e}).\nRun /model to set another, or check the provider is up."
            ),
            None => String::new(),
        },
        Status::NoBackend => "No AI connector is set up yet.\nRun /setup to add a provider + model — the key is stored in your OS keychain. (/model to change later.)".to_string(),
        Status::NoModel { backend } => format!(
            "Provider “{backend}” is set, but no model.\nRun /model to choose one."
        ),
        Status::NoKey { backend } => format!(
            "Provider “{backend}” needs an API key.\nRun /setup to add it (stored in your OS keychain, never on disk)."
        ),
    }
}

/// The dry stand-in's reply for a turn: `Some(guidance)` when not Ready (or a Ready
/// turn failed as unreachable), `None` when the real brain should answer.
pub fn dry_reply(status: &Status, last_error: Option<&str>) -> Option<String> {
    let g = guidance(status, last_error);
    if g.is_empty() {
        None
    } else {
        Some(g)
    }
}

/// Does a runtime send-error look like the model is unreachable (vs a real bug)?
/// Used to decide whether the dry "unreachable" guidance applies.
pub fn is_unreachable(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    [
        "http",
        "connection",
        "connect",
        "refused",
        "timeout",
        "404",
        "dns",
        "unreachable",
        "provider",
    ]
    .iter()
    .any(|m| e.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_classifies_each_case() {
        assert_eq!(status_for(None, None, false), Status::NoBackend);
        assert_eq!(
            status_for(Some("bogus"), Some("m"), false),
            Status::NoBackend
        );
        assert_eq!(
            status_for(Some("ollama"), None, false),
            Status::NoModel {
                backend: "ollama".into()
            }
        );
        // ollama needs no key → Ready with a model.
        assert_eq!(
            status_for(Some("ollama"), Some("gemma"), false),
            Status::Ready {
                backend: "ollama".into(),
                model: "gemma".into()
            }
        );
        // nvidia needs a key.
        assert_eq!(
            status_for(Some("nvidia"), Some("m"), false),
            Status::NoKey {
                backend: "nvidia".into()
            }
        );
        assert_eq!(
            status_for(Some("nvidia"), Some("m"), true),
            Status::Ready {
                backend: "nvidia".into(),
                model: "m".into()
            }
        );
    }

    #[test]
    fn dry_reply_guides_when_not_ready_and_is_silent_when_ready() {
        assert!(dry_reply(&Status::NoBackend, None)
            .unwrap()
            .contains("/setup"));
        assert!(dry_reply(
            &Status::NoModel {
                backend: "ollama".into()
            },
            None
        )
        .unwrap()
        .contains("/model"));
        assert!(dry_reply(
            &Status::NoKey {
                backend: "nvidia".into()
            },
            None
        )
        .unwrap()
        .contains("key"));
        // Ready + no error → no dry reply (the real brain answers).
        assert_eq!(
            dry_reply(
                &Status::Ready {
                    backend: "ollama".into(),
                    model: "m".into()
                },
                None
            ),
            None
        );
        // Ready + a runtime error → unreachable guidance.
        assert!(dry_reply(
            &Status::Ready {
                backend: "ollama".into(),
                model: "m".into()
            },
            Some("HTTP 404")
        )
        .unwrap()
        .contains("unreachable"));
    }

    #[test]
    fn unreachable_classifier() {
        assert!(is_unreachable("provider HTTP 500"));
        assert!(is_unreachable("connection refused"));
        assert!(!is_unreachable("invalid json in tool args"));
    }
}
