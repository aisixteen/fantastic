//! The **dynamic input control** (Claude-Code-style) + the `/setup` · `/model`
//! wizard. The chat input region is normally the [`Control::Chat`] composer, but a
//! flow can switch it to an arrow-key [`Control::Select`] or a (maskable)
//! [`Control::Field`]. Pure + unit-tested; the TUI drives keys + rendering off it.

/// The providers offered by `/setup` (the wired backends).
pub const PROVIDERS: [&str; 3] = ["ollama", "nvidia", "anthropic"];

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
}

impl Control {
    pub fn select_up(&mut self) {
        if let Control::Select { cursor, .. } = self {
            *cursor = cursor.saturating_sub(1);
        }
    }
    pub fn select_down(&mut self) {
        if let Control::Select {
            cursor, options, ..
        } = self
        {
            if *cursor + 1 < options.len() {
                *cursor += 1;
            }
        }
    }
    pub fn type_char(&mut self, c: char) {
        if let Control::Field { value, .. } = self {
            value.push(c);
        }
    }
    pub fn backspace(&mut self) {
        if let Control::Field { value, .. } = self {
            value.pop();
        }
    }
    /// The value submitted on Enter (the selected option, or the field text).
    pub fn submitted_value(&self) -> String {
        match self {
            Control::Select {
                options, cursor, ..
            } => options.get(*cursor).cloned().unwrap_or_default(),
            Control::Field { value, .. } => value.clone(),
            Control::Chat => String::new(),
        }
    }
}

/// The connector being assembled by the wizard.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Draft {
    pub backend: String,
    pub model: String,
    /// Empty when no key was entered (e.g. ollama, or a `/model` change that keeps
    /// the existing key).
    pub key: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Provider,
    Model,
    Key,
}

/// The result of submitting the current control.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Advance {
    /// Show this next control.
    Next(Control),
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

    /// `/model` — keep the existing provider/key, just (re)choose the model.
    pub fn start_model(backend: &str, model: &str, key_present: bool) -> (Self, Control) {
        let flow = SetupFlow {
            step: Step::Model,
            draft: Draft {
                backend: backend.to_string(),
                model: model.to_string(),
                key: String::new(),
            },
            needs_key: needs_key(backend),
            key_present,
        };
        let ctrl = flow_model_control(model);
        (flow, ctrl)
    }

    /// Submit the current control's value; advance the wizard.
    pub fn submit(&mut self, value: String) -> Advance {
        match self.step {
            Step::Provider => {
                self.draft.backend = value.trim().to_string();
                self.needs_key = needs_key(&self.draft.backend);
                self.step = Step::Model;
                Advance::Next(flow_model_control(""))
            }
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
    fn setup_ollama_skips_key() {
        let (mut f, c0) = SetupFlow::start_setup();
        assert!(matches!(c0, Control::Select { .. }));
        // pick ollama → model field.
        let a = f.submit("ollama".into());
        assert!(matches!(
            a,
            Advance::Next(Control::Field { masked: false, .. })
        ));
        // enter model → DONE (ollama needs no key).
        match f.submit("gemma4:12b".into()) {
            Advance::Done(d) => {
                assert_eq!(d.backend, "ollama");
                assert_eq!(d.model, "gemma4:12b");
                assert!(d.key.is_empty());
            }
            _ => panic!("expected Done"),
        }
    }

    #[test]
    fn setup_nvidia_requires_key_step() {
        let (mut f, _) = SetupFlow::start_setup();
        f.submit("nvidia".into());
        // model → key field (masked).
        match f.submit("meta/llama-3.1-8b".into()) {
            Advance::Next(Control::Field { masked: true, .. }) => {}
            other => panic!("expected masked key field, got {other:?}"),
        }
        // key → DONE.
        match f.submit("sk-secret".into()) {
            Advance::Done(d) => {
                assert_eq!(d.backend, "nvidia");
                assert_eq!(d.key, "sk-secret");
            }
            _ => panic!("expected Done"),
        }
    }

    #[test]
    fn model_only_flow_skips_key_when_already_present() {
        // /model on an nvidia connector that already has a key → no key step.
        let (mut f, c0) = SetupFlow::start_model("nvidia", "old-model", true);
        assert!(matches!(c0, Control::Field { masked: false, .. }));
        match f.submit("new-model".into()) {
            Advance::Done(d) => {
                assert_eq!(d.backend, "nvidia");
                assert_eq!(d.model, "new-model");
            }
            _ => panic!("expected Done (key already present)"),
        }
    }
}
