//! The LIVE **question card** — a small modal a mind raises by emitting a
//! `{type:"inquiry", …}` event on its own agent. The TUI mirrors that event into
//! its brain inbox; this module turns it into an interactive card, collects the
//! human's answer, and yields a `{type:"answer", …}` payload the shell `send`s back
//! to the asking agent.
//!
//! Self-contained + pure-testable (mirrors [`crate::input`]): parsing
//! ([`QuestionCard::from_event`]), key-driven mutation ([`QuestionCard::on_key`]),
//! and the accept/cancel payloads are unit-tested without a live terminal;
//! rendering lives here too ([`QuestionCard::render`]) so `lib.rs` stays a thin
//! shell (one field, an event-intercept, a ~5-line key shim, one render call).

use ratatui::prelude::*;
use ratatui::widgets::Paragraph;
use serde_json::{json, Value};

use ratatui::crossterm::event::KeyCode;

/// The four question shapes the card can render.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A free-text field.
    Text,
    /// Pick exactly one option (radio list).
    Single,
    /// Pick any number of options (checkbox list).
    Multi,
    /// A Yes/No — synthesized into a two-option list; the answer is a bool.
    Confirm,
}

/// One selectable option (a `single`/`multi` choice, or a synthesized `confirm`
/// Yes/No). `label` falls back to `value`; `description` may be empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Opt {
    pub value: String,
    pub label: String,
    pub description: String,
}

/// The outcome of feeding a key to the card: keep it open, or resolve it with the
/// answer payload to `send` back.
pub enum QuestionAction {
    /// The key mutated the card (or was ignored); the card stays open.
    Pending,
    /// Enter — carry the `{type:"answer",action:"accept",value:…}` payload.
    Accept(Value),
    /// Esc — carry the `{type:"answer",action:"cancel"}` payload (NO value, ever).
    Cancel(Value),
}

/// A live question awaiting the human's answer. Built from a `question` event; keys
/// drive it via [`QuestionCard::on_key`]; the answer is [`QuestionCard::accept_payload`]
/// / [`QuestionCard::cancel_payload`].
pub struct QuestionCard {
    pub request_id: String,
    /// The agent id to `send` the answer back to (the asking mind).
    pub source: String,
    /// Carried through from the event for parity / future multi-client routing; the
    /// fixed answer shape does not echo it, so it is parsed but not otherwise read.
    #[allow(dead_code)]
    pub client_id: String,
    pub kind: Kind,
    pub question: String,
    /// An optional short chip (`<=12` chars) shown in the card title.
    pub header: String,
    pub options: Vec<Opt>,
    pub cursor: usize,
    pub checked: Vec<bool>,
    pub text: String,
}

impl QuestionCard {
    /// Parse a `{type:"inquiry", …}` event into a card. `None` if it isn't a
    /// question, if `request_id` is missing/empty, if `source` is missing/empty (we
    /// couldn't answer without a target), or if a `single`/`multi` carries no
    /// options. `confirm` synthesizes a Yes/No list; `default` prefills the state.
    pub fn from_event(ev: &Value) -> Option<QuestionCard> {
        if ev.get("type").and_then(Value::as_str) != Some("question") {
            return None;
        }
        let request_id = non_empty(ev.get("request_id"))?;
        let source = non_empty(ev.get("source"))?;
        let client_id = ev
            .get("client_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let kind = match ev.get("kind").and_then(Value::as_str) {
            Some("single") => Kind::Single,
            Some("multi") => Kind::Multi,
            Some("confirm") => Kind::Confirm,
            _ => Kind::Text,
        };
        let question = ev
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let header = ev
            .get("header")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        let options: Vec<Opt> = match kind {
            // Yes is index 0 (→ true on accept); No is index 1 (→ false).
            Kind::Confirm => vec![
                Opt {
                    value: "true".to_string(),
                    label: "Yes".to_string(),
                    description: String::new(),
                },
                Opt {
                    value: "false".to_string(),
                    label: "No".to_string(),
                    description: String::new(),
                },
            ],
            // A choice with no options is unanswerable → let it fall through.
            Kind::Single | Kind::Multi => parse_options(ev)?,
            Kind::Text => Vec::new(),
        };

        let n = options.len();
        let mut card = QuestionCard {
            request_id,
            source,
            client_id,
            kind,
            question,
            header,
            options,
            cursor: 0,
            checked: vec![false; n],
            text: String::new(),
        };
        card.apply_default(ev.get("default"));
        Some(card)
    }

    /// Apply the optional `default`: single/confirm → move the cursor to the
    /// matching option; multi → pre-check the listed values; text → seed the buffer.
    fn apply_default(&mut self, default: Option<&Value>) {
        let Some(default) = default else { return };
        match self.kind {
            Kind::Text => {
                if let Some(s) = default.as_str() {
                    self.text = s.to_string();
                }
            }
            Kind::Single | Kind::Confirm => {
                // Confirm's default is a bool; normalize it to the "true"/"false"
                // option value so both kinds resolve by matching the option list.
                let want = match default {
                    Value::Bool(b) => b.to_string(),
                    Value::String(s) => s.clone(),
                    _ => return,
                };
                if let Some(i) = self.options.iter().position(|o| o.value == want) {
                    self.cursor = i;
                }
            }
            Kind::Multi => {
                if let Some(arr) = default.as_array() {
                    let wanted: Vec<&str> = arr.iter().filter_map(Value::as_str).collect();
                    for (i, o) in self.options.iter().enumerate() {
                        if wanted.contains(&o.value.as_str()) {
                            self.checked[i] = true;
                        }
                    }
                }
            }
        }
    }

    /// ↑ : previous option (single/multi/confirm; clamped).
    pub fn up(&mut self) {
        if matches!(self.kind, Kind::Single | Kind::Multi | Kind::Confirm) {
            self.cursor = self.cursor.saturating_sub(1);
        }
    }

    /// ↓ : next option (single/multi/confirm; clamped).
    pub fn down(&mut self) {
        if matches!(self.kind, Kind::Single | Kind::Multi | Kind::Confirm)
            && self.cursor + 1 < self.options.len()
        {
            self.cursor += 1;
        }
    }

    /// Space : flip the option under the cursor (multi only).
    pub fn toggle(&mut self) {
        if self.kind == Kind::Multi {
            if let Some(c) = self.checked.get_mut(self.cursor) {
                *c = !*c;
            }
        }
    }

    /// Type a character into the buffer (text only).
    pub fn type_char(&mut self, c: char) {
        if self.kind == Kind::Text {
            self.text.push(c);
        }
    }

    /// Backspace the buffer (text only).
    pub fn backspace(&mut self) {
        if self.kind == Kind::Text {
            self.text.pop();
        }
    }

    /// Drive the card from a key: mutate + stay [`QuestionAction::Pending`], or
    /// resolve on Enter/Esc. The whole modal contract lives here so `lib.rs`'s key
    /// shim is a thin match on the returned action.
    pub fn on_key(&mut self, code: KeyCode) -> QuestionAction {
        match code {
            KeyCode::Up => self.up(),
            KeyCode::Down => self.down(),
            // Space toggles a multi option; in text it types a literal space.
            KeyCode::Char(' ') if self.kind == Kind::Multi => self.toggle(),
            KeyCode::Char(c) => self.type_char(c),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Enter => return QuestionAction::Accept(self.accept_payload()),
            KeyCode::Esc => return QuestionAction::Cancel(self.cancel_payload()),
            _ => {}
        }
        QuestionAction::Pending
    }

    /// The `{type:"answer",action:"accept",value:…}` payload, with the value typed
    /// per kind (text→string · single→chosen value · multi→array · confirm→bool).
    pub fn accept_payload(&self) -> Value {
        json!({
            "type": "answer",
            "request_id": self.request_id,
            "action": "accept",
            "value": self.value(),
        })
    }

    /// The `{type:"answer",action:"cancel"}` payload — NO `value` key, ever (the
    /// "no fabricated answer" rule).
    pub fn cancel_payload(&self) -> Value {
        json!({
            "type": "answer",
            "request_id": self.request_id,
            "action": "cancel",
        })
    }

    /// The accepted value, typed by kind.
    fn value(&self) -> Value {
        match self.kind {
            Kind::Text => Value::String(self.text.clone()),
            Kind::Single => Value::String(
                self.options
                    .get(self.cursor)
                    .map(|o| o.value.clone())
                    .unwrap_or_default(),
            ),
            Kind::Multi => Value::Array(
                self.options
                    .iter()
                    .zip(&self.checked)
                    .filter(|(_, &on)| on)
                    .map(|(o, _)| Value::String(o.value.clone()))
                    .collect(),
            ),
            // Yes is index 0.
            Kind::Confirm => Value::Bool(self.cursor == 0),
        }
    }

    /// Render the card as a centered, opaque overlay OVER the chat (mirrors
    /// `render_help`: a centered `Rect` blacked out with `fill_black`, then the
    /// widget's glyphs on top). `within` is the inset chat area.
    pub fn render(&self, f: &mut Frame, within: Rect) {
        let mag = Color::Indexed(165);
        let dim = Style::default().fg(Color::DarkGray);
        let body = Style::default().fg(Color::White);
        let chip = Style::default()
            .fg(Color::Black)
            .bg(mag)
            .add_modifier(Modifier::BOLD);

        // The card wants 30..64 columns — but never MORE than the frame has (a 30-col
        // minimum in a 10-col terminal would index outside the buffer: the same lesson
        // the help overlay learned).
        let w = within
            .width
            .saturating_sub(4)
            .clamp(30, 64)
            .min(within.width);
        let inner = (w as usize).saturating_sub(3);

        let mut lines: Vec<Line> = vec![Line::from("")];
        // Title chip: the header (else a generic marker), rendered as a badge.
        let head = if self.header.is_empty() {
            "question".to_string()
        } else {
            clip(&self.header, 12)
        };
        lines.push(Line::from(Span::styled(format!(" {head} "), chip)));
        lines.push(Line::from(""));
        // The question prompt (pre-wrapped so the card can size itself).
        for row in wrap(&self.question, inner) {
            lines.push(Line::from(Span::styled(
                format!(" {row}"),
                body.add_modifier(Modifier::BOLD),
            )));
        }
        lines.push(Line::from(""));

        // Per-kind body.
        match self.kind {
            Kind::Single | Kind::Confirm => {
                for (i, o) in self.options.iter().enumerate() {
                    let sel = i == self.cursor;
                    let st = if sel {
                        Style::default().fg(mag).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Gray)
                    };
                    let mark = if sel { " ▸ " } else { "   " };
                    let mut spans = vec![Span::styled(mark, st), Span::styled(o.label.clone(), st)];
                    if !o.description.is_empty() {
                        spans.push(Span::styled(format!("  {}", o.description), dim));
                    }
                    lines.push(Line::from(spans));
                }
            }
            Kind::Multi => {
                for (i, o) in self.options.iter().enumerate() {
                    let sel = i == self.cursor;
                    let on = self.checked.get(i).copied().unwrap_or(false);
                    let st = if sel {
                        Style::default().fg(mag).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Gray)
                    };
                    let cur = if sel { " ▸ " } else { "   " };
                    let boxed = if on { "[x] " } else { "[ ] " };
                    let mut spans = vec![
                        Span::styled(cur, st),
                        Span::styled(boxed, st),
                        Span::styled(o.label.clone(), st),
                    ];
                    if !o.description.is_empty() {
                        spans.push(Span::styled(format!("  {}", o.description), dim));
                    }
                    lines.push(Line::from(spans));
                }
            }
            Kind::Text => {
                lines.push(Line::from(vec![
                    Span::styled(" ▸ ", dim),
                    Span::styled(self.text.clone(), body),
                    Span::styled("▏", Style::default().fg(mag)),
                ]));
            }
        }

        // Key-hint footer.
        lines.push(Line::from(""));
        let hint = match self.kind {
            Kind::Multi => "↑↓ · space · enter · esc",
            Kind::Text => "type · enter · esc",
            _ => "↑↓ · enter · esc",
        };
        lines.push(Line::from(Span::styled(format!(" {hint}"), dim)));

        let h = (lines.len() as u16 + 1).min(within.height);
        let area = Rect {
            x: within.x + (within.width.saturating_sub(w)) / 2,
            y: within.y + (within.height.saturating_sub(h)) / 2,
            width: w,
            height: h,
        };
        crate::fill_black(f.buffer_mut(), area);
        f.render_widget(Paragraph::new(lines), area);
    }
}

/// A present, non-empty string field, else `None`.
fn non_empty(v: Option<&Value>) -> Option<String> {
    let s = v.and_then(Value::as_str)?;
    (!s.is_empty()).then(|| s.to_string())
}

/// Parse the `options` array into [`Opt`]s (label falls back to value; description
/// optional). `None` if the array is absent or yields no options.
fn parse_options(ev: &Value) -> Option<Vec<Opt>> {
    let arr = ev.get("options").and_then(Value::as_array)?;
    let opts: Vec<Opt> = arr.iter().filter_map(parse_opt).collect();
    (!opts.is_empty()).then_some(opts)
}

/// One option object → [`Opt`]; `None` if it has no string `value`.
fn parse_opt(v: &Value) -> Option<Opt> {
    let value = v.get("value").and_then(Value::as_str)?.to_string();
    let label = v
        .get("label")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| value.clone());
    let description = v
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Some(Opt {
        value,
        label,
        description,
    })
}

/// Truncate `s` to at most `max` chars.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

/// Greedy word-wrap `text` to `width` columns (≥1 row). Whitespace-collapsing; a
/// long single word is placed on its own row (it may overflow — clipped at render).
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if line.is_empty() {
            line = word.to_string();
        } else if line.chars().count() + 1 + word.chars().count() <= width {
            line.push(' ');
            line.push_str(word);
        } else {
            out.push(std::mem::take(&mut line));
            line = word.to_string();
        }
    }
    if !line.is_empty() {
        out.push(line);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn q(v: Value) -> Option<QuestionCard> {
        QuestionCard::from_event(&v)
    }

    #[test]
    fn from_event_parses_single() {
        let card = q(json!({
            "type":"inquiry","request_id":"q_1","kind":"single",
            "question":"Which auth method?","header":"Auth",
            "options":[
                {"value":"oauth","label":"OAuth","description":"Browser consent"},
                {"value":"token"}
            ],
            "source":"mind_1","client_id":"c1"
        }))
        .expect("parses");
        assert_eq!(card.kind, Kind::Single);
        assert_eq!(card.source, "mind_1");
        assert_eq!(card.options.len(), 2);
        assert_eq!(card.options[0].label, "OAuth");
        assert_eq!(card.options[0].description, "Browser consent");
        assert_eq!(card.options[1].label, "token"); // label falls back to value
        assert_eq!(card.options[1].description, "");
        // Enter accepts the first option's value.
        let p = card.accept_payload();
        assert_eq!(p["type"], "answer");
        assert_eq!(p["action"], "accept");
        assert_eq!(p["request_id"], "q_1");
        assert_eq!(p["value"], "oauth");
    }

    #[test]
    fn from_event_parses_text() {
        let mut card = q(json!({
            "type":"inquiry","request_id":"q3","kind":"text",
            "question":"Name?","source":"m"
        }))
        .unwrap();
        assert_eq!(card.kind, Kind::Text);
        for c in "hi there".chars() {
            card.type_char(c);
        }
        card.backspace();
        assert_eq!(card.accept_payload()["value"], "hi ther");
    }

    #[test]
    fn from_event_parses_confirm_and_accepts_bool() {
        let mut card = q(json!({
            "type":"inquiry","request_id":"q4","kind":"confirm",
            "question":"Proceed?","source":"m"
        }))
        .unwrap();
        assert_eq!(card.kind, Kind::Confirm);
        assert_eq!(card.options.len(), 2);
        assert_eq!(card.options[0].label, "Yes");
        assert_eq!(card.options[1].label, "No");
        // cursor 0 = Yes = true.
        assert_eq!(card.accept_payload()["value"], json!(true));
        card.down(); // No
        assert_eq!(card.accept_payload()["value"], json!(false));
    }

    #[test]
    fn multi_toggle_yields_array_value() {
        let mut card = q(json!({
            "type":"inquiry","request_id":"q2","kind":"multi",
            "question":"Pick some","source":"m",
            "options":[{"value":"a"},{"value":"b"},{"value":"c"}]
        }))
        .unwrap();
        assert_eq!(card.kind, Kind::Multi);
        card.toggle(); // check a (cursor 0)
        card.down();
        card.down();
        card.toggle(); // check c (cursor 2)
        assert_eq!(card.accept_payload()["value"], json!(["a", "c"]));
    }

    #[test]
    fn none_without_source() {
        assert!(q(json!({
            "type":"inquiry","request_id":"q","kind":"text","question":"?"
        }))
        .is_none());
        // Empty source is unanswerable too.
        assert!(q(json!({
            "type":"inquiry","request_id":"q","kind":"text","question":"?","source":""
        }))
        .is_none());
    }

    #[test]
    fn none_when_choice_options_empty() {
        assert!(q(json!({
            "type":"inquiry","request_id":"q","kind":"single",
            "question":"?","source":"m","options":[]
        }))
        .is_none());
        // Options key missing on a choice kind → None.
        assert!(q(json!({
            "type":"inquiry","request_id":"q","kind":"multi","question":"?","source":"m"
        }))
        .is_none());
    }

    #[test]
    fn none_on_wrong_type_or_missing_request_id() {
        assert!(q(json!({"type":"answer","request_id":"q","source":"m"})).is_none());
        assert!(q(json!({
            "type":"inquiry","kind":"text","question":"?","source":"m"
        }))
        .is_none());
    }

    #[test]
    fn cancel_never_carries_value() {
        let card = q(json!({
            "type":"inquiry","request_id":"q5","kind":"text","question":"?","source":"m"
        }))
        .unwrap();
        let c = card.cancel_payload();
        assert_eq!(c["type"], "answer");
        assert_eq!(c["action"], "cancel");
        assert_eq!(c["request_id"], "q5");
        assert!(
            c.get("value").is_none(),
            "cancel must never fabricate a value"
        );
    }

    #[test]
    fn default_prefills_single_and_multi() {
        // single: the cursor lands on the option whose value matches `default`.
        let s = q(json!({
            "type":"inquiry","request_id":"q","kind":"single","question":"?","source":"m",
            "options":[{"value":"a"},{"value":"b"},{"value":"c"}],"default":"b"
        }))
        .unwrap();
        assert_eq!(s.accept_payload()["value"], "b");
        // multi: the listed values are pre-checked (in option order).
        let m = q(json!({
            "type":"inquiry","request_id":"q","kind":"multi","question":"?","source":"m",
            "options":[{"value":"a"},{"value":"b"},{"value":"c"}],"default":["a","c"]
        }))
        .unwrap();
        assert_eq!(m.accept_payload()["value"], json!(["a", "c"]));
    }

    #[test]
    fn default_prefills_text_and_confirm() {
        let t = q(json!({
            "type":"inquiry","request_id":"q","kind":"text","question":"?","source":"m",
            "default":"seed"
        }))
        .unwrap();
        assert_eq!(t.accept_payload()["value"], "seed");
        // confirm default false → No (index 1) → false.
        let c = q(json!({
            "type":"inquiry","request_id":"q","kind":"confirm","question":"?","source":"m",
            "default":false
        }))
        .unwrap();
        assert_eq!(c.accept_payload()["value"], json!(false));
    }

    #[test]
    fn on_key_navigates_and_accepts() {
        let mut card = q(json!({
            "type":"inquiry","request_id":"q","kind":"single","question":"?","source":"m",
            "options":[{"value":"a"},{"value":"b"}]
        }))
        .unwrap();
        assert!(matches!(
            card.on_key(KeyCode::Down),
            QuestionAction::Pending
        ));
        match card.on_key(KeyCode::Enter) {
            QuestionAction::Accept(v) => assert_eq!(v["value"], "b"),
            _ => panic!("expected Accept"),
        }
    }

    #[test]
    fn on_key_esc_cancels_without_value() {
        let mut card = q(json!({
            "type":"inquiry","request_id":"q","kind":"text","question":"?","source":"m"
        }))
        .unwrap();
        match card.on_key(KeyCode::Esc) {
            QuestionAction::Cancel(v) => {
                assert_eq!(v["action"], "cancel");
                assert!(v.get("value").is_none());
            }
            _ => panic!("expected Cancel"),
        }
    }

    #[test]
    fn on_key_space_toggles_multi_but_types_in_text() {
        let mut multi = q(json!({
            "type":"inquiry","request_id":"q","kind":"multi","question":"?","source":"m",
            "options":[{"value":"a"}]
        }))
        .unwrap();
        multi.on_key(KeyCode::Char(' ')); // toggles option a
        assert_eq!(multi.accept_payload()["value"], json!(["a"]));

        let mut text = q(json!({
            "type":"inquiry","request_id":"q","kind":"text","question":"?","source":"m"
        }))
        .unwrap();
        text.on_key(KeyCode::Char(' ')); // a literal space in text
        text.on_key(KeyCode::Char('x'));
        assert_eq!(text.accept_payload()["value"], " x");
    }
}
