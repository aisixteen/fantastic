//! The **active kernel** — which kernel the chat + hub currently drive.
//!
//! The TUI is a kernel MANAGER. It always holds the in-proc `★ RAM` manager kernel
//! (full local streaming), and can switch to a **folder kernel** reached over the
//! loopback gateway. Both are normalized behind [`ActiveKernel`] so the hub
//! (`reflect_tree`) and the chat (`dispatch`) don't care which one is live.
//!
//! Phase 1: the in-proc arm is the current behavior verbatim; the remote arm does
//! reflect + verbs (request→response over HTTP). Live token-streaming for a remote
//! kernel is Phase 2 (a WS mirror that feeds the same `brain_rx` inbox).

use std::path::PathBuf;
use std::sync::Arc;

use fantastic_host::gateway::{KernelHandle, Runtime};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};

/// Which kernel the chat + hub drive right now.
#[derive(Clone)]
pub enum ActiveKernel {
    /// The in-proc manager kernel (`★ RAM`) — full local streaming.
    InProc(Arc<Kernel>),
    // NOTE(kernel-switcher): the Kernels screen constructs this when you select a
    // folder kernel (Slice 4); the arms below are already implemented so the switch
    // is a one-line change there. `allow` drops when that lands.
    /// A folder kernel over the loopback gateway (reflect + verbs; Phase 2 streams).
    #[allow(dead_code)]
    Remote(RemoteKernel),
}

/// A folder kernel reached over the gateway — an HTTP handle + a display label.
#[allow(dead_code)]
#[derive(Clone)]
pub struct RemoteKernel {
    pub handle: KernelHandle,
    /// Short label for the header/squares (the folder basename).
    pub label: String,
}

/// One square on the **Kernels** screen (the top nav level).
pub enum Square {
    /// The in-proc `★ RAM` manager kernel — always first, always live.
    Ram,
    /// A folder kernel from the persisted registry (live = a reflect-ping answered).
    Folder {
        dir: PathBuf,
        runtime: Runtime,
        live: bool,
    },
    /// The trailing `+ new` action (opens the folder browser).
    New,
}

impl Square {
    /// The folder basename shown on the square (`~` collapsed), or the fixed labels.
    pub fn label(&self) -> String {
        match self {
            Square::Ram => "★ RAM".to_string(),
            Square::New => "+ new".to_string(),
            Square::Folder { dir, .. } => dir
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| dir.to_string_lossy().into_owned()),
        }
    }
}

/// The Kernels-screen model: the row of squares + the selection. `★ RAM` is
/// synthetic (always index 0); `+ new` is always last; folder kernels sit between.
pub struct KernelsView {
    pub squares: Vec<Square>,
    pub sel: usize,
}

impl Default for KernelsView {
    fn default() -> Self {
        Self {
            squares: vec![Square::Ram, Square::New],
            sel: 0,
        }
    }
}

impl KernelsView {
    /// Rebuild the squares from the registry folders + their live flags — `★ RAM`
    /// first, `+ new` last — keeping the selection index in range.
    pub fn rebuild(&mut self, folders: Vec<(PathBuf, Runtime, bool)>) {
        let mut squares = vec![Square::Ram];
        for (dir, runtime, live) in folders {
            squares.push(Square::Folder { dir, runtime, live });
        }
        squares.push(Square::New);
        self.sel = self.sel.min(squares.len() - 1);
        self.squares = squares;
    }

    /// ← : the previous square (clamped).
    pub fn move_left(&mut self) {
        self.sel = self.sel.saturating_sub(1);
    }

    /// → : the next square (clamped).
    pub fn move_right(&mut self) {
        if self.sel + 1 < self.squares.len() {
            self.sel += 1;
        }
    }

    /// The selected square (clamped into range).
    pub fn selected(&self) -> &Square {
        &self.squares[self.sel.min(self.squares.len().saturating_sub(1))]
    }
}

impl ActiveKernel {
    /// The in-proc kernel, if this is the RAM kernel (`None` for a remote one).
    /// Used by the AI-turn path, whose brain provisioning is in-proc only in Phase 1.
    pub fn in_proc(&self) -> Option<Arc<Kernel>> {
        match self {
            ActiveKernel::InProc(k) => Some(Arc::clone(k)),
            ActiveKernel::Remote(_) => None,
        }
    }

    /// Reflect the whole NESTED agent tree (the hub reads `reply["tree"]`).
    pub async fn reflect_tree(&self) -> Value {
        match self {
            ActiveKernel::InProc(k) => {
                k.send(&AgentId::from("kernel"), json!({"type":"reflect"}))
                    .await
            }
            // The gateway reflects the ROOT record (which carries `["tree"]`); pass
            // it straight through — `Hub::from_reflect` reads `["tree"]` and falls
            // back to a synthetic tree if it's absent.
            ActiveKernel::Remote(r) => r.handle.reflect(None).await.unwrap_or_else(|_| json!({})),
        }
    }

    /// The flat list of addressable agent ids (Tab-complete / Shift-Tab / nogo).
    pub async fn agent_ids(&self) -> Vec<String> {
        let tree = match self {
            ActiveKernel::InProc(k) => {
                k.send(
                    &AgentId::from("kernel"),
                    json!({"type":"reflect","tree":"ids"}),
                )
                .await
            }
            ActiveKernel::Remote(r) => r.handle.reflect(None).await.unwrap_or_else(|_| json!({})),
        };
        let mut ids = vec!["kernel".to_string()];
        crate::collect_ids(&tree, &mut ids);
        ids
    }

    /// Dispatch a verb to `target` and return the reply. Runs inside a spawned task.
    ///
    /// InProc **mirrors** the target's inbox first (`watch`) so any events it emits
    /// while handling the verb render live via `brain_rx` — the same path `@ai`
    /// uses. Remote is HTTP request→response (its live emits arrive over the Phase-2
    /// WS mirror, not here).
    pub async fn dispatch(&self, target: &AgentId, payload: Value) -> Value {
        match self {
            ActiveKernel::InProc(k) => {
                k.watch(target, AgentId::from(crate::CLIENT_ID)).await;
                k.send(target, payload).await
            }
            ActiveKernel::Remote(r) => r
                .handle
                .send(target.as_str(), payload)
                .await
                .unwrap_or_else(|e| json!({ "error": e.to_string() })),
        }
    }
}
