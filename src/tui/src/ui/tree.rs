//! THE LEFT PANE: the whole kernel, as a tree.
//!
//! Every agent, not only the ones that talk. A transport leg, a loader, a file
//! surface and a model all appear, because the tree is the kernel's actual shape
//! and hiding the plumbing would leave a human unable to see why a call was
//! refused. Selecting any of them changes the right pane to THAT agent's UI.
//!
//! Presence is derived and never stored: blocked outranks working, because a
//! question addressed to the human is more urgent than work in flight, and a
//! human scanning this pane should see what needs them first.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::kern::model::{AgentId, TreeNode};
use crate::panel::PanelKind;
use crate::presence::Presence;
use crate::theme;
use crate::ui::side::SideView;

/// One rendered line of the tree.
#[derive(Debug, Clone, PartialEq)]
pub struct TreeRow {
    pub id: AgentId,
    pub depth: u8,
    pub label: String,
    pub kind: PanelKind,
    /// The node carries a handler, so there is something to dispatch to.
    pub live: bool,
    pub has_children: bool,
    pub collapsed: bool,
    pub presence: Presence,
    /// Open questions this agent has addressed to the human.
    pub inquiries: usize,
}

/// What the tree needs in order to be flattened. Everything is borrowed; the pane
/// owns no copy of the kernel.
pub struct Src<'a> {
    pub tree: Option<&'a TreeNode>,
    pub collapsed: &'a std::collections::HashSet<AgentId>,
    pub filter: Option<&'a str>,
    pub tasks: &'a [crate::kern::model::Task],
    pub waiting: &'a std::collections::HashMap<AgentId, usize>,
    pub done: &'a std::collections::HashSet<AgentId>,
    /// Agents whose record declares a valid `view.tui`.
    pub declared: &'a std::collections::HashSet<AgentId>,
}

/// Flatten the tree into the lines to draw.
///
/// A BARE GROUPING NODE IS NOT A ROW. A node with no handler (other than the
/// root) dispatches nothing and has no UI to open, so its label is noise: it
/// is not drawn, and its subtree rises one level. The grouping still exists in
/// the kernel; the sidebar just doesn't spend a row saying so.
///
/// A FILTER FLATTENS RATHER THAN PRUNES. Keeping ancestors of a match would show
/// rows the human did not search for, and hiding them would orphan the matches;
/// showing matches at depth zero is the one option that is neither.
pub fn flatten(src: &Src<'_>) -> Vec<TreeRow> {
    let Some(root) = src.tree else {
        return Vec::new();
    };
    let mut flat = Vec::new();
    root.walk(0, &mut flat);

    let needle = src.filter.map(|f| f.to_lowercase());
    let mut out = Vec::new();
    // The depth of the shallowest collapsed ancestor currently hiding rows.
    let mut hide_below: Option<u8> = None;
    // Depths of the bare grouping nodes above the current node, which lift it.
    let mut bare_above: Vec<u8> = Vec::new();

    for (node, depth) in flat {
        if let Some(d) = hide_below {
            if depth > d {
                continue;
            }
            hide_below = None;
        }
        while bare_above.last().is_some_and(|&d| d >= depth) {
            bare_above.pop();
        }
        if node.handler_module.is_none() && depth > 0 {
            bare_above.push(depth);
            continue;
        }
        let depth = depth - bare_above.len() as u8;
        let label = node
            .display_name
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| node.id.clone());

        if let Some(n) = &needle {
            if !node.id.to_lowercase().contains(n) && !label.to_lowercase().contains(n) {
                continue;
            }
        }

        let collapsed = src.collapsed.contains(&node.id);
        if collapsed && needle.is_none() {
            hide_below = Some(depth);
        }
        let inquiries = src.waiting.get(&node.id).copied().unwrap_or(0);
        out.push(TreeRow {
            id: node.id.clone(),
            // A filtered list is flat: there is no parent context to indent under.
            depth: if needle.is_some() { 0 } else { depth },
            label,
            kind: PanelKind::choose(
                &node.id,
                node.handler_module.as_deref().unwrap_or(""),
                src.declared.contains(&node.id),
            ),
            live: node.handler_module.is_some(),
            has_children: !node.children.is_empty(),
            collapsed,
            presence: crate::presence::of(
                &node.id,
                src.tasks,
                inquiries,
                src.done.contains(&node.id),
            ),
            inquiries,
        });
    }
    out
}

fn colour(p: Presence) -> ratatui::style::Color {
    match p {
        Presence::Working => theme::WORKING,
        Presence::Blocked => theme::BLOCKED,
        Presence::Done => theme::DONE,
        Presence::Idle => theme::IDLE,
    }
}

/// Which row a click at screen row `y` lands on — the same scroll `render`
/// uses, so a click hits exactly the line it is on.
pub fn row_at(n: usize, sel: usize, area: Rect, filter: bool, view: SideView, folded: usize, y: u16) -> Option<usize> {
    if y < area.y || y >= area.y + area.height {
        return None;
    }
    if view == SideView::Rail {
        return crate::ui::side::rail_row_at(n, sel, area, y);
    }
    let head = usize::from(filter);
    let footer = 1 + usize::from(view == SideView::Flat && folded > 0);
    let h = (area.height as usize).saturating_sub(head + footer);
    let top = if sel >= h && h > 0 { sel + 1 - h } else { 0 };
    let line = (y - area.y) as usize;
    let i = line.checked_sub(head).filter(|l| *l < h)? + top;
    (i < n).then_some(i)
}

pub fn render(
    f: &mut Frame<'_>,
    area: Rect,
    rows: &[TreeRow],
    sel: usize,
    focused: bool,
    filter: Option<&str>,
    view: SideView,
    folded: usize,
) {
    if view == SideView::Rail {
        crate::ui::side::render_rail(f.buffer_mut(), area, rows, sel);
        return;
    }
    let mut lines: Vec<Line> = Vec::new();

    if let Some(q) = filter {
        lines.push(Line::from(vec![
            Span::styled("  /", Style::default().fg(theme::PROMPT)),
            Span::styled(
                q.to_string(),
                Style::default().fg(theme::FG).add_modifier(Modifier::BOLD),
            ),
            // Says how to get the whole tree back. A filter that hid everything
            // and offered no way out read as a broken pane.
            Span::styled("  Esc clears", Style::default().fg(theme::DIM)),
        ]));
    }

    // The last row is the view hint; FLAT also keeps one for `+ N system`.
    let footer = 1 + usize::from(view == SideView::Flat && folded > 0);
    let h = (area.height as usize).saturating_sub(lines.len() + footer);
    // Keep the selection on screen.
    let top = if sel >= h && h > 0 { sel + 1 - h } else { 0 };

    for (i, r) in rows.iter().enumerate().skip(top).take(h) {
        let selected = i == sel;
        let mut style = Style::default().fg(colour(r.presence));
        if selected {
            style = style.add_modifier(Modifier::REVERSED);
            if focused {
                style = style.add_modifier(Modifier::BOLD);
            }
        }
        // The expand marker only appears where there is something to expand, so a
        // leaf never looks like it is hiding work.
        let arrow = match (r.has_children, r.collapsed) {
            (true, true) => theme::FOLDED,
            (true, false) => theme::OPENED,
            _ => ' ',
        };
        let node = if r.kind == PanelKind::Activity {
            theme::INQUIRY
        } else if r.kind == PanelKind::Setup {
            '+'
        } else if view == SideView::Flat && r.depth > 0 {
            '#'
        } else if r.live {
            theme::NODE_LIVE
        } else {
            theme::NODE_BARE
        };
        let mut badge = String::new();
        if r.presence == Presence::Working {
            badge.push(theme::RUNNING);
        }
        if r.inquiries > 0 {
            badge.push_str(&format!("{}{}", theme::INQUIRY, r.inquiries));
        }
        let text = format!(
            "{}{arrow} {node} {}{}",
            "  ".repeat(r.depth as usize),
            r.label,
            if badge.is_empty() {
                String::new()
            } else {
                format!("  {badge}")
            }
        );
        lines.push(Line::from(Span::styled(
            crate::ui::chrome::clip_str(&text, area.width as usize),
            style,
        )));
    }

    if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            if filter.is_some() {
                "  no agent matches that filter."
            } else {
                "  reading the tree…"
            },
            Style::default().fg(theme::DIM),
        )));
    }
    if view == SideView::Flat && folded > 0 {
        lines.push(Line::from(Span::styled(
            format!("    + {folded} system"),
            Style::default().fg(theme::DIM),
        )));
    }
    f.render_widget(Paragraph::new(lines), area);
    let hint = Rect { y: area.y + area.height.saturating_sub(1), height: 1, ..area };
    f.render_widget(
        Paragraph::new(Span::styled(
            if view == SideView::Flat { " v tree · < collapse" } else { " v flat · < collapse" },
            Style::default().fg(theme::DIM),
        )),
        hint,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_click_lands_on_the_row_drawn_there() {
        let area = Rect::new(1, 1, 30, 10);
        // no filter: line 0 is row 0; the last line is the view hint
        assert_eq!(row_at(5, 0, area, false, SideView::Tree, 0, 1), Some(0));
        assert_eq!(row_at(5, 0, area, false, SideView::Tree, 0, 3), Some(2));
        assert_eq!(row_at(5, 0, area, false, SideView::Tree, 0, 7), None, "past the last row");
        // a filter line pushes rows down by one
        assert_eq!(row_at(5, 0, area, true, SideView::Tree, 0, 1), None);
        assert_eq!(row_at(5, 0, area, true, SideView::Tree, 0, 2), Some(0));
        // scrolled: selection 20 of 30 with 9 visible lines
        assert_eq!(row_at(30, 20, area, false, SideView::Tree, 0, 1), Some(12));
    }
    use std::collections::{HashMap, HashSet};

    fn node(id: &str, handler: Option<&str>, children: Vec<TreeNode>) -> TreeNode {
        TreeNode {
            id: id.into(),
            parent_id: None,
            handler_module: handler.map(str::to_string),
            display_name: None,
            description: None,
            children,
        }
    }

    fn kernel() -> TreeNode {
        node(
            "kernel_state",
            Some("kernel_state.tools"),
            vec![
                node("store", Some("file_bridge.tools"), vec![]),
                node(
                    "roster",
                    Some("static_loader.tools"),
                    vec![
                        node("ai", Some("vllm_backend.tools"), vec![]),
                    ],
                ),
                node(
                    "web",
                    Some("web.tools"),
                    vec![node("rest", Some("web_rest.tools"), vec![])],
                ),
            ],
        )
    }

    struct Bag {
        collapsed: HashSet<AgentId>,
        waiting: HashMap<AgentId, usize>,
        done: HashSet<AgentId>,
        declared: HashSet<AgentId>,
    }
    impl Bag {
        fn new() -> Self {
            Self {
                collapsed: HashSet::new(),
                waiting: HashMap::new(),
                done: HashSet::new(),
                declared: HashSet::new(),
            }
        }
    }

    fn flat(
        t: &TreeNode,
        b: &Bag,
        filter: Option<&str>,
        tasks: &[crate::kern::model::Task],
    ) -> Vec<TreeRow> {
        flatten(&Src {
            tree: Some(t),
            collapsed: &b.collapsed,
            filter,
            tasks,
            waiting: &b.waiting,
            done: &b.done,
            declared: &b.declared,
        })
    }

    #[test]
    fn the_whole_kernel_is_shown_including_the_plumbing() {
        let t = kernel();
        let b = Bag::new();
        let ids: Vec<_> = flat(&t, &b, None, &[]).into_iter().map(|r| r.id).collect();
        assert_eq!(
            ids,
            vec![
                "kernel_state",
                "store",
                "roster",
                "ai",
                "web",
                "rest"
            ],
            "a transport leg is an agent and must be selectable"
        );
    }

    #[test]
    fn every_node_gets_the_panel_its_module_calls_for() {
        let t = kernel();
        let b = Bag::new();
        let rows = flat(&t, &b, None, &[]);
        let kind = |id: &str| rows.iter().find(|r| r.id == id).unwrap().kind;
        assert_eq!(kind("ai"), PanelKind::Ai);
        assert_eq!(kind("store"), PanelKind::Files);
        assert_eq!(kind("rest"), PanelKind::Generic);
    }

    #[test]
    fn collapsing_hides_a_whole_subtree_and_nothing_more() {
        let t = kernel();
        let mut b = Bag::new();
        b.collapsed.insert("roster".into());
        let ids: Vec<_> = flat(&t, &b, None, &[]).into_iter().map(|r| r.id).collect();
        assert_eq!(ids, vec!["kernel_state", "store", "roster", "web", "rest"]);
    }

    #[test]
    fn a_bare_grouping_node_is_not_drawn_and_its_children_rise_one_level() {
        // A bare node dispatches nothing, so its label is noise. It is not a
        // row; what it groups sits one level up.
        let t = node(
            "kernel_state",
            Some("kernel_state.tools"),
            vec![
                node("group", None, vec![
                    node("exec", Some("exec_bridge.tools"), vec![]),
                    node("files", Some("file_bridge.tools"), vec![]),
                ]),
                node("fs_loader", Some("static_loader.tools"), vec![]),
            ],
        );
        let b = Bag::new();
        let rows = flat(&t, &b, None, &[]);
        let got: Vec<(&str, u8)> = rows.iter().map(|r| (r.id.as_str(), r.depth)).collect();
        assert_eq!(got, vec![("kernel_state", 0), ("exec", 1), ("files", 1), ("fs_loader", 1)]);
        assert!(rows.iter().all(|r| r.live), "every row drawn has something to call");
    }

    #[test]
    fn a_filter_flattens_to_the_matches() {
        let t = kernel();
        let b = Bag::new();
        let rows = flat(&t, &b, Some("st"), &[]);
        let ids: Vec<_> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["kernel_state", "store", "roster", "rest"]);
        assert!(
            rows.iter().all(|r| r.depth == 0),
            "a filtered list has no hierarchy to indent"
        );
    }

    #[test]
    fn a_filter_ignores_collapse_so_a_match_is_never_hidden() {
        let t = kernel();
        let mut b = Bag::new();
        b.collapsed.insert("roster".into());
        let ids: Vec<_> = flat(&t, &b, Some("ai"), &[])
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(ids, vec!["ai"]);
    }

    #[test]
    fn blocked_outranks_working_because_a_question_needs_you_first() {
        use crate::kern::model::Task;
        let t = kernel();
        let mut b = Bag::new();
        b.waiting.insert("ai".into(), 2);
        let tasks = vec![Task {
            id: 1,
            agent: "ai".into(),
            parent: None,
            verb: "send".into(),
            started_at: 0.0,
        }];
        let rows = flat(&t, &b, None, &tasks);
        let ai = rows.iter().find(|r| r.id == "ai").unwrap();
        assert_eq!(ai.presence, Presence::Blocked);
        assert_eq!(ai.inquiries, 2);
    }

    #[test]
    fn an_infra_loop_never_makes_an_agent_look_busy() {
        use crate::kern::model::Task;
        let t = kernel();
        let b = Bag::new();
        let tasks = vec![Task {
            id: 1,
            agent: "rest".into(),
            parent: None,
            verb: "ws_drain".into(),
            started_at: 0.0,
        }];
        let rows = flat(&t, &b, None, &tasks);
        let rest = rows.iter().find(|r| r.id == "rest").unwrap();
        assert_eq!(rest.presence, Presence::Idle);
    }

    #[test]
    fn a_declared_agent_gets_the_declared_kind_in_the_tree_and_a_built_in_keeps_its_own() {
        let t = node(
            "root",
            Some("kernel_state.tools"),
            vec![
                node("mine", Some("my.tools"), vec![]),
                node("plain", Some("other.tools"), vec![]),
                node("sched", Some("scheduler.tools"), vec![]),
            ],
        );
        let mut b = Bag::new();
        b.declared = ["mine", "sched"].iter().map(|s| s.to_string()).collect();
        let kind = |id: &str| flat(&t, &b, None, &[]).into_iter().find(|r| r.id == id).unwrap().kind;
        assert_eq!(kind("mine"), PanelKind::Declared);
        assert_eq!(kind("plain"), PanelKind::Generic);
        assert_eq!(kind("sched"), PanelKind::Scheduler);
    }

    #[test]
    fn an_empty_tree_renders_nothing_rather_than_inventing_a_root() {
        let b = Bag::new();
        let rows = flatten(&Src {
            tree: None,
            collapsed: &b.collapsed,
            filter: None,
            tasks: &[],
            waiting: &b.waiting,
            done: &b.done,
            declared: &b.declared,
        });
        assert!(rows.is_empty());
    }
}
