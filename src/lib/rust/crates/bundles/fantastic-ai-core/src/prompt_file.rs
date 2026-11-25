//! The system block from a FILE: the record's `file_bridge_id`
//! names the file VIEW this agent reads it THROUGH, `system_prompt_file` the view's
//! own path. Not set → nothing is looked up (the default block). Set and unusable
//! → [`PromptFileError`]: the turn fails loud, never another block.
//!
//! The agent reads the file through that view, AS ITSELF, with the view's own verbs
//! only — every read and watch is its own gated send, so its own `can` holds a line
//! on the view (`read`, `watch`). It watches the view through a private inbox and
//! arms the view's change relay (`watch`): a `changed` event for the path marks the
//! text STALE, and the next turn reads it again — no restart, and no read on a turn
//! nothing changed. RAM only: armed on the first turn that needs it, re-armed when
//! the record names another view or path, dropped when either field is cleared
//! or the agent deleted. The reason a set file is unusable is also the agent's
//! `system_prompt_file_error` until a read succeeds.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use fantastic_bundle::{py_repr, py_strip};
use fantastic_kernel::{AgentId, Item, Kernel, Status, TaskId};
use serde_json::{json, Map, Value};

/// `system_prompt_file` is set and cannot be the block — the turn fails, said so.
/// Python `PromptFileError(RuntimeError)`.
#[derive(Debug, Clone)]
pub struct PromptFileError(pub String);

impl std::fmt::Display for PromptFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PromptFileError: {}", self.0)
    }
}

/// A raise out of a verb handler (the `context_recall` verb) — the kernel settles
/// it `failed` with python's `f"{type(e).__name__}: {e}"`.
impl std::error::Error for PromptFileError {}

struct FileState {
    file: (String, String),
    text: Option<String>,
    stale: Arc<AtomicBool>,
    error: Option<String>,
    inbox: AgentId,
    task: Option<TaskId>,
}

fn files() -> &'static Mutex<HashMap<String, FileState>> {
    static FILES: OnceLock<Mutex<HashMap<String, FileState>>> = OnceLock::new();
    FILES.get_or_init(Mutex::default)
}

fn named<'a>(meta: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    meta.get(key)?.as_str()
}

/// `(file_bridge_id, system_prompt_file)` — the view and its path — when the record
/// names both, or `None`.
#[must_use]
pub fn ref_of(meta: &Map<String, Value>) -> Option<(String, String)> {
    let view = named(meta, "file_bridge_id")?;
    let path = named(meta, "system_prompt_file")?;
    Some((view.to_string(), path.to_string()))
}

/// Why the file is not this agent's block right now, or `None`.
#[must_use]
pub fn error_of(self_id: &AgentId) -> Option<String> {
    files()
        .lock()
        .expect("prompt files")
        .get(self_id.as_str())
        .and_then(|s| s.error.clone())
}

/// python `posixpath.normpath` for a path under a root.
fn normpath(p: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." if out.last().is_some_and(|l| *l != "..") => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    if out.is_empty() {
        ".".to_string()
    } else {
        out.join("/")
    }
}

/// The file's text for this turn; `None` when `system_prompt_file` is not set
/// (nothing is looked up — the default block). Set and unusable — no
/// `file_bridge_id`, a refused or failed read, an empty file — is a
/// [`PromptFileError`]: the turn fails loud, it never falls to another block. Runs
/// in the agent's own turn, so every send here is the agent's.
pub async fn text(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
) -> Result<Option<String>, PromptFileError> {
    let meta = crate::helpers::agent_meta(self_id, kernel);
    let wanted = ref_of(&meta);
    let held = files()
        .lock()
        .expect("prompt files")
        .get(self_id.as_str())
        .map(|s| (s.file.clone(), s.task.is_some()));
    let Some(file) = wanted else {
        if held.is_some() {
            drop_file(self_id, kernel).await;
        }
        if let Some(path) = named(&meta, "system_prompt_file") {
            return Err(PromptFileError(format!(
                "{self_id}: system_prompt_file {} is set but the record has no file_bridge_id \
                 — name the file view it is read through",
                py_repr(path)
            )));
        }
        return Ok(None);
    };
    match held {
        Some((f, _)) if f != file => {
            drop_file(self_id, kernel).await;
            arm(self_id, kernel, file.clone()).await;
        }
        // arming failed on an earlier turn — the same one path, again
        Some((_, false)) | None => arm(self_id, kernel, file.clone()).await,
        Some(_) => {}
    }
    let (stale, armed) = {
        let all = files().lock().expect("prompt files");
        let st = all.get(self_id.as_str()).expect("armed above");
        (Arc::clone(&st.stale), st.task.is_some())
    };
    if armed && stale.swap(false, Ordering::SeqCst) {
        let (mut txt, mut err) = read(kernel, &file.0, &file.1).await;
        if err.is_none() && txt.as_deref().is_some_and(|t| py_strip(t).is_empty()) {
            txt = None;
            err = Some(format!("{} {}: the file is empty", file.0, file.1));
        }
        if let Some(st) = files()
            .lock()
            .expect("prompt files")
            .get_mut(self_id.as_str())
        {
            st.text = txt;
            st.error = err;
        }
    }
    let all = files().lock().expect("prompt files");
    let st = all.get(self_id.as_str()).expect("armed above");
    match &st.error {
        Some(e) => Err(PromptFileError(format!(
            "{self_id}: system_prompt_file: {e}"
        ))),
        None => Ok(st.text.clone()),
    }
}

async fn send(kernel: &Arc<Kernel>, target: &str, payload: Value) -> (Status, Value) {
    let out = kernel
        .send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0);
    (out.status, out.result)
}

fn why(status: Status, res: &Value) -> String {
    fantastic_kernel::refusal::error_text(res).unwrap_or_else(|| status.as_str().to_string())
}

fn failed(status: Status, res: &Value) -> bool {
    status != Status::Completed
        || !res.is_object()
        || res.get("error").is_some_and(|e| !e.is_null())
}

/// Watch the view (its events reach a private inbox), then arm its change relay
/// (`watch`) — the watch first, so no change between the two is missed.
async fn arm(self_id: &AgentId, kernel: &Arc<Kernel>, file: (String, String)) {
    let (view, path) = file.clone();
    let inbox = AgentId::from(format!("{}/prompt_file", self_id.as_str()));
    let stale = Arc::new(AtomicBool::new(true));
    files().lock().expect("prompt files").insert(
        self_id.as_str().to_string(),
        FileState {
            file,
            text: None,
            stale: Arc::clone(&stale),
            error: None,
            inbox: inbox.clone(),
            task: None,
        },
    );
    let set_error = |e: String| {
        if let Some(st) = files()
            .lock()
            .expect("prompt files")
            .get_mut(self_id.as_str())
        {
            st.error = Some(e);
        }
    };
    let view_id = AgentId::from(view.as_str());
    let mut rx = kernel.ensure_inbox(&inbox);
    if let Some(denial) = kernel.watch(&view_id, inbox.clone()).await {
        kernel.remove_inbox(&inbox);
        set_error(format!(
            "{view} watch: {}",
            fantastic_kernel::refusal::error_text(&denial).unwrap_or_default()
        ));
        return;
    }
    let (status, res) = send(kernel, &view, json!({"type": "watch"})).await;
    if failed(status, &res) {
        kernel.unwatch(&view_id, &inbox).await;
        kernel.remove_inbox(&inbox);
        set_error(format!("{view} watch {path}: {}", why(status, &res)));
        return;
    }
    let norm = normpath(&path);
    let tid = kernel.spawn_task(self_id, "prompt_file", async move {
        while let Some(item) = rx.recv().await {
            let p = &item.payload;
            if p.get("type").and_then(Value::as_str) == Some("changed")
                && p.get("path")
                    .and_then(Value::as_str)
                    .map(normpath)
                    .as_deref()
                    == Some(&norm)
            {
                stale.store(true, Ordering::SeqCst);
            }
        }
    });
    if let Some(st) = files()
        .lock()
        .expect("prompt files")
        .get_mut(self_id.as_str())
    {
        st.task = Some(tid);
    }
}

/// The whole file through `read_stream`: its bytes, or why not.
pub(crate) async fn read_bytes(
    kernel: &Arc<Kernel>,
    bridge: &str,
    path: &str,
) -> Result<Vec<u8>, String> {
    let mut bytes: Vec<u8> = Vec::new();
    let mut offset: u64 = 0;
    loop {
        let out = kernel
            .send(vec![Item::new(
                AgentId::from(bridge),
                json!({"type": "read_stream", "path": path, "offset": offset}),
            )])
            .await
            .remove(0);
        if failed(out.status, &out.result) {
            return Err(format!(
                "{bridge} read_stream {path}: {}",
                why(out.status, &out.result)
            ));
        }
        if let Some(b) = out.body.as_ref().and_then(|b| b.at("bytes")) {
            bytes.extend_from_slice(b);
        }
        if out.result.get("eof").and_then(Value::as_bool) == Some(true) {
            break;
        }
        offset = out
            .result
            .get("next_offset")
            .and_then(Value::as_u64)
            .unwrap_or(offset);
    }
    Ok(bytes)
}

/// The whole file through the view's `read`, page by page: its lines joined again,
/// with the trailing newline it had → (text, None) or (None, why). The view refuses
/// a binary or non-utf-8 file itself.
async fn read(kernel: &Arc<Kernel>, view: &str, path: &str) -> (Option<String>, Option<String>) {
    let mut lines: Vec<String> = Vec::new();
    let newline = loop {
        let offset = lines.len();
        let (status, res) = send(
            kernel,
            view,
            json!({"type": "read", "path": path, "offset": offset}),
        )
        .await;
        if failed(status, &res) {
            return (
                None,
                Some(format!("{view} read {path}: {}", why(status, &res))),
            );
        }
        let page = res.get("lines").and_then(Value::as_array);
        lines.extend(
            page.into_iter()
                .flatten()
                .filter_map(|l| l.get("text").and_then(Value::as_str))
                .map(str::to_string),
        );
        if res.get("eof").and_then(Value::as_bool) == Some(true) || lines.len() == offset {
            break res.get("newline").and_then(Value::as_bool) == Some(true);
        }
    };
    let mut text = lines.join("\n");
    if newline {
        text.push('\n');
    }
    (Some(text), None)
}

/// Stop watching: the drain task, the watch on the view, the inbox. The view's own
/// relay is the view's (it ends with the view).
pub async fn drop_file(self_id: &AgentId, kernel: &Arc<Kernel>) {
    let Some(st) = files()
        .lock()
        .expect("prompt files")
        .remove(self_id.as_str())
    else {
        return;
    };
    if let Some(tid) = st.task {
        kernel.cancel_task(&tid).await;
        kernel
            .unwatch(&AgentId::from(st.file.0.as_str()), &st.inbox)
            .await;
        kernel.remove_inbox(&st.inbox);
    }
}
