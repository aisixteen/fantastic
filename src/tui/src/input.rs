//! The **dynamic input control** (Claude-Code-style) + the `/setup` · `/model`
//! wizard. The chat input region is normally the [`Control::Chat`] composer, but a
//! flow can switch it to an arrow-key [`Control::Select`], a (maskable)
//! [`Control::Field`], a live-filtered [`Control::Autocomplete`] model picker, or a
//! transient [`Control::Probing`] spinner while the ollama endpoint is reached.
//! Pure + unit-tested; the TUI drives keys + rendering off it.

/// The providers offered by `/setup` (the wired backends).
pub const PROVIDERS: [&str; 3] = ["ollama", "nvidia", "anthropic"];

/// The default ollama port — a *confirmed* prefill (editable), honoring "by
/// default no defaults": the user still sees + submits it, nothing silent.
pub const DEFAULT_OLLAMA_PORT: &str = "11434";

/// What the input region currently is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Control {
    /// The normal chat composer (rendered/handled via `chat::Composer`).
    Chat,
    /// Arrow-key select from a titled option list.
    Select {
        title: String,
        options: Vec<String>,
        cursor: usize,
    },
    /// A single-line text field; `masked` hides the value (API key).
    Field {
        title: String,
        value: String,
        masked: bool,
    },
    /// A model picker with **live autocomplete**: `options` is the full model list
    /// (from the `list_models` probe); `query` filters it (prefix, then substring);
    /// `cursor` indexes the FILTERED view. Type to filter, ↑↓ to move, Enter to pick.
    Autocomplete {
        title: String,
        options: Vec<String>,
        query: String,
        cursor: usize,
    },
    /// A transient "probing…" state shown while the ollama endpoint is being
    /// reached (`list_models`). Owns no editable value; Esc cancels the flow.
    Probing { title: String },
}

impl Control {
    pub fn select_up(&mut self) {
        match self {
            Control::Select { cursor, .. } | Control::Autocomplete { cursor, .. } => {
                *cursor = cursor.saturating_sub(1);
            }
            _ => {}
        }
    }
    pub fn select_down(&mut self) {
        match self {
            Control::Select {
                cursor, options, ..
            } => {
                if *cursor + 1 < options.len() {
                    *cursor += 1;
                }
            }
            Control::Autocomplete {
                cursor,
                options,
                query,
                ..
            } => {
                let n = filter_models(options, query).len();
                if *cursor + 1 < n {
                    *cursor += 1;
                }
            }
            _ => {}
        }
    }
    pub fn type_char(&mut self, c: char) {
        match self {
            Control::Field { value, .. } => value.push(c),
            // Typing filters the autocomplete; a changed query re-anchors the cursor.
            Control::Autocomplete { query, cursor, .. } => {
                query.push(c);
                *cursor = 0;
            }
            _ => {}
        }
    }
    pub fn backspace(&mut self) {
        match self {
            Control::Field { value, .. } => {
                value.pop();
            }
            Control::Autocomplete { query, cursor, .. } => {
                query.pop();
                *cursor = 0;
            }
            _ => {}
        }
    }
    /// The value submitted on Enter (the selected option, the field text, or the
    /// highlighted autocomplete match).
    pub fn submitted_value(&self) -> String {
        match self {
            Control::Select {
                options, cursor, ..
            } => options.get(*cursor).cloned().unwrap_or_default(),
            Control::Field { value, .. } => value.clone(),
            Control::Autocomplete {
                options,
                query,
                cursor,
                ..
            } => filter_models(options, query)
                .get(*cursor)
                .cloned()
                .unwrap_or_default(),
            Control::Chat | Control::Probing { .. } => String::new(),
        }
    }
}

/// Filter `options` by `query` (case-insensitive): **prefix matches first**, then
/// substring matches, each in the original order. An empty query returns all.
/// Pure — the autocomplete's whole matching rule, unit-tested.
pub fn filter_models(options: &[String], query: &str) -> Vec<String> {
    let q = query.trim().to_ascii_lowercase();
    if q.is_empty() {
        return options.to_vec();
    }
    let mut prefix: Vec<String> = Vec::new();
    let mut substr: Vec<String> = Vec::new();
    for o in options {
        let lo = o.to_ascii_lowercase();
        if lo.starts_with(&q) {
            prefix.push(o.clone());
        } else if lo.contains(&q) {
            substr.push(o.clone());
        }
    }
    prefix.extend(substr);
    prefix
}

/// The connector being assembled by the wizard.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Draft {
    pub backend: String,
    pub model: String,
    /// Empty when no key was entered (e.g. ollama, or a `/model` change that keeps
    /// the existing key).
    pub key: String,
    /// The full ollama endpoint (`http://127.0.0.1:<port>`), empty for keyed
    /// providers. Threaded into `set_ai_connector` + `FANTASTIC_OLLAMA_ENDPOINT`.
    pub endpoint: String,
    /// The chosen ollama port (for the `@ :<port>` confirmation), empty otherwise.
    pub port: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Provider,
    /// ollama only — the editable port field (prefilled `11434`).
    Port,
    /// ollama only — waiting for the `list_models` probe result.
    Probing,
    Model,
    Key,
}

/// The result of submitting the current control.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Advance {
    /// Show this next control.
    Next(Control),
    /// Probe this ollama endpoint (async, off the pure flow): the TUI runs
    /// `list_models`, then calls back [`SetupFlow::probe_succeeded`] /
    /// [`SetupFlow::probe_failed`]. Shows a "probing…" state meanwhile.
    Probe { endpoint: String },
    /// The wizard is done — persist this connector.
    Done(Draft),
}

/// The `/setup` (full) or `/model` (model-only) guided flow.
pub struct SetupFlow {
    step: Step,
    draft: Draft,
    /// True once the chosen provider needs an API key (nvidia/anthropic).
    needs_key: bool,
    /// True if a key is already stored (a `/model` change can then skip the key step).
    key_present: bool,
}

fn needs_key(backend: &str) -> bool {
    matches!(backend, "nvidia" | "anthropic")
}

impl SetupFlow {
    /// `/setup` — start from the provider chooser.
    pub fn start_setup() -> (Self, Control) {
        let flow = SetupFlow {
            step: Step::Provider,
            draft: Draft::default(),
            needs_key: false,
            key_present: false,
        };
        (flow, flow_provider_control())
    }

    /// `/model` — keep the existing provider/key, just (re)choose the model. For
    /// ollama it keeps the stored `endpoint` and jumps straight to a fresh probe
    /// (the model list may have changed); with no stored endpoint it re-asks the
    /// port. Keyed providers jump to the Model field as before.
    pub fn start_model(
        backend: &str,
        model: &str,
        key_present: bool,
        endpoint: Option<&str>,
    ) -> (Self, Advance) {
        let mut flow = SetupFlow {
            step: Step::Model,
            draft: Draft {
                backend: backend.to_string(),
                model: model.to_string(),
                key: String::new(),
                endpoint: String::new(),
                port: String::new(),
            },
            needs_key: needs_key(backend),
            key_present,
        };
        if backend == "ollama" {
            match endpoint.map(str::trim).filter(|e| !e.is_empty()) {
                Some(ep) => {
                    flow.draft.endpoint = ep.to_string();
                    flow.draft.port = port_of(ep);
                    flow.step = Step::Probing;
                    (
                        flow,
                        Advance::Probe {
                            endpoint: ep.to_string(),
                        },
                    )
                }
                None => {
                    flow.step = Step::Port;
                    (flow, Advance::Next(port_control(DEFAULT_OLLAMA_PORT)))
                }
            }
        } else {
            (flow, Advance::Next(flow_model_control(model)))
        }
    }

    /// Submit the current control's value; advance the wizard.
    pub fn submit(&mut self, value: String) -> Advance {
        match self.step {
            Step::Provider => {
                self.draft.backend = value.trim().to_string();
                self.needs_key = needs_key(&self.draft.backend);
                if self.draft.backend == "ollama" {
                    // ollama is keyless: pick the port, then probe → autocomplete.
                    self.step = Step::Port;
                    Advance::Next(port_control(DEFAULT_OLLAMA_PORT))
                } else {
                    self.step = Step::Model;
                    Advance::Next(flow_model_control(""))
                }
            }
            Step::Port => {
                let port = value.trim().to_string();
                let endpoint = format!("http://127.0.0.1:{port}");
                self.draft.port = port;
                self.draft.endpoint = endpoint.clone();
                self.step = Step::Probing;
                Advance::Probe { endpoint }
            }
            // Enter is inert while probing (the TUI gates it); kept total + benign.
            Step::Probing => Advance::Next(Control::Probing {
                title: "probing…".to_string(),
            }),
            Step::Model => {
                self.draft.model = value.trim().to_string();
                if self.needs_key && !self.key_present {
                    self.step = Step::Key;
                    Advance::Next(Control::Field {
                        title: format!("{} API key", self.draft.backend),
                        value: String::new(),
                        masked: true,
                    })
                } else {
                    Advance::Done(self.draft.clone())
                }
            }
            Step::Key => {
                self.draft.key = value;
                Advance::Done(self.draft.clone())
            }
        }
    }

    /// True while the flow is awaiting a probe result (guards stale/duplicate
    /// probe callbacks in the TUI event loop).
    pub fn is_probing(&self) -> bool {
        self.step == Step::Probing
    }

    /// The probe reached the endpoint: advance to the live-autocomplete Model
    /// picker over the installed model names.
    pub fn probe_succeeded(&mut self, models: Vec<String>) -> Control {
        self.step = Step::Model;
        Control::Autocomplete {
            title: "Model  (live /api/tags)".to_string(),
            options: models,
            query: String::new(),
            cursor: 0,
        }
    }

    /// The probe failed (unreachable / bad port): return to the Port step,
    /// prefilled with the port that was tried — reconnect, never a hang.
    pub fn probe_failed(&mut self) -> Control {
        self.step = Step::Port;
        port_control(&self.draft.port)
    }
}

fn flow_provider_control() -> Control {
    Control::Select {
        title: "Choose a provider".to_string(),
        options: PROVIDERS.iter().map(|s| s.to_string()).collect(),
        cursor: 0,
    }
}

fn flow_model_control(prefill: &str) -> Control {
    Control::Field {
        title: "Model id".to_string(),
        value: prefill.to_string(),
        masked: false,
    }
}

/// The ollama Port field (prefilled + editable — a confirmed value, not a silent
/// default).
fn port_control(prefill: &str) -> Control {
    Control::Field {
        title: "ollama port".to_string(),
        value: prefill.to_string(),
        masked: false,
    }
}

/// The port of an endpoint URL (`http://127.0.0.1:11434` → `11434`); falls back to
/// [`DEFAULT_OLLAMA_PORT`] when there's no trailing numeric port. Display-only.
fn port_of(endpoint: &str) -> String {
    endpoint
        .rsplit(':')
        .next()
        .map(|s| s.trim_end_matches('/'))
        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        .unwrap_or(DEFAULT_OLLAMA_PORT)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_cursor_clamps() {
        let mut c = Control::Select {
            title: "t".into(),
            options: vec!["a".into(), "b".into()],
            cursor: 0,
        };
        c.select_up(); // already at top → stays 0
        assert_eq!(c.submitted_value(), "a");
        c.select_down();
        assert_eq!(c.submitted_value(), "b");
        c.select_down(); // clamps at last
        assert_eq!(c.submitted_value(), "b");
    }

    #[test]
    fn field_edits_and_masking_holds_value() {
        let mut c = Control::Field {
            title: "k".into(),
            value: String::new(),
            masked: true,
        };
        for ch in "sk-1".chars() {
            c.type_char(ch);
        }
        c.backspace();
        assert_eq!(c.submitted_value(), "sk-"); // the real value is intact (mask is render-only)
    }

    #[test]
    fn filter_models_prefix_then_substring_case_insensitive() {
        let opts: Vec<String> = ["gemma3:12b", "gemma3:4b", "llama-gemma", "mistral"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        // Empty query → all, in order.
        assert_eq!(filter_models(&opts, "").len(), 4);
        // "gemma" → the two prefix matches first, then the substring match.
        assert_eq!(
            filter_models(&opts, "gemma"),
            vec![
                "gemma3:12b".to_string(),
                "gemma3:4b".to_string(),
                "llama-gemma".to_string()
            ]
        );
        // Case-insensitive.
        assert_eq!(filter_models(&opts, "MISTRAL"), vec!["mistral".to_string()]);
        // No match → empty.
        assert!(filter_models(&opts, "zzz").is_empty());
    }

    #[test]
    fn autocomplete_navigates_the_filtered_view() {
        let mut c = Control::Autocomplete {
            title: "Model".into(),
            options: ["gemma3:12b", "gemma3:4b", "mistral"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            query: String::new(),
            cursor: 0,
        };
        // Type to filter to the two gemma models; cursor re-anchors to 0.
        for ch in "gemma".chars() {
            c.type_char(ch);
        }
        assert_eq!(c.submitted_value(), "gemma3:12b");
        c.select_down();
        assert_eq!(c.submitted_value(), "gemma3:4b");
        c.select_down(); // clamps at the filtered end (2 items)
        assert_eq!(c.submitted_value(), "gemma3:4b");
        // Backspacing the query widens the list again + re-anchors.
        for _ in 0..5 {
            c.backspace();
        }
        assert_eq!(c.submitted_value(), "gemma3:12b");
    }

    #[test]
    fn setup_ollama_port_then_probe_then_autocomplete() {
        let (mut f, c0) = SetupFlow::start_setup();
        assert!(matches!(c0, Control::Select { .. }));
        // Pick ollama → the Port field, prefilled 11434 (editable).
        match f.submit("ollama".into()) {
            Advance::Next(Control::Field {
                title,
                value,
                masked: false,
            }) => {
                assert_eq!(title, "ollama port");
                assert_eq!(value, "11434");
            }
            other => panic!("expected ollama port field, got {other:?}"),
        }
        // Submit the port → an async probe of the endpoint.
        match f.submit("11500".into()) {
            Advance::Probe { endpoint } => assert_eq!(endpoint, "http://127.0.0.1:11500"),
            other => panic!("expected Probe, got {other:?}"),
        }
        assert!(f.is_probing());
        // The probe succeeds → a live-autocomplete Model picker over the names.
        let ctrl = f.probe_succeeded(vec!["gemma3:12b".into(), "gemma3:4b".into()]);
        assert!(matches!(ctrl, Control::Autocomplete { .. }));
        assert!(!f.is_probing());
        // Pick a model → DONE (ollama is keyless), endpoint + port threaded through.
        match f.submit("gemma3:4b".into()) {
            Advance::Done(d) => {
                assert_eq!(d.backend, "ollama");
                assert_eq!(d.model, "gemma3:4b");
                assert!(d.key.is_empty());
                assert_eq!(d.endpoint, "http://127.0.0.1:11500");
                assert_eq!(d.port, "11500");
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn probe_failure_returns_to_the_port_step_prefilled() {
        let (mut f, _) = SetupFlow::start_setup();
        f.submit("ollama".into());
        let _ = f.submit("9999".into()); // → Probe
        assert!(f.is_probing());
        // Unreachable → the flow returns to the Port step (reconnect), not a hang,
        // prefilled with the port that was tried.
        match f.probe_failed() {
            Control::Field { title, value, .. } => {
                assert_eq!(title, "ollama port");
                assert_eq!(value, "9999");
            }
            other => panic!("expected the port field back, got {other:?}"),
        }
        assert!(!f.is_probing());
    }

    #[test]
    fn setup_nvidia_requires_key_step() {
        let (mut f, _) = SetupFlow::start_setup();
        // nvidia is keyed → straight to the Model field (no port step).
        match f.submit("nvidia".into()) {
            Advance::Next(Control::Field { masked: false, .. }) => {}
            other => panic!("expected model field, got {other:?}"),
        }
        // model → key field (masked).
        match f.submit("meta/llama-3.1-8b".into()) {
            Advance::Next(Control::Field { masked: true, .. }) => {}
            other => panic!("expected masked key field, got {other:?}"),
        }
        // key → DONE (no endpoint for a keyed provider).
        match f.submit("sk-secret".into()) {
            Advance::Done(d) => {
                assert_eq!(d.backend, "nvidia");
                assert_eq!(d.key, "sk-secret");
                assert!(d.endpoint.is_empty());
            }
            _ => panic!("expected Done"),
        }
    }

    #[test]
    fn model_only_ollama_probes_stored_endpoint_or_reasks_port() {
        // /model on an ollama connector with a stored endpoint → re-probe it.
        let (f, adv) = SetupFlow::start_model("ollama", "old", false, Some("http://box:11434"));
        match adv {
            Advance::Probe { endpoint } => assert_eq!(endpoint, "http://box:11434"),
            other => panic!("expected Probe, got {other:?}"),
        }
        assert!(f.is_probing());
        // /model on ollama with NO stored endpoint → re-ask the port.
        let (_f, adv) = SetupFlow::start_model("ollama", "old", false, None);
        assert!(matches!(
            adv,
            Advance::Next(Control::Field { masked: false, .. })
        ));
    }

    #[test]
    fn model_only_flow_skips_key_when_already_present() {
        // /model on an nvidia connector that already has a key → no key step.
        let (mut f, adv) = SetupFlow::start_model("nvidia", "old-model", true, None);
        assert!(matches!(
            adv,
            Advance::Next(Control::Field { masked: false, .. })
        ));
        match f.submit("new-model".into()) {
            Advance::Done(d) => {
                assert_eq!(d.backend, "nvidia");
                assert_eq!(d.model, "new-model");
            }
            _ => panic!("expected Done (key already present)"),
        }
    }
}
