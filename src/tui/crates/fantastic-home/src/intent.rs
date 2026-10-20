//! THE USER'S INTENTS, typed: what a page, a menu or a shell asks of the home.
//!
//! The wire shape is the `home` agent's verbs, exactly:
//!
//! ```json
//! {"type":"open","paths":["/a"]}   {"type":"start","id":"g"}   {"type":"stop","id":"g"}
//! {"type":"remove","id":"g"}       {"type":"select","id":null} {"type":"select_page","index":0}
//! {"type":"close_page","id":"g"}
//! ```
//!
//! The agent parses those verbs into an `Intent` before it checks them against
//! the home; the menu spec carries them as what an item sends.

use serde::{Deserialize, Serialize};

use crate::viewmodel::Event;

/// One user intent.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Intent {
    /// Open these folders (absolute; a listed row's path as it is).
    Open {
        paths: Vec<String>,
    },
    Start {
        id: String,
    },
    Stop {
        id: String,
    },
    /// Remove an entry from the list.
    #[serde(rename = "remove")]
    Forget {
        id: String,
    },
    /// Show this entry's page; `None` shows the home.
    Select {
        id: Option<String>,
    },
    /// Show the open page at this index (0-based).
    SelectPage {
        index: usize,
    },
    ClosePage {
        id: String,
    },
}

impl Intent {
    /// Its verb, as the wire names it.
    pub fn verb(&self) -> &'static str {
        match self {
            Intent::Open { .. } => "open",
            Intent::Start { .. } => "start",
            Intent::Stop { .. } => "stop",
            Intent::Forget { .. } => "remove",
            Intent::Select { .. } => "select",
            Intent::SelectPage { .. } => "select_page",
            Intent::ClosePage { .. } => "close_page",
        }
    }

    /// The event it reduces, unstamped (`now_ms` 0: the OS layer stamps it).
    pub fn event(self) -> Event {
        match self {
            Intent::Open { paths } => Event::Open { paths, now_ms: 0 },
            Intent::Start { id } => Event::Start { id, now_ms: 0 },
            Intent::Stop { id } => Event::Stop { id },
            Intent::Forget { id } => Event::Forget { id },
            Intent::Select { id } => Event::Select { id },
            Intent::SelectPage { index } => Event::SelectPage { index },
            Intent::ClosePage { id } => Event::ClosePage { id },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intents_have_the_verbs_wire_shapes() {
        let cases = [
            (
                Intent::Open {
                    paths: vec!["/a".into()],
                },
                r#"{"type":"open","paths":["/a"]}"#,
            ),
            (
                Intent::Start { id: "g".into() },
                r#"{"type":"start","id":"g"}"#,
            ),
            (
                Intent::Stop { id: "g".into() },
                r#"{"type":"stop","id":"g"}"#,
            ),
            (
                Intent::Forget { id: "g".into() },
                r#"{"type":"remove","id":"g"}"#,
            ),
            (
                Intent::Select { id: None },
                r#"{"type":"select","id":null}"#,
            ),
            (
                Intent::Select {
                    id: Some("g".into()),
                },
                r#"{"type":"select","id":"g"}"#,
            ),
            (
                Intent::SelectPage { index: 2 },
                r#"{"type":"select_page","index":2}"#,
            ),
            (
                Intent::ClosePage { id: "g".into() },
                r#"{"type":"close_page","id":"g"}"#,
            ),
        ];
        for (intent, wire) in cases {
            assert_eq!(serde_json::to_string(&intent).expect("serializes"), wire);
            assert_eq!(
                serde_json::from_str::<Intent>(wire).expect(wire),
                intent,
                "{wire}"
            );
            let verb = format!(r#"{{"type":"{}""#, intent.verb());
            assert!(wire.starts_with(&verb), "{wire} names {verb}");
        }
    }

    #[test]
    fn an_unknown_field_or_verb_is_not_an_intent() {
        for wire in [
            r#"{"type":"start","id":"g","now":1}"#,
            r#"{"type":"forget","id":"g"}"#,
            r#"{"type":"select_page","index":-1}"#,
            r#"{"type":"open"}"#,
        ] {
            assert!(serde_json::from_str::<Intent>(wire).is_err(), "{wire}");
        }
    }
}
