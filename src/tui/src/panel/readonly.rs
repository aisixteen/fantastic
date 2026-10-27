//! Shared parts of the READ-ONLY panels (resources, tasks, store, scheduler,
//! cabinet, credentials): the line builders, the scroll keys and the refusal
//! reader. A panel built on these sends reads only and shows every `{error}`.

use crossterm::event::{KeyCode as K, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;
use serde_json::Value;

use crate::theme;

/// A section heading.
pub fn head(t: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("  ── {t} "),
        Style::default()
            .fg(theme::LABEL)
            .add_modifier(Modifier::BOLD),
    ))
}

/// A plain line.
pub fn plain(s: String) -> Line<'static> {
    Line::from(Span::styled(s, Style::default().fg(theme::FG)))
}

/// A quiet line.
pub fn dim(s: String) -> Line<'static> {
    Line::from(Span::styled(s, Style::default().fg(theme::DIM)))
}

/// A refusal line: the kernel's own words, never swallowed.
pub fn bad(s: String) -> Line<'static> {
    Line::from(Span::styled(s, Style::default().fg(theme::FAILED)))
}

/// A row that may carry the cursor.
pub fn sel(s: String, on: bool) -> Line<'static> {
    if on {
        Line::from(Span::styled(
            s,
            Style::default()
                .fg(theme::SELECT)
                .add_modifier(Modifier::BOLD),
        ))
    } else {
        plain(s)
    }
}

/// The error text of a refusal reply, or None.
pub fn refusal(reply: &Value) -> Option<String> {
    let e = reply.get("error")?;
    let mut s = e
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| e.to_string());
    if let Some(m) = reply.get("message").and_then(Value::as_str) {
        s = format!("{s}: {m}");
    }
    Some(s)
}

/// A string field, or empty.
pub fn st(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

/// j/k/arrows, PageUp/PageDown and `g` over `n` rows. True when consumed.
pub fn nav(k: KeyEvent, cursor: &mut usize, scroll: &mut u16, n: usize) -> bool {
    match k.code {
        K::Down | K::Char('j') => {
            if n > 0 {
                *cursor = (*cursor + 1).min(n - 1);
            }
            true
        }
        K::Up | K::Char('k') => {
            *cursor = cursor.saturating_sub(1);
            true
        }
        K::PageDown => {
            *scroll = scroll.saturating_add(10);
            true
        }
        K::PageUp => {
            *scroll = scroll.saturating_sub(10);
            true
        }
        K::Char('g') => {
            *scroll = 0;
            *cursor = 0;
            true
        }
        _ => false,
    }
}

pub fn draw(f: &mut Frame<'_>, area: Rect, lines: Vec<Line<'static>>, scroll: u16) {
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
}

pub fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// One line of any JSON value, cut to `n` characters.
pub fn brief(v: &Value, n: usize) -> String {
    let s = match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let s = s.replace('\n', " ");
    if s.chars().count() > n {
        let mut t: String = s.chars().take(n).collect();
        t.push('…');
        t
    } else {
        s
    }
}

#[cfg(test)]
pub mod testing {
    use super::*;
    use crate::kern::model::AgentId;
    use crate::panel::Cx;

    pub fn text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Run `f` with a `Cx` built the way generic.rs builds one.
    pub fn with_cx<R>(agent: &str, reflect: Option<&Value>, f: impl FnOnce(&Cx<'_>) -> R) -> R {
        let store = crate::store::Store::default();
        let seen = std::collections::HashSet::new();
        let a: AgentId = agent.into();
        let cx = Cx {
            agent: &a,
            store: &store,
            tasks: &[],
            seen: &seen,
            reflect,
        };
        f(&cx)
    }

    pub fn key(c: crossterm::event::KeyCode) -> KeyEvent {
        KeyEvent::new(c, crossterm::event::KeyModifiers::NONE)
    }

    /// The verbs of every `Cmd::Send` in `out`.
    pub fn sent(out: &[crate::ev::Cmd]) -> Vec<String> {
        out.iter()
            .filter_map(|c| match c {
                crate::ev::Cmd::Send { payload, .. } => payload
                    .get("type")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                _ => None,
            })
            .collect()
    }
}
