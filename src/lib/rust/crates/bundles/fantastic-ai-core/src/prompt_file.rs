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

/// Where the next step goes once the file is current ([`refresh`]).
type Next<R> = Box<dyn FnOnce(R) -> futures_util::future::BoxFuture<'static, ()> + Send>;

/// BRING THE FILE CURRENT for this pass, as STEPS (#795): the view watched and its
/// change relay armed the first time a pass needs it (or when the record names another
/// view or path), the file read again when the view reported it changed — each call a
/// step, whose settle is the next one; then `next`. Nothing is looked up when
/// `system_prompt_file` is not set (the default block), and a file held before is
/// dropped; nor when the record's own `system_prompt` is in force. What the pass then
/// reads is [`current`].
pub(crate) async fn refresh<R, F, Fut>(self_id: &AgentId, kernel: &Arc<Kernel>, rec: R, next: F)
where
    R: Send + 'static,
    F: FnOnce(R) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let meta = crate::helpers::agent_meta(self_id, kernel);
    // the record's own `system_prompt` beats the file: nothing is looked up then
    if crate::record::record_system_prompt(&meta).is_some() {
        return next(rec).await;
    }
    let held = files()
        .lock()
        .expect("prompt files")
        .get(self_id.as_str())
        .map(|s| (s.file.clone(), s.task.is_some()));
    let Some(file) = ref_of(&meta) else {
        if held.is_some() {
            drop_file(self_id, kernel).await;
        }
        return next(rec).await;
    };
    let next: Next<R> = Box::new(move |rec| Box::pin(next(rec)));
    match held {
        Some((f, _)) if f != file => {
            drop_file(self_id, kernel).await;
            arm(self_id, kernel, file, rec, next).await;
        }
        // arming failed on an earlier pass — the same one path, again
        Some((_, false)) | None => arm(self_id, kernel, file, rec, next).await,
        Some(_) => reread(self_id, kernel, file, rec, next).await,
    }
}

/// The file's text for this pass, as [`refresh`] left it; `None` when
/// `system_prompt_file` is not set (the default block). Set and unusable — no
/// `file_bridge_id`, a refused or failed read, an empty file — is a
/// [`PromptFileError`]: the pass fails loud, it never falls to another block.
///
/// # Errors
/// The file is set and unusable, or no refresh has read it yet.
pub(crate) fn current(self_id: &AgentId, kernel: &Kernel) -> Result<Option<String>, PromptFileError> {
    let meta = crate::helpers::agent_meta(self_id, kernel);
    if ref_of(&meta).is_none() {
        if let Some(path) = named(&meta, "system_prompt_file") {
            return Err(PromptFileError(format!(
                "{self_id}: system_prompt_file {} is set but the record has no file_bridge_id \
                 — name the file view it is read through",
                py_repr(path)
            )));
        }
        return Ok(None);
    }
    let all = files().lock().expect("prompt files");
    match all.get(self_id.as_str()) {
        Some(FileState { error: Some(e), .. }) => Err(PromptFileError(format!(
            "{self_id}: system_prompt_file: {e}"
        ))),
        Some(st) => Ok(st.text.clone()),
        None => Err(PromptFileError(format!(
            "{self_id}: system_prompt_file: not read yet"
        ))),
    }
}

fn why(status: Status, res: &Value) -> String {
    fantastic_kernel::refusal::error_text(res).unwrap_or_else(|| status.as_str().to_string())
}

fn failed(status: Status, res: &Value) -> bool {
    status != Status::Completed
        || !res.is_object()
        || res.get("error").is_some_and(|e| !e.is_null())
}

/// Set why the file is unusable.
fn set_error(self_id: &AgentId, e: String) {
    if let Some(st) = files()
        .lock()
        .expect("prompt files")
        .get_mut(self_id.as_str())
    {
        st.error = Some(e);
    }
}

/// Watch the view (its events reach a private inbox), then arm its change relay
/// (`watch`, a step) — the watch first, so no change between the two is missed; then
/// the first read.
async fn arm<R: Send + 'static>(self_id: &AgentId, kernel: &Arc<Kernel>, file: (String, String), rec: R, next: Next<R>) {
    let (view, path) = file.clone();
    let inbox = AgentId::from(format!("{}/prompt_file", self_id.as_str()));
    let stale = Arc::new(AtomicBool::new(true));
    files().lock().expect("prompt files").insert(
        self_id.as_str().to_string(),
        FileState {
            file: file.clone(),
            text: None,
            stale: Arc::clone(&stale),
            error: None,
            inbox: inbox.clone(),
            task: None,
        },
    );
    let view_id = AgentId::from(view.as_str());
    let mut rx = kernel.ensure_inbox(&inbox);
    if let Some(denial) = kernel.watch(&view_id, inbox.clone()).await {
        kernel.remove_inbox(&inbox);
        set_error(
            self_id,
            format!(
                "{view} watch: {}",
                fantastic_kernel::refusal::error_text(&denial).unwrap_or_default()
            ),
        );
        return next(rec).await;
    }
    let item = Item::new(view_id.clone(), json!({"type": "watch"}));
    let (k, me) = (Arc::clone(kernel), self_id.clone());
    crate::steps::call(kernel, item, rec, move |rec, out| async move {
        if failed(out.status, &out.result) {
            k.unwatch(&view_id, &inbox).await;
            k.remove_inbox(&inbox);
            set_error(&me, format!("{view} watch {path}: {}", why(out.status, &out.result)));
            return next(rec).await;
        }
        let norm = normpath(&path);
        let tid = k.spawn_task(&me, "prompt_file", async move {
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
            .get_mut(me.as_str())
        {
            st.task = Some(tid);
        }
        reread(&me, &k, file, rec, next).await;
    })
    .await;
}

/// Read the file again when the view said it changed (or it was never read): its text
/// or why not is kept; then `next`.
async fn reread<R: Send + 'static>(self_id: &AgentId, kernel: &Arc<Kernel>, file: (String, String), rec: R, next: Next<R>) {
    let held = files()
        .lock()
        .expect("prompt files")
        .get(self_id.as_str())
        .map(|st| (Arc::clone(&st.stale), st.task.is_some()));
    let Some((stale, armed)) = held else {
        return next(rec).await;
    };
    if !(armed && stale.swap(false, Ordering::SeqCst)) {
        return next(rec).await;
    }
    let me = self_id.clone();
    let lines: ReadLines<(R, Next<R>, (String, String))> = Box::new(move |(rec, next, file), r| {
        Box::pin(async move {
            let (mut txt, mut err) = match r {
                Ok(t) => (Some(t), None),
                Err(e) => (None, Some(e)),
            };
            if err.is_none() && txt.as_deref().is_some_and(|t| py_strip(t).is_empty()) {
                txt = None;
                err = Some(format!("{} {}: the file is empty", file.0, file.1));
            }
            if let Some(st) = files()
                .lock()
                .expect("prompt files")
                .get_mut(me.as_str())
            {
                st.text = txt;
                st.error = err;
            }
            next(rec).await;
        })
    });
    let (view, path) = file.clone();
    read_lines(Arc::clone(kernel), view, path, Vec::new(), (rec, next, file), lines).await;
}

/// The step that gets a whole file's text, or why not.
type ReadLines<R> =
    Box<dyn FnOnce(R, Result<String, String>) -> futures_util::future::BoxFuture<'static, ()> + Send>;

/// The whole file through the view's `read`, page by page, as STEPS: its lines joined
/// again, with the trailing newline it had. The view refuses a binary or non-utf-8
/// file itself.
fn read_lines<R: Send + 'static>(
    kernel: Arc<Kernel>,
    view: String,
    path: String,
    mut lines: Vec<String>,
    rec: R,
    next: ReadLines<R>,
) -> futures_util::future::BoxFuture<'static, ()> {
    Box::pin(async move {
        let offset = lines.len();
        let item = Item::new(
            AgentId::from(view.as_str()),
            json!({"type": "read", "path": path, "offset": offset}),
        );
        let k = Arc::clone(&kernel);
        crate::steps::call(&kernel, item, rec, move |rec, out| async move {
            let res = out.result;
            if failed(out.status, &res) {
                let why = format!("{view} read {path}: {}", why(out.status, &res));
                return next(rec, Err(why)).await;
            }
            let page = res.get("lines").and_then(Value::as_array);
            lines.extend(
                page.into_iter()
                    .flatten()
                    .filter_map(|l| l.get("text").and_then(Value::as_str))
                    .map(str::to_string),
            );
            if res.get("eof").and_then(Value::as_bool) == Some(true) || lines.len() == offset {
                let mut text = lines.join("\n");
                if res.get("newline").and_then(Value::as_bool) == Some(true) {
                    text.push('\n');
                }
                return next(rec, Ok(text)).await;
            }
            read_lines(k, view, path, lines, rec, next).await;
        })
        .await;
    })
}

/// The whole file through `read_stream`, as STEPS (#795): one page per call, each
/// settle the next step; `next` gets its bytes, or why not.
pub(crate) async fn read_bytes<R, F, Fut>(kernel: &Arc<Kernel>, bridge: String, path: String, rec: R, next: F)
where
    R: Send + 'static,
    F: FnOnce(R, Result<Vec<u8>, String>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let next: ReadNext<R> = Box::new(move |rec, r| Box::pin(next(rec, r)));
    read_page(Arc::clone(kernel), bridge, path, 0, Vec::new(), rec, next).await;
}

/// The step that gets a whole file's bytes.
type ReadNext<R> =
    Box<dyn FnOnce(R, Result<Vec<u8>, String>) -> futures_util::future::BoxFuture<'static, ()> + Send>;

/// One page of [`read_bytes`] from `offset`, `bytes` read so far.
fn read_page<R: Send + 'static>(
    kernel: Arc<Kernel>,
    bridge: String,
    path: String,
    offset: u64,
    bytes: Vec<u8>,
    rec: R,
    next: ReadNext<R>,
) -> futures_util::future::BoxFuture<'static, ()> {
    Box::pin(async move {
        let item = Item::new(
            AgentId::from(bridge.as_str()),
            json!({"type": "read_stream", "path": path, "offset": offset}),
        );
        let k = Arc::clone(&kernel);
        crate::steps::call(&kernel, item, (rec, bytes), move |(rec, mut bytes), out| async move {
            if failed(out.status, &out.result) {
                let why = format!("{bridge} read_stream {path}: {}", why(out.status, &out.result));
                return next(rec, Err(why)).await;
            }
            if let Some(b) = out.body.as_ref().and_then(|b| b.at("bytes")) {
                bytes.extend_from_slice(b);
            }
            if out.result.get("eof").and_then(Value::as_bool) == Some(true) {
                return next(rec, Ok(bytes)).await;
            }
            let at = out.result.get("next_offset").and_then(Value::as_u64).unwrap_or(offset);
            read_page(k, bridge, path, at, bytes, rec, next).await;
        })
        .await;
    })
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
