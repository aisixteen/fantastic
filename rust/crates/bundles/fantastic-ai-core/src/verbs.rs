//! Verb entrypoints the backends dispatch through. A backend's `Bundle`
//! does only its own pre-checks (file_bridge_id, api_key) + provider
//! build, then calls these. The provider is the per-backend seam,
//! constructed by the backend and passed in per `send`.

use crate::agent_loop::{run_generation, BackendConfig};
use crate::events::{emit_done, emit_status, to_caller};
use crate::helpers::{mint_send_id, now_secs, safe_client, DEFAULT_CLIENT_ID};
use crate::history::load_history;
use crate::provider::Provider;
use crate::state::{state_for, status_snapshot, CurrentEntry, QueuedEntry};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;

/// `status` verb.
pub fn status(agent_id: &AgentId, payload: &Value) -> Value {
    status_snapshot(agent_id, payload)
}

/// `interrupt` verb.
pub fn interrupt(agent_id: &AgentId) -> Value {
    if crate::state::interrupt(agent_id) {
        json!({"interrupted": true})
    } else {
        json!({"interrupted": false})
    }
}

/// `refresh_menu` verb.
pub fn refresh_menu(agent_id: &AgentId) -> Value {
    let state = state_for(agent_id);
    *state.menu.lock().expect("menu poisoned") = None;
    json!({"refreshed": true})
}

/// `history` verb. `name` is the backend's error-message prefix.
pub async fn history(
    agent_id: &AgentId,
    payload: &Value,
    kernel: &Arc<Kernel>,
    name: &str,
) -> Value {
    if crate::helpers::file_bridge_id(agent_id, kernel).is_none() {
        return json!({"error": format!("{name}: file_bridge_id required")});
    }
    let client_id = safe_client(
        payload
            .get("client_id")
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_CLIENT_ID),
    );
    let messages = load_history(agent_id, kernel, &client_id).await;
    json!({"messages": messages, "client_id": client_id})
}

/// Per-`send` ceiling, in seconds.
pub const SEND_TIMEOUT_SECS: u64 = 180;

/// The shared `send` flow: enqueue → FIFO lock → spawn the agentic loop
/// → wait with timeout → clean up. The backend supplies a built
/// `provider` (it has already failfast-checked file_bridge_id / api_key)
/// and its `cfg`.
pub async fn send(
    provider: Arc<dyn Provider>,
    agent_id: &AgentId,
    payload: &Value,
    kernel: &Arc<Kernel>,
    cfg: BackendConfig,
) -> Value {
    let route = cfg.route;
    let text = payload
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let client_id = safe_client(
        payload
            .get("client_id")
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_CLIENT_ID),
    );
    let send_id = mint_send_id();
    let state = state_for(agent_id);

    // Enqueue.
    let entry = QueuedEntry {
        client_id: client_id.clone(),
        text: text.clone(),
        send_id: send_id.clone(),
        queued_at: now_secs(),
    };
    state.queue.lock().expect("queue poisoned").push_back(entry);

    // Best-effort contention detection.
    let contested = state.lock.try_lock().is_err();
    if contested {
        let ahead = state
            .queue
            .lock()
            .expect("queue poisoned")
            .len()
            .saturating_sub(1);
        to_caller(
            kernel,
            agent_id,
            &client_id,
            route,
            json!({"type": "queued", "source": agent_id.as_str(), "send_id": send_id}),
        )
        .await;
        let mut detail = Map::new();
        detail.insert("send_id".to_string(), json!(send_id));
        detail.insert("ahead".to_string(), json!(ahead));
        emit_status(
            kernel, &state, agent_id, &client_id, route, "queued", detail,
        )
        .await;
    }

    // Acquire FIFO lock.
    let lock_arc = Arc::clone(&state.lock);
    let _guard = lock_arc.lock_owned().await;

    // Pop ourselves from the queue, become the in-flight entry.
    {
        let mut q = state.queue.lock().expect("queue poisoned");
        if let Some(pos) = q.iter().position(|e| e.send_id == send_id) {
            q.remove(pos);
        }
    }
    {
        let mut cur = state.current_meta.lock().expect("current poisoned");
        *cur = Some(CurrentEntry {
            client_id: client_id.clone(),
            text: text.clone(),
            send_id: send_id.clone(),
            started_at: now_secs(),
            phase: "thinking".to_string(),
            text_so_far: String::new(),
            last_tool: None,
        });
    }
    emit_status(
        kernel,
        &state,
        agent_id,
        &client_id,
        route,
        "thinking",
        Map::new(),
    )
    .await;

    // Spawn the streaming task so `interrupt` can abort it via JoinHandle.
    let agent_id_owned = agent_id.clone();
    let client_id_owned = client_id.clone();
    let text_owned = text.clone();
    let kernel_owned = Arc::clone(kernel);
    let state_owned = Arc::clone(&state);
    let provider_owned = Arc::clone(&provider);
    let task_result: Arc<AsyncMutex<Option<Result<String, String>>>> =
        Arc::new(AsyncMutex::new(None));
    let task_result_inner = Arc::clone(&task_result);

    let join: JoinHandle<()> = tokio::spawn(async move {
        let outcome = run_generation(
            &provider_owned,
            &agent_id_owned,
            &state_owned,
            &text_owned,
            &kernel_owned,
            &client_id_owned,
            cfg,
        )
        .await;
        *task_result_inner.lock().await = Some(outcome);
    });

    // Stash the JoinHandle so interrupt can abort it.
    {
        let mut t = state.current_task.lock().expect("task poisoned");
        if let Some(prev) = t.take() {
            prev.abort();
        }
        *t = Some(join);
    }
    let abort_handle = {
        let t = state.current_task.lock().expect("task poisoned");
        t.as_ref().map(|j| j.abort_handle())
    };

    // Wait for completion or timeout.
    let timeout = Duration::from_secs(SEND_TIMEOUT_SECS);
    let wait_outcome: Result<Option<Result<String, String>>, &'static str> = {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let finished = state
                .current_task
                .lock()
                .expect("task poisoned")
                .as_ref()
                .map(|j| j.is_finished())
                .unwrap_or(true);
            if finished {
                break Ok(task_result.lock().await.take());
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                if let Some(ah) = &abort_handle {
                    ah.abort();
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
                break Err("timeout");
            }
            let step = std::cmp::min(
                Duration::from_millis(50),
                deadline.saturating_duration_since(now),
            );
            tokio::time::sleep(step).await;
        }
    };

    // Clear the task slot (drop the JoinHandle).
    let taken = state.current_task.lock().expect("task poisoned").take();
    drop(taken);

    let reply = match wait_outcome {
        Ok(Some(Ok(final_text))) => {
            json!({
                "response": final_text,
                "final": final_text,
                "client_id": client_id,
            })
        }
        Ok(Some(Err(e))) => {
            emit_status(kernel, &state, agent_id, &client_id, route, "done", {
                let mut m = Map::new();
                m.insert("reason".to_string(), json!("error"));
                m.insert("error".to_string(), json!(e.clone()));
                m
            })
            .await;
            emit_done(kernel, agent_id, &client_id, route).await;
            json!({"error": e, "client_id": client_id})
        }
        Ok(None) => {
            // Task aborted (interrupt) — task_result never wrote.
            emit_status(kernel, &state, agent_id, &client_id, route, "done", {
                let mut m = Map::new();
                m.insert("reason".to_string(), json!("interrupted"));
                m
            })
            .await;
            emit_done(kernel, agent_id, &client_id, route).await;
            json!({"response": "", "interrupted": true, "client_id": client_id})
        }
        Err(_) => {
            emit_status(kernel, &state, agent_id, &client_id, route, "done", {
                let mut m = Map::new();
                m.insert("reason".to_string(), json!("timeout"));
                m
            })
            .await;
            emit_done(kernel, agent_id, &client_id, route).await;
            json!({
                "error": format!("send: timeout after {}s", SEND_TIMEOUT_SECS),
                "client_id": client_id,
            })
        }
    };

    // Clear current_meta.
    *state.current_meta.lock().expect("current poisoned") = None;
    drop(_guard);
    reply
}
