//! THE INPUT LINE, and the two palettes that hang off it.
//!
//! One honest thing this must never hide: a second message sent while a turn holds
//! the backend's lock is QUEUED, not an interrupt. It is linked to the running
//! turn and it does not steer it. The composer says so in the hint, because a
//! human who believes they interrupted a model and did not has been lied to by
//! their tool.
//!
//! `@` opens the agent palette, `/` the verb palette. Verbs come from the selected
//! agent's own reflect, so the palette is correct for an agent written after this
//! binary was compiled.

use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Plain text to the selected agent.
    Message,
    /// A verb call on the selected agent: `type` plus arguments.
    Verb,
    /// No act path exists (the human's own agent is not installed), so the
    /// composer refuses input and says why rather than dropping keystrokes.
    Locked,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Palette {
    None,
    /// Choosing an agent to address.
    Agents {
        query: String,
        sel: usize,
    },
    /// Choosing a verb on the selected agent.
    Verbs {
        query: String,
        sel: usize,
    },
}

#[derive(Debug)]
pub struct Composer {
    pub mode: Mode,
    pub text: String,
    /// A BYTE index into `text`, always on a char boundary.
    pub caret: usize,
    pub history: Vec<String>,
    /// How far back through history the human has walked; zero is the live line.
    pub recall: usize,
    pub palette: Palette,
    /// The row this message will answer, if the human picked one. Answering is an
    /// ordinary send with `re`; there is no answer verb, because nothing was
    /// parked.
    pub re: Option<crate::kern::model::RowId>,
}

impl Default for Composer {
    fn default() -> Self {
        Self {
            mode: Mode::Message,
            text: String::new(),
            caret: 0,
            history: Vec::new(),
            recall: 0,
            palette: Palette::None,
            re: None,
        }
    }
}

impl Composer {
    pub fn insert(&mut self, c: char) {
        self.text.insert(self.caret, c);
        self.caret += c.len_utf8();
        // A PALETTE IS A COMPLETION FOR THE TOKEN BEING TYPED, so typing narrows
        // it and a space ENDS it. Without that the palette stayed open over a
        // finished line and swallowed the Enter that meant "send" — measured
        // 2026-09-13: `/start command=pwd` became `/reflect` on Enter.
        if self.palette != Palette::None {
            if c.is_whitespace() {
                self.palette = Palette::None;
            } else {
                self.query_push(c);
            }
            return;
        }
        self.open_palette_if_triggered(c);
    }

    fn query_push(&mut self, c: char) {
        match &mut self.palette {
            Palette::Agents { query, sel } | Palette::Verbs { query, sel } => {
                query.push(c);
                // The list just changed under the cursor, so start from the top
                // rather than pointing at whatever happens to be at the old index.
                *sel = 0;
            }
            Palette::None => {}
        }
    }

    fn query_pop(&mut self) {
        let empty = match &mut self.palette {
            Palette::Agents { query, sel } | Palette::Verbs { query, sel } => {
                query.pop();
                *sel = 0;
                query.is_empty()
            }
            Palette::None => return,
        };
        // Backspacing past the trigger closes it: the token it was completing is
        // gone.
        if empty && !self.text[..self.caret].ends_with(['@', '/']) {
            self.palette = Palette::None;
        }
    }

    /// Move the palette cursor. `len` is how many options are currently listed,
    /// which only the caller knows.
    pub fn palette_move(&mut self, d: isize, len: usize) {
        if len == 0 {
            return;
        }
        if let Palette::Agents { sel, .. } | Palette::Verbs { sel, .. } = &mut self.palette {
            let next = (*sel as isize + d).rem_euclid(len as isize) as usize;
            *sel = next;
        }
    }

    pub fn insert_str(&mut self, s: &str) {
        // A paste is text, never a trigger: pasting a line containing `@` must not
        // open a palette in the middle of the paste.
        self.text.insert_str(self.caret, s);
        self.caret += s.len();
    }

    fn open_palette_if_triggered(&mut self, c: char) {
        // Only at the start of a word, so an email address or a path does not
        // spring a palette.
        let at_word_start = self.text[..self.caret - c.len_utf8()]
            .chars()
            .next_back()
            .map(|p| p.is_whitespace())
            .unwrap_or(true);
        if !at_word_start {
            return;
        }
        match c {
            '@' => {
                self.palette = Palette::Agents {
                    query: String::new(),
                    sel: 0,
                }
            }
            '/' => {
                self.palette = Palette::Verbs {
                    query: String::new(),
                    sel: 0,
                }
            }
            _ => {}
        }
    }

    pub fn backspace(&mut self) {
        if self.caret == 0 {
            return;
        }
        if self.palette != Palette::None {
            self.query_pop();
        }
        let prev = self.text[..self.caret]
            .chars()
            .next_back()
            .map(char::len_utf8)
            .unwrap_or(1);
        self.caret -= prev;
        self.text.remove(self.caret);
    }

    pub fn delete(&mut self) {
        if self.caret < self.text.len() {
            self.text.remove(self.caret);
        }
    }

    pub fn left(&mut self) {
        if let Some(c) = self.text[..self.caret].chars().next_back() {
            self.caret -= c.len_utf8();
        }
    }

    pub fn right(&mut self) {
        if let Some(c) = self.text[self.caret..].chars().next() {
            self.caret += c.len_utf8();
        }
    }

    pub fn home(&mut self) {
        self.caret = 0;
    }

    pub fn end(&mut self) {
        self.caret = self.text.len();
    }

    pub fn kill_line(&mut self) {
        self.text.clear();
        self.caret = 0;
    }

    /// Ctrl-W: back over one word, whitespace-delimited.
    pub fn kill_word(&mut self) {
        let head = &self.text[..self.caret];
        let trimmed = head.trim_end();
        let cut = trimmed
            .rfind(char::is_whitespace)
            .map(|i| i + 1)
            .unwrap_or(0);
        self.text.replace_range(cut..self.caret, "");
        self.caret = cut;
    }

    /// Take the line and file it in history. An empty line is not history.
    pub fn take(&mut self) -> Option<String> {
        let t = self.text.trim().to_string();
        if t.is_empty() {
            return None;
        }
        if self.history.last().map(String::as_str) != Some(t.as_str()) {
            self.history.push(t.clone());
        }
        self.text.clear();
        self.caret = 0;
        self.recall = 0;
        self.palette = Palette::None;
        Some(t)
    }

    pub fn recall_back(&mut self) {
        if self.recall < self.history.len() {
            self.recall += 1;
            self.load_recall();
        }
    }

    pub fn recall_forward(&mut self) {
        if self.recall > 0 {
            self.recall -= 1;
            self.load_recall();
        }
    }

    fn load_recall(&mut self) {
        self.text = if self.recall == 0 {
            String::new()
        } else {
            self.history[self.history.len() - self.recall].clone()
        };
        self.caret = self.text.len();
    }

    /// A `/q ` prefix is an EXPLICIT queue: the human is saying "behind whatever is
    /// running", so the hint does not need to warn them.
    pub fn explicit_queue(&self) -> bool {
        self.text.starts_with("/q ")
    }

    /// The text actually sent, with an explicit queue prefix removed.
    pub fn payload_text(text: &str) -> &str {
        text.strip_prefix("/q ").unwrap_or(text).trim()
    }

    /// A verb-mode line parsed into a payload. `read path=a.md limit=200` becomes
    /// `{"type":"read","path":"a.md","limit":200}`.
    ///
    /// Values are typed: a number is a number, `true`/`false` are booleans,
    /// `[a,b,c]` is a LIST, `null` is null, and anything else is a string (quotes
    /// stripped). The list form is not a nicety — `exec.start` takes
    /// `args:list[str]`, and without it `args=hello` went out as a string and was
    /// refused, which looked like the shell doing nothing at all.
    ///
    /// A line with no verb is refused rather than sent as an empty call.
    pub fn parse_verb(line: &str) -> Result<serde_json::Value, String> {
        let line = line.strip_prefix('/').unwrap_or(line).trim();
        let mut it = line.split_whitespace();
        let verb = it.next().ok_or_else(|| "name a verb".to_string())?;
        let mut m = serde_json::Map::new();
        m.insert("type".into(), serde_json::Value::String(verb.to_string()));
        for tok in it {
            let Some((k, v)) = tok.split_once('=') else {
                return Err(format!("{tok:?} is not key=value"));
            };
            m.insert(k.to_string(), scalar(v));
        }
        Ok(serde_json::Value::Object(m))
    }

    /// `/export <path>` (or `/export path=<path>`) for the SELECTED agent, as the
    /// `export_subtree` call that writes it and everything under it as a blueprint
    /// folder at `path` (relative to the running dir; no `..`).
    pub fn parse_export(line: &str, selected: Option<&str>) -> Result<serde_json::Value, String> {
        let agent = selected
            .filter(|a| !a.is_empty() && !a.starts_with('#'))
            .ok_or_else(|| "select an agent first.".to_string())?;
        let rest = line.trim().strip_prefix('/').unwrap_or(line.trim());
        let rest = rest.strip_prefix("export").unwrap_or(rest).trim();
        let path = rest.strip_prefix("path=").unwrap_or(rest).trim().trim_matches('"');
        if path.is_empty() {
            return Err("usage: /export <path>".into());
        }
        if std::path::Path::new(path).components().any(|c| c == std::path::Component::ParentDir) {
            return Err("path may not hold `..`".into());
        }
        Ok(serde_json::json!({"type": "export_subtree", "agent_id": agent, "path": path}))
    }

    /// One value, typed. Shared by the arg parser so a list's elements are read
    /// the same way a bare value is.
    fn one(v: &str) -> serde_json::Value {
        let v = v.trim();
        if v == "null" {
            return serde_json::Value::Null;
        }
        if v == "true" || v == "false" {
            return serde_json::Value::from(v == "true");
        }
        if let Ok(n) = v.parse::<i64>() {
            return serde_json::Value::from(n);
        }
        if let Ok(f) = v.parse::<f64>() {
            return serde_json::Value::from(f);
        }
        serde_json::Value::String(v.trim_matches('"').to_string())
    }

    /// The caret's column, in terminal cells.
    pub fn caret_col(&self) -> u16 {
        self.text[..self.caret].width() as u16
    }
}

/// A value as typed on a verb line. `[a,b]` is a list; `[]` is an empty list.
fn scalar(v: &str) -> serde_json::Value {
    let v = v.trim();
    if let Some(inner) = v.strip_prefix('[').and_then(|x| x.strip_suffix(']')) {
        let inner = inner.trim();
        if inner.is_empty() {
            return serde_json::Value::Array(Vec::new());
        }
        return serde_json::Value::Array(inner.split(',').map(Composer::one).collect());
    }
    Composer::one(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn editing_keeps_the_caret_on_char_boundaries() {
        let mut c = Composer::default();
        for ch in "héllo".chars() {
            c.insert(ch);
        }
        assert_eq!(c.text, "héllo");
        c.left();
        c.left();
        c.left();
        c.left();
        assert_eq!(c.caret, 1, "one byte in, before the two-byte é");
        c.backspace();
        assert_eq!(c.text, "éllo");
        c.delete();
        assert_eq!(c.text, "llo");
    }

    #[test]
    fn kill_word_stops_at_the_previous_space() {
        let mut c = Composer::default();
        c.insert_str("use auth-v2.ts now");
        c.end();
        c.kill_word();
        assert_eq!(c.text, "use auth-v2.ts ");
        c.kill_word();
        assert_eq!(c.text, "use ");
        c.kill_word();
        assert_eq!(c.text, "");
    }

    #[test]
    fn a_palette_opens_only_at_a_word_start() {
        let mut c = Composer::default();
        c.insert('@');
        assert!(matches!(c.palette, Palette::Agents { .. }));

        let mut c = Composer::default();
        c.insert_str("mail");
        c.end();
        c.insert('@');
        assert_eq!(
            c.palette,
            Palette::None,
            "an address is not a palette trigger"
        );
    }

    #[test]
    fn a_paste_never_springs_a_palette() {
        let mut c = Composer::default();
        c.insert_str("@ai /read");
        assert_eq!(c.palette, Palette::None);
        assert_eq!(c.text, "@ai /read");
    }

    #[test]
    fn history_skips_blanks_and_duplicates_and_walks_both_ways() {
        let mut c = Composer::default();
        c.insert_str("   ");
        assert!(c.take().is_none());
        assert!(c.history.is_empty());

        c.insert_str("first");
        c.take();
        c.insert_str("first");
        c.take();
        assert_eq!(c.history, vec!["first"], "a repeat is not new history");

        c.insert_str("second");
        c.take();
        c.recall_back();
        assert_eq!(c.text, "second");
        c.recall_back();
        assert_eq!(c.text, "first");
        c.recall_back();
        assert_eq!(c.text, "first", "there is nothing older");
        c.recall_forward();
        assert_eq!(c.text, "second");
        c.recall_forward();
        assert_eq!(c.text, "", "forward past the newest is the live line");
    }

    #[test]
    fn an_explicit_queue_prefix_is_stripped_from_what_is_sent() {
        let mut c = Composer::default();
        c.insert_str("/q run the tests");
        assert!(c.explicit_queue());
        assert_eq!(Composer::payload_text(&c.text), "run the tests");
        assert_eq!(Composer::payload_text("plain"), "plain");
    }

    #[test]
    fn a_verb_line_becomes_a_typed_payload() {
        let v = Composer::parse_verb("/read path=a.md limit=200 tail=true").unwrap();
        assert_eq!(
            v,
            json!({"type":"read","path":"a.md","limit":200,"tail":true})
        );
        assert_eq!(Composer::parse_verb("ps").unwrap(), json!({"type":"ps"}));
    }

    #[test]
    fn a_malformed_verb_line_is_refused_rather_than_sent() {
        assert!(Composer::parse_verb("").is_err());
        let e = Composer::parse_verb("read pathA").unwrap_err();
        assert!(e.contains("key=value"), "{e}");
    }

    #[test]
    fn a_value_can_be_a_list_because_start_needs_one() {
        // `exec.start` takes args:list[str]. Without this the string form was
        // refused and the pane showed nothing.
        let v = Composer::parse_verb("/start command=echo args=[hello,world]").unwrap();
        assert_eq!(v, json!({"type":"start","command":"echo","args":["hello","world"]}));
        assert_eq!(Composer::parse_verb("/start args=[]").unwrap()["args"], json!([]));
        assert_eq!(Composer::parse_verb("/x a=[1,2]").unwrap()["a"], json!([1, 2]));
    }

    #[test]
    fn null_and_booleans_are_typed_not_stringified() {
        let v = Composer::parse_verb("/x a=null b=true c=false d=1.5").unwrap();
        assert!(v["a"].is_null());
        assert_eq!(v["b"], json!(true));
        assert_eq!(v["c"], json!(false));
        assert_eq!(v["d"], json!(1.5));
    }

    #[test]
    fn a_quoted_value_keeps_its_spacelessness_without_the_quotes() {
        let v = Composer::parse_verb(r#"send text="hello""#).unwrap();
        assert_eq!(v["text"], "hello");
    }

    #[test]
    fn the_caret_column_counts_cells_not_bytes() {
        let mut c = Composer::default();
        c.insert_str("héllo");
        c.end();
        assert_eq!(c.caret_col(), 5, "é is one cell but two bytes");
    }

    #[test]
    fn parse_export_builds_export_subtree_payload_for_the_selected_agent() {
        let p = Composer::parse_export("/export out/team", Some("a1")).unwrap();
        assert_eq!(p, json!({"type": "export_subtree", "agent_id": "a1", "path": "out/team"}));
    }

    #[test]
    fn parse_export_accepts_path_key_form() {
        let p = Composer::parse_export("/export path=out/team", Some("a1")).unwrap();
        assert_eq!(p["path"], "out/team");
        assert_eq!(p["agent_id"], "a1");
    }

    #[test]
    fn parse_export_without_path_shows_usage_error() {
        for l in ["/export", "/export   ", "/export path="] {
            assert_eq!(Composer::parse_export(l, Some("a1")).unwrap_err(), "usage: /export <path>", "{l:?}");
        }
    }

    #[test]
    fn parse_export_refuses_dotdot_path() {
        for l in ["/export ../x", "/export a/../../x", "/export path=../x"] {
            assert_eq!(Composer::parse_export(l, Some("a1")).unwrap_err(), "path may not hold `..`", "{l:?}");
        }
    }

    #[test]
    fn parse_export_without_selection_says_select_an_agent() {
        assert_eq!(Composer::parse_export("/export out", None).unwrap_err(), "select an agent first.");
        assert_eq!(Composer::parse_export("/export out", Some("#bundles")).unwrap_err(), "select an agent first.");
    }
}
