//! The game **hub** (lobby): the kernel's agents as a **hierarchical top-down
//! tree** (UML/graphviz style — root on top, children fanning out below, joined by
//! connector lines). Every node IS an agent; Enter opens the selected agent's
//! chat. The brain is pinned as **home** (`★ ai`, AI-first). Arrows fly the tree:
//! ↑ to the parent, ↓ to a child, ←/→ between siblings.
//!
//! Pure model (no terminal) + fully unit-tested. It parses a `reflect` reply into
//! a single-root tree, grafts the brain (`★ ai`) and the host pseudo-agents
//! (`sh`/`ws`) onto the root, and lays the nodes out with a tidy leaf-packed
//! algorithm (depth = row, `col` = horizontal slot). The TUI reads `nodes()` +
//! `sel` to draw the tree and `selected_agent()` to open one.

use serde_json::Value;

/// A recursive builder node — the raw shape before layout/flattening.
struct Build {
    id: String,
    label: String,
    agent: String,
    ty: Option<String>,
    is_home: bool,
    children: Vec<Build>,
}

/// One laid-out node of the tree (what the renderer draws + what keys act on).
pub struct HubNode {
    /// Display label without the `@` (the brain shows as `ai`).
    pub label: String,
    /// The agent to open on Enter (the brain → `"ai"`).
    pub agent: String,
    /// Type tag from `handler_module` (`.tools` stripped); `None` = bare/container.
    pub ty: Option<String>,
    /// The brain — pinned home, marked `★`.
    pub is_home: bool,
    /// Tree depth (0 = root) → the row it sits on.
    pub depth: usize,
    /// Horizontal slot in leaf units (internal nodes center over their children).
    pub col: f32,
    /// Index of the parent node (`None` for the root).
    pub parent: Option<usize>,
    /// Indices of the child nodes, left→right.
    pub children: Vec<usize>,
}

/// The laid-out agent tree + the current selection.
pub struct Hub {
    nodes: Vec<HubNode>,
    pub sel: usize,
}

fn strip_tools(h: &str) -> String {
    h.strip_suffix(".tools").unwrap_or(h).to_string()
}

fn parse(v: &Value) -> Build {
    let id = v
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let dn = v
        .get("display_name")
        .and_then(Value::as_str)
        .unwrap_or(&id)
        .to_string();
    let ty = v
        .get("handler_module")
        .and_then(Value::as_str)
        .map(strip_tools);
    let children = v
        .get("children")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(parse).collect())
        .unwrap_or_default();
    let is_brain = id == "brain";
    Build {
        agent: if is_brain { "ai".into() } else { id.clone() },
        label: if is_brain { "ai".into() } else { dn },
        is_home: is_brain,
        id,
        ty,
        children,
    }
}

/// Mark the brain node `★ ai` home wherever it sits; return whether one was found.
fn home_brain(b: &mut Build) -> bool {
    if b.id == "brain" {
        b.is_home = true;
        b.label = "ai".into();
        b.agent = "ai".into();
        return true;
    }
    b.children.iter_mut().any(home_brain)
}

fn synthetic(id: &str, ty: &str, home: bool) -> Build {
    Build {
        id: id.into(),
        label: id.into(),
        agent: id.into(),
        ty: Some(ty.into()),
        is_home: home,
        children: Vec::new(),
    }
}

impl Hub {
    /// Build from a root `reflect` reply (`reply["tree"]` = the nested kernel tree).
    /// The kernel root is THE root of a single hierarchical tree. AI-first: the
    /// brain becomes `★ ai` in place, or — if it isn't provisioned yet (lazy) — a
    /// synthetic `★ ai` is grafted as the root's first child so `@ai` is always
    /// enterable. The host pseudo-agents `@sh` / `@ws` (not kernel agents) hang off
    /// the root at the tail.
    pub fn from_reflect(reply: &Value) -> Hub {
        let mut root = match reply.get("tree") {
            Some(t) => parse(t),
            None => synthetic("kernel", "kernel", false),
        };
        if !home_brain(&mut root) {
            root.children.insert(0, synthetic("ai", "brain", true));
        }
        root.children.push(synthetic("sh", "shell", false));
        root.children.push(synthetic("ws", "workspace", false));

        let mut nodes: Vec<HubNode> = Vec::new();
        let mut next_leaf = 0.0_f32;
        flatten(root, 0, None, &mut nodes, &mut next_leaf);
        // Start on home (the brain) — AI-first; fall back to the root.
        let sel = nodes.iter().position(|n| n.is_home).unwrap_or(0);
        Hub { nodes, sel }
    }

    /// All laid-out nodes (root first, then pre-order).
    pub fn nodes(&self) -> &[HubNode] {
        &self.nodes
    }

    /// The agent the selected node opens (for Enter → `Tabs::focus`).
    pub fn selected_agent(&self) -> String {
        self.nodes[self.sel].agent.clone()
    }

    /// ↑ : climb to the parent.
    pub fn move_up(&mut self) {
        if let Some(p) = self.nodes[self.sel].parent {
            self.sel = p;
        }
    }

    /// ↓ : descend to the first (leftmost) child.
    pub fn move_down(&mut self) {
        if let Some(&c) = self.nodes[self.sel].children.first() {
            self.sel = c;
        }
    }

    /// ← : the nearest node to the left on the same row.
    pub fn move_left(&mut self) {
        self.sel = self.sibling(false).unwrap_or(self.sel);
    }

    /// → : the nearest node to the right on the same row.
    pub fn move_right(&mut self) {
        self.sel = self.sibling(true).unwrap_or(self.sel);
    }

    /// Nearest node on the same depth to the right (`fwd`) or left of the selection.
    fn sibling(&self, fwd: bool) -> Option<usize> {
        let cur = &self.nodes[self.sel];
        self.nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.depth == cur.depth)
            .filter(|(_, n)| {
                if fwd {
                    n.col > cur.col
                } else {
                    n.col < cur.col
                }
            })
            .min_by(|(_, a), (_, b)| {
                // Closest by column in the chosen direction.
                let da = (a.col - cur.col).abs();
                let db = (b.col - cur.col).abs();
                da.partial_cmp(&db).unwrap()
            })
            .map(|(i, _)| i)
    }
}

/// Flatten a `Build` subtree into `nodes` (reserving each node's slot before its
/// children so parent/child indices are stable), assigning `depth` + a leaf-packed
/// `col`. Returns the pushed node's index.
fn flatten(
    b: Build,
    depth: usize,
    parent: Option<usize>,
    nodes: &mut Vec<HubNode>,
    next_leaf: &mut f32,
) -> usize {
    let idx = nodes.len();
    let Build {
        id: _,
        label,
        agent,
        ty,
        is_home,
        children,
    } = b;
    // Reserve this node's slot; fields patched once children (and `col`) are known.
    nodes.push(HubNode {
        label,
        agent,
        ty,
        is_home,
        depth,
        col: 0.0,
        parent,
        children: Vec::new(),
    });
    let mut child_idxs = Vec::new();
    for c in children {
        child_idxs.push(flatten(c, depth + 1, Some(idx), nodes, next_leaf));
    }
    let col = if child_idxs.is_empty() {
        let c = *next_leaf;
        *next_leaf += 1.0;
        c
    } else {
        let first = nodes[*child_idxs.first().unwrap()].col;
        let last = nodes[*child_idxs.last().unwrap()].col;
        (first + last) / 2.0
    };
    nodes[idx].col = col;
    nodes[idx].children = child_idxs;
    idx
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn reply_with_brain() -> Value {
        json!({"tree": {
            "id": "core", "display_name": "core", "handler_module": null,
            "children": [
                {"id": "brain", "display_name": "brain", "handler_module": "ollama_backend.tools", "children": []},
                {"id": "fs", "display_name": "fs", "handler_module": "file_bridge.tools", "children": [
                    {"id": "store", "display_name": "store", "handler_module": "yaml_state.tools", "children": []}
                ]}
            ]
        }})
    }

    fn node<'a>(hub: &'a Hub, agent: &str) -> &'a HubNode {
        hub.nodes().iter().find(|n| n.agent == agent).expect("node")
    }

    /// The currently-selected node.
    fn sel(hub: &Hub) -> &HubNode {
        &hub.nodes()[hub.sel]
    }

    #[test]
    fn single_root_tree_with_brain_as_home() {
        let hub = Hub::from_reflect(&reply_with_brain());
        // core is THE root (depth 0, no parent); brain is a child, home, → ai.
        let core = node(&hub, "core");
        assert_eq!(core.depth, 0);
        assert!(core.parent.is_none());
        let brain = node(&hub, "ai"); // the brain, remapped to the `ai` agent
        assert_eq!(brain.depth, 1);
        assert_eq!(brain.label, "ai");
        assert!(brain.is_home);
        assert_eq!(brain.ty.as_deref(), Some("ollama_backend"));
        // store nests under fs (a real grandchild) — the tree is hierarchical.
        assert_eq!(node(&hub, "store").depth, 2);
        // host pseudo-agents hang off the root.
        assert_eq!(node(&hub, "sh").depth, 1);
        assert_eq!(node(&hub, "ws").depth, 1);
    }

    #[test]
    fn synthetic_ai_grafted_when_no_brain_yet() {
        let reply = json!({"tree": {"id":"core","display_name":"core","children":[]}});
        let hub = Hub::from_reflect(&reply);
        let ai = node(&hub, "ai");
        assert!(ai.is_home && ai.agent == "ai" && ai.depth == 1);
        assert_eq!(ai.ty.as_deref(), Some("brain")); // synthetic stand-in
                                                     // Selection starts on home.
        assert!(sel(&hub).is_home);
    }

    #[test]
    fn layout_centers_parents_over_children() {
        let hub = Hub::from_reflect(&reply_with_brain());
        // core's col is the midpoint of its children's cols → strictly between the
        // leftmost and rightmost child.
        let core = node(&hub, "core");
        let cols: Vec<f32> = core.children.iter().map(|&i| hub.nodes()[i].col).collect();
        let (lo, hi) = (
            cols.iter().cloned().fold(f32::MAX, f32::min),
            cols.iter().cloned().fold(f32::MIN, f32::max),
        );
        assert!(
            core.col > lo && core.col < hi,
            "root centered over children"
        );
    }

    #[test]
    fn arrows_walk_parent_child_siblings() {
        let mut hub = Hub::from_reflect(&reply_with_brain());
        // Start on home (the brain, → `ai`, a child of core).
        assert_eq!(sel(&hub).agent, "ai");
        hub.move_up(); // → parent (core, the root)
        assert_eq!(sel(&hub).agent, "core");
        hub.move_down(); // → first child (leftmost) — the brain sits first
        assert_eq!(sel(&hub).agent, "ai");
        hub.move_right(); // → next sibling on the row (fs)
        assert_eq!(sel(&hub).agent, "fs");
        hub.move_left(); // → back to the brain
        assert_eq!(sel(&hub).agent, "ai");
    }

    #[test]
    fn selected_agent_maps_brain_to_ai() {
        let hub = Hub::from_reflect(&reply_with_brain());
        assert_eq!(hub.selected_agent(), "ai");
    }
}
