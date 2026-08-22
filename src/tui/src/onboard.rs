//! **Add an agent** — the connector-onboarding feature in ONE module: the `/setup` ·
//! `/model` wizard (design `20_provider_onboarding.md`) and the `+ agent` picker that
//! seeds an AI backend under the root. (The old authored-script kinds — a JS connector,
//! a plan/execute mind — and their in-frame editor are gone with the per-language
//! script executor; the ONE executor is `exec_bridge`, its ONE shim `python`.)
//!
//! Feature-shaped: its state transitions, its async kernel work, its keys AND its
//! render live here. `lib.rs` keeps only thin wiring — the `App` fields, one `select!`
//! arm ([`on_event`]), the key routes ([`handle_flow_key`]) and the render calls
//! ([`render_flow`] · [`flow_height`]).
//!
//! **Async discipline** (one rule, no exceptions): kernel work never runs inline. A
//! spawned task owns a cloned [`crate::kernels::ActiveKernel`] and reports back through
//! `app.onboard_tx` → the `select!` loop → [`on_event`], the ONLY place a result mutates
//! UI state. The async door is the ollama endpoint probe (`list_models`).
//!
//! **Invisibility discipline** (the second rule): this module's kernel work is PLUMBING,
//! never a chat. Every dispatch here goes through the quiet
//! [`crate::kernels::ActiveKernel::send`], NOT `dispatch` — `dispatch` subscribes the
//! client inbox to the target (that watcher fanout is how a user's `@<id> <verb>` renders
//! its live activity), and the wizard's `create_agent` / `list_models` / `delete_agent`
//! are not the user's commands. What the user sees of this module is only what it pushes
//! deliberately: the wizard controls and its own notes. The throwaway prober is reaped
//! for the same reason — plumbing leaves no node behind.

use fantastic_kernel::AgentId;
use ratatui::crossterm::event::KeyCode;
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::chat::{Body, State};
use crate::{input, App, Phase, BRAIN_ID};

/// The throwaway ollama agent used to probe an endpoint's `/api/tags` (created on
/// demand; any `ollama_backend.tools` agent serves `list_models`).
const OLLAMA_PROBE_ID: &str = "ollama_probe";

/// The only kind the `+ agent` picker offers: an AI backend. (The authored-script
/// kinds — a JS connector, a plan/execute mind — are gone with the per-language
/// script executor; the ONE executor is `exec_bridge`, its ONE shim `python`.) The label is
/// the picker's option string AND the submitted value it matches on.
const KIND_AI: &str = "AI agent";

/// Async results from the onboarding tasks, delivered back into the `select!` loop
/// (see the module async discipline). One channel for both doors.
pub(crate) enum OnboardEvent {
    /// The ollama endpoint is reachable — its installed model names. Tagged with the
    /// probe generation so a stale result (from a cancelled/restarted flow) is dropped.
    Models(u64, Vec<String>),
    /// The endpoint is unreachable / errored — the message names the URL.
    ProbeError(u64, String),
    /// A `ps` → `cancel` round-trip finished (the Esc / Ctrl+C kill — see
    /// [`crate::plan`]): `(the driver whose loop was stopped — `""` for a bare `@ai`
    /// turn, the task ids the kernel ACTUALLY killed)`. An empty kill list means
    /// nothing was in flight, and is reported as such.
    Cancelled(String, Vec<String>),
}

/// Fold an async onboarding result into the UI (called from the `select!` loop — the
/// ONLY place these mutate UI state).
pub(crate) fn on_event(app: &mut App, ev: OnboardEvent) {
    match ev {
        OnboardEvent::Models(gen, models) => on_probe_result(app, gen, Ok(models)),
        OnboardEvent::ProbeError(gen, e) => on_probe_result(app, gen, Err(e)),
        OnboardEvent::Cancelled(driver, killed) => crate::plan::on_cancelled(app, driver, killed),
    }
}

// ── The wizard (`/setup` · `/model`) ────────────────────────────────────────

/// Start the "connect me to" flow (the `/setup` wizard; also `+ agent → AI agent`).
/// Runs over the `@ai` chat so its probe errors + the final confirmation land where
/// the user is looking.
pub(crate) fn start_connect_flow(app: &mut App) {
    let (flow, ctrl) = input::SetupFlow::start_setup();
    app.flow = Some(flow);
    app.control = ctrl;
    app.add_agent_picker = false;
    app.tabs.focus("ai");
    app.composer.set_sender("ai");
}

/// `/model` — keep the configured provider/key and just (re)choose the model. For
/// ollama that keeps the stored endpoint and re-probes it, so the model step is a
/// live autocomplete over what's actually installed. With NO connector configured
/// there is nothing to re-model → fall back to the full `/setup`.
pub(crate) fn start_model_flow(app: &mut App) {
    let cfg = fantastic_host::ai_config();
    let Some(backend) = cfg.backend else {
        start_connect_flow(app);
        return;
    };
    let (flow, adv) = input::SetupFlow::start_model(
        &backend,
        cfg.model.as_deref().unwrap_or(""),
        cfg.key_present,
        cfg.endpoint.as_deref(),
    );
    app.flow = Some(flow);
    app.add_agent_picker = false;
    // Run it over `@ai` — that's where a probe error / the final confirmation land
    // as `brain:` lines.
    app.tabs.focus("ai");
    app.composer.set_sender("ai");
    apply_advance(app, adv);
}

/// Drive the active dynamic control with a key. Esc cancels back to chat; Select /
/// Autocomplete use ↑/↓ + Enter; Field + Autocomplete take chars/Backspace (the
/// autocomplete's chars FILTER rather than type). Enter submits — either the
/// add-agent type picker, or the connector wizard (advancing / probing / finishing).
/// A `Probing` control is inert (the probe result drives the next step).
pub(crate) fn handle_flow_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Esc => close_flow(app),
        KeyCode::Up => app.control.select_up(),
        KeyCode::Down => app.control.select_down(),
        KeyCode::Char(c) => app.control.type_char(c),
        KeyCode::Backspace => app.control.backspace(),
        KeyCode::Enter => {
            // While probing there is nothing to submit — the async result advances us.
            if matches!(app.control, input::Control::Probing { .. }) {
                return;
            }
            let value = app.control.submitted_value();
            // The add-agent type picker rides the same Select gate, but it isn't a
            // wizard step — it CHOOSES which door to open (design 28).
            if app.add_agent_picker {
                app.add_agent_picker = false;
                match value.as_str() {
                    KIND_AI => start_connect_flow(app),
                    _ => app.control = input::Control::Chat,
                }
                return;
            }
            // An autocomplete with no match has nothing to pick — never submit an
            // empty model (that would half-set the connector).
            if matches!(app.control, input::Control::Autocomplete { .. }) && value.is_empty() {
                return;
            }
            let Some(flow) = app.flow.as_mut() else {
                app.control = input::Control::Chat;
                return;
            };
            let adv = flow.submit(value);
            apply_advance(app, adv);
        }
        _ => {}
    }
}

/// Cancel any running flow / picker: back to the composer, nothing saved.
fn close_flow(app: &mut App) {
    app.control = input::Control::Chat;
    app.flow = None;
    app.add_agent_picker = false;
}

/// Apply a wizard [`input::Advance`]: show the next control, kick off the async
/// endpoint probe (showing a "probing…" state), or persist the finished connector.
fn apply_advance(app: &mut App, adv: input::Advance) {
    match adv {
        input::Advance::Next(ctrl) => app.control = ctrl,
        input::Advance::Probe { endpoint } => {
            app.control = input::Control::Probing {
                title: format!("probing {endpoint} …"),
            };
            start_probe(app, endpoint);
        }
        input::Advance::Done(draft) => finish_setup(app, draft),
    }
}

/// Probe an ollama endpoint ASYNCHRONOUSLY: a spawned task owns a cloned active
/// kernel, creates the throwaway `ollama_probe` agent (any `ollama_backend.tools`
/// agent serves the verb), sends `list_models` with the endpoint override, **reaps the
/// prober**, and reports back through `onboard_tx` → the `select!` loop — the ONLY
/// place the result mutates UI state. The probe doubles as the reachability check:
/// `{models}` advances, `{error}` reconnects.
///
/// All three verbs ride the QUIET [`crate::kernels::ActiveKernel::send`] (module rule):
/// they are plumbing, and plumbing is not a chat line. The prober is torn down on BOTH
/// outcomes — a throwaway that outlived its probe would show up as a node in the user's
/// hub, which is the same leak in another window.
fn start_probe(app: &mut App, endpoint: String) {
    app.probe_gen += 1;
    let gen = app.probe_gen;
    let active = app.active.clone();
    let tx = app.onboard_tx.clone();
    tokio::spawn(async move {
        // Idempotent: a re-create on an existing prober is harmless, and any error
        // here surfaces through the `list_models` reply below rather than silently.
        let _ = active
            .send(
                &AgentId::from("kernel"),
                json!({
                    "type": "create_agent",
                    "handler_module": "ollama_backend.tools",
                    "id": OLLAMA_PROBE_ID,
                }),
            )
            .await;
        let reply = active
            .send(
                &AgentId::from(OLLAMA_PROBE_ID),
                json!({"type": "list_models", "endpoint": endpoint}),
            )
            .await;
        // Reap it the moment the reply is in hand — reachable or not, the prober never
        // outlives the probe (nothing to see in the tree, nothing to clean up later).
        let _ = active
            .send(
                &AgentId::from("kernel"),
                json!({"type": "delete_agent", "id": OLLAMA_PROBE_ID}),
            )
            .await;
        let res = match reply.get("models").and_then(Value::as_array) {
            Some(arr) => OnboardEvent::Models(
                gen,
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect(),
            ),
            None => OnboardEvent::ProbeError(
                gen,
                reply
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("ollama: unreachable")
                    .to_string(),
            ),
        };
        let _ = tx.send(res);
    });
}

/// Fold an endpoint-probe result into the flow. A reachable endpoint advances to the
/// live-autocomplete Model step; an unreachable one (or an endpoint with no models)
/// surfaces an inline error and **returns to the Port step** — reconnect, never a hang
/// and never a half-set connector. Stale results (a cancelled/restarted flow) are
/// dropped on the generation guard.
fn on_probe_result(app: &mut App, gen: u64, outcome: Result<Vec<String>, String>) {
    if gen != app.probe_gen {
        return; // a stale probe from a flow that was cancelled or restarted
    }
    let Some(flow) = app.flow.as_mut() else {
        return;
    };
    if !flow.is_probing() {
        return;
    }
    // A reachable endpoint with ZERO models is still a dead end (nothing to pick),
    // so it reconnects with a clear reason rather than an empty picker.
    let err = match outcome {
        Ok(models) if models.is_empty() => Some("no models installed at this endpoint".to_string()),
        Ok(models) => {
            app.control = flow.probe_succeeded(models);
            return;
        }
        Err(e) => Some(e),
    };
    app.control = flow.probe_failed();
    if let Some(e) = err {
        app.tabs.deliver("ai").push(
            "brain",
            "you",
            Body::Text(format!(
                "{e}\n↩ reconnect — check the port / that ollama is running."
            )),
            State::Error,
        );
    }
}

/// Persist a completed connector draft (settings + OS keychain + the ollama
/// `ai.endpoint`), update the live env, re-provision the brain (drop the old one),
/// and confirm in the `@ai` agent.
fn finish_setup(app: &mut App, draft: input::Draft) {
    app.control = input::Control::Chat;
    app.flow = None;
    app.add_agent_picker = false;
    let key = (!draft.key.trim().is_empty()).then_some(draft.key.as_str());
    // The chosen endpoint is persisted so `:11434` is never re-assumed; `None`
    // CLEARS any stale one (switching to a keyed provider).
    let endpoint = (!draft.endpoint.trim().is_empty()).then_some(draft.endpoint.as_str());
    let mut seal = State::Done;
    let note = match fantastic_host::set_ai_connector(&draft.backend, &draft.model, endpoint, key) {
        Ok(()) => {
            // Update the live env so the re-provisioned brain reads the new config.
            std::env::set_var("FANTASTIC_AI_BACKEND", &draft.backend);
            std::env::set_var("FANTASTIC_AI_MODEL", &draft.model);
            if let Some(k) = key {
                std::env::set_var("FANTASTIC_AI_KEY", k);
            }
            // `ensure_brain` threads the endpoint from here into the brain's
            // create_agent; clear it when the provider carries none.
            match endpoint {
                Some(e) => std::env::set_var("FANTASTIC_OLLAMA_ENDPOINT", e),
                None => std::env::remove_var("FANTASTIC_OLLAMA_ENDPOINT"),
            }
            // Re-provision: drop the old brain + history so the next `@ai`
            // re-ensures with the new model/key/endpoint (a stale brain keeps the old).
            app.brain_ready = false;
            let kernel = Arc::clone(&app.kernel);
            tokio::spawn(async move {
                for id in [BRAIN_ID, "ai_fs"] {
                    let _ = kernel
                        .send(
                            &AgentId::from("kernel"),
                            json!({"type":"delete_agent","id":id}),
                        )
                        .await;
                }
            });
            match (endpoint.is_some(), draft.port.as_str()) {
                (true, port) if !port.is_empty() => format!(
                    "connector set: {} · {} @ :{} — @ai is ready.",
                    draft.backend, draft.model, port
                ),
                _ => format!(
                    "connector set: {} · {} — @ai is ready.",
                    draft.backend, draft.model
                ),
            }
        }
        Err(e) => {
            seal = State::Error;
            format!("✗ {e}")
        }
    };
    app.tabs
        .deliver("ai")
        .push("brain", "you", Body::Text(note), seal);
    app.tabs.focus("ai");
    app.composer.set_sender("ai");
}

// ── The authored JS agents (connector · mind) + their in-frame editor ───────

/// Open the **add-agent type picker** from the Hub's `+ agent` node: a Select over
/// the composer on the existing flow gate. Only one kind now — an AI backend.
pub(crate) fn open_add_agent_picker(app: &mut App) {
    app.phase = Phase::Chat;
    app.scroll = 0;
    app.tabs.focus("ai");
    app.composer.set_sender("ai");
    app.add_agent_picker = true;
    app.flow = None;
    app.control = input::Control::Select {
        title: "add an agent under root".to_string(),
        options: vec![KIND_AI.to_string()],
        cursor: 0,
    };
}

// ── Render ──────────────────────────────────────────────────────────────────

/// Max model rows the autocomplete shows at once (it slides to keep the cursor in
/// view) — bounds the bottom region so a 40-model ollama doesn't eat the transcript.
const AUTOCOMPLETE_ROWS: usize = 8;

/// How many rows the chat's bottom region needs for `ctrl`, clamped to `cap`: a Select
/// needs room for its options, the model Autocomplete for its (windowed) matches; the
/// Chat composer and the one-line Field/Probing states take the standard 4.
pub(crate) fn flow_height(ctrl: &input::Control, cap: u16) -> u16 {
    match ctrl {
        input::Control::Select { options, .. } => (options.len() as u16 + 4).clamp(5, cap),
        // title + query + the (windowed) matches + hint.
        input::Control::Autocomplete { options, query, .. } => {
            let n = input::filter_models(options, query)
                .len()
                .clamp(1, AUTOCOMPLETE_ROWS) as u16;
            (n + 5).clamp(5, cap)
        }
        // Chat: 1 star-gap + 3-row input. Field/Probing: title + row + hint.
        input::Control::Chat | input::Control::Field { .. } | input::Control::Probing { .. } => 4,
    }
}

/// Render the active setup-flow control: an arrow-key option list, a (maskable)
/// text field, the live-filtered model autocomplete, or the "probing…" state — a
/// Claude-Code-style dynamic input.
pub(crate) fn render_flow(f: &mut Frame, area: Rect, ctrl: &input::Control) {
    let mag = Color::Indexed(165);
    let dim = Style::default().fg(Color::DarkGray);
    let title_st = Style::default()
        .fg(Color::LightMagenta)
        .add_modifier(Modifier::BOLD);
    match ctrl {
        input::Control::Select {
            title,
            options,
            cursor,
        } => {
            let mut lines = vec![Line::from(Span::styled(format!("⚙ {title}"), title_st))];
            for (i, opt) in options.iter().enumerate() {
                let st = if i == *cursor {
                    Style::default().fg(mag).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                };
                let mark = if i == *cursor { "▸ " } else { "  " };
                lines.push(Line::from(vec![
                    Span::styled(mark, st),
                    Span::styled(opt.clone(), st),
                ]));
            }
            lines.push(Line::from(Span::styled(
                "↑↓ select · enter · esc cancel",
                dim,
            )));
            f.render_widget(Paragraph::new(lines), area);
        }
        input::Control::Field {
            title,
            value,
            masked,
        } => {
            let shown = if *masked {
                "•".repeat(value.chars().count())
            } else {
                value.clone()
            };
            let lines = vec![
                Line::from(Span::styled(format!("⚙ {title}"), title_st)),
                Line::from(vec![
                    Span::styled(" ▸ ", dim),
                    Span::styled(shown, Style::default().fg(Color::White)),
                ]),
                Line::from(Span::styled("enter · esc cancel", dim)),
            ];
            f.render_widget(Paragraph::new(lines), area);
            let cx = area.x + 3 + value.chars().count() as u16;
            f.set_cursor_position((cx.min(area.x + area.width.saturating_sub(1)), area.y + 1));
        }
        // The live model picker: a typed query line that FILTERS the installed
        // models below it (prefix, then substring). ↑↓ move the cursor in the
        // filtered view; Enter picks it.
        input::Control::Autocomplete {
            title,
            options,
            query,
            cursor,
        } => {
            let matches = input::filter_models(options, query);
            let mut lines = vec![Line::from(Span::styled(format!("⚙ {title}"), title_st))];
            lines.push(Line::from(vec![
                Span::styled(" ▸ ", dim),
                Span::styled(query.clone(), Style::default().fg(Color::White)),
                Span::styled("▏", Style::default().fg(mag)),
            ]));
            if matches.is_empty() {
                lines.push(Line::from(Span::styled("   no match", dim)));
            } else {
                // Slide the window so the highlighted match stays visible.
                let sel = (*cursor).min(matches.len() - 1);
                let start = (sel + 1).saturating_sub(AUTOCOMPLETE_ROWS);
                for (i, opt) in matches
                    .iter()
                    .enumerate()
                    .skip(start)
                    .take(AUTOCOMPLETE_ROWS)
                {
                    let st = if i == sel {
                        Style::default().fg(mag).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Gray)
                    };
                    let mark = if i == sel { " ▸ " } else { "   " };
                    lines.push(Line::from(vec![
                        Span::styled(mark, st),
                        Span::styled(opt.clone(), st),
                    ]));
                }
            }
            lines.push(Line::from(Span::styled(
                "↑↓ · type to filter · enter · esc cancel",
                dim,
            )));
            f.render_widget(Paragraph::new(lines), area);
            let cx = area.x + 3 + query.chars().count() as u16;
            f.set_cursor_position((cx.min(area.x + area.width.saturating_sub(1)), area.y + 1));
        }
        // Reaching the endpoint (`list_models`) — transient, no input to take.
        input::Control::Probing { title } => {
            let lines = vec![
                Line::from(Span::styled(format!("⚙ {title}"), title_st)),
                Line::from(Span::styled(
                    " ⟳ probing…",
                    dim.add_modifier(Modifier::ITALIC),
                )),
                Line::from(Span::styled("esc cancel", dim)),
            ];
            f.render_widget(Paragraph::new(lines), area);
        }
        input::Control::Chat => {}
    }
}

#[cfg(test)]
mod tests {
    //! The onboarding feature end-to-end, against a REAL in-proc host kernel through
    //! the REAL key path (`handle_input`) — no tty, no network, no model. The async
    //! doors (probe · `/edit` read) are driven exactly as the app drives them: the
    //! spawned task reports on `onboard_tx`, the test drains it like the `select!`
    //! loop does and hands it to [`on_event`].
    use super::*;
    use crate::e2e::{ctrl, key, test_app, test_app_async, type_line};
    use crate::{handle_input, hub, on_brain_event, transcript_lines, CLIENT_ID};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::mpsc;

    /// The transcript of the ACTIVE agent, flattened to one string.
    fn said(app: &App) -> String {
        transcript_lines(app)
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

    /// The dim activity notes recorded in `id`'s chat (what a leak would show up as).
    fn notes(app: &App, id: &str) -> Vec<String> {
        app.tabs
            .chat(id)
            .map(|t| {
                t.msgs()
                    .iter()
                    .filter_map(|m| match &m.body {
                        Body::Note(n) => Some(n.clone()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Register the TUI's CLIENT inbox on the kernel exactly as `run()` does — this is
    /// the ONE door into the chat: the kernel fans a watched agent's traffic in here,
    /// and the `select!` loop routes it with [`on_brain_event`]. Anything the app does
    /// WITHOUT watching leaves this channel empty.
    fn client_inbox(app: &App) -> tokio::sync::mpsc::Receiver<Value> {
        let (tx, rx) = tokio::sync::mpsc::channel::<Value>(256);
        app.kernel.inboxes.insert(AgentId::from(CLIENT_ID), tx);
        rx
    }

    /// Drain the client inbox and route every event through the REAL loop handler.
    /// Returns how many events arrived (0 = nothing was mirrored to the user at all).
    fn drain(app: &mut App, rx: &mut tokio::sync::mpsc::Receiver<Value>) -> Vec<Value> {
        let mut got = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            got.push(ev.clone());
            on_brain_event(app, ev);
        }
        got
    }

    /// A stand-in ollama endpoint: a listener answering `GET /api/tags` with ollama's
    /// real `{"models":[{"name":…}]}` shape, so the probe's SUCCESS path runs
    /// end-to-end and hermetically (no network, no ollama, no model).
    async fn stub_ollama(models: &[&str]) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the stub endpoint");
        let addr = listener.local_addr().expect("stub endpoint address");
        let body =
            json!({ "models": models.iter().map(|m| json!({"name": m})).collect::<Vec<_>>() })
                .to_string();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        format!("http://{addr}")
    }

    /// The scripted stub's sentinel for "ACCEPT this request and NEVER answer it" — the
    /// brain's `send` then stays in flight, which is exactly the state a cancel has to
    /// find in the task table (a mind blocked inside its own brain call).
    const HANG: &str = "@hang";

    /// A stand-in ollama `/api/chat`: a listener answering each POST with ollama's
    /// real NDJSON shape (`{"message":{"content":…},"done":true}`), taking the next
    /// line of `scripts` per request. So a REAL `ollama_backend` brain runs the whole
    /// send → stream → final path against it, hermetically (no network, no model).
    /// Once the script runs out it repeats a terminal `FINAL:` line (a mind's loop
    /// must not spin on an empty stub). A [`HANG`] line hangs that one request forever.
    async fn stub_chat(scripts: &[&str]) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the stub endpoint");
        let addr = listener.local_addr().expect("stub endpoint address");
        let queue: Arc<std::sync::Mutex<std::collections::VecDeque<String>>> = Arc::new(
            std::sync::Mutex::new(scripts.iter().map(|s| s.to_string()).collect()),
        );
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let queue = Arc::clone(&queue);
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let _ = sock.read(&mut buf).await;
                    let next = queue
                        .lock()
                        .expect("stub script")
                        .pop_front()
                        .unwrap_or_else(|| "FINAL: (script exhausted)".to_string());
                    if next == HANG {
                        // Hold the connection open and never answer: the brain's send
                        // stays in flight until the kernel CANCELS it.
                        std::future::pending::<()>().await;
                    }
                    let body = format!(
                        "{}\n",
                        json!({"model":"stub","message":{"role":"assistant","content":next},"done":true})
                    );
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/x-ndjson\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        format!("http://{addr}")
    }

    /// The agent ids the HUB would draw right now (it lays out `reflect`'s tree — so
    /// this is exactly "what a node in the hub" means).
    async fn hub_agents(app: &App) -> Vec<String> {
        hub::Hub::from_reflect(&app.active.reflect_tree().await)
            .nodes()
            .iter()
            .map(|n| n.agent.clone())
            .collect()
    }

    /// Re-wire `app.onboard_tx` to a channel WE own, so a test can drain the async
    /// onboarding result the way the `select!` loop does (and hand it to `on_event`).
    fn intercept(app: &mut App) -> mpsc::UnboundedReceiver<OnboardEvent> {
        let (tx, rx) = mpsc::unbounded_channel::<OnboardEvent>();
        app.onboard_tx = tx;
        rx
    }

    /// The next onboarding result off the channel (bounded — a hang is a failure, not
    /// a stall). Its arrival also means the task's kernel sends have all completed.
    async fn next_onboard(rx: &mut mpsc::UnboundedReceiver<OnboardEvent>) -> OnboardEvent {
        tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("the onboarding task must report back")
            .expect("the onboarding channel must stay open")
    }

    /// Pump one onboarding event through the loop's handler (bounded — a hang is a
    /// failure, not a stall).
    async fn pump(app: &mut App, rx: &mut mpsc::UnboundedReceiver<OnboardEvent>) {
        let ev = next_onboard(rx).await;
        on_event(app, ev);
    }

    #[test]
    fn hub_adder_enter_opens_the_type_picker() {
        let mut app = test_app();
        app.phase = Phase::Hub;
        // Fly to the `+ agent` node and open it.
        let add = app
            .hub
            .nodes()
            .iter()
            .position(|n| n.is_adder)
            .expect("the hub carries a `+ agent` adder");
        app.hub.sel = add;
        handle_input(&mut app, key(KeyCode::Enter));
        assert!(app.add_agent_picker, "`+ agent` opens the type picker");
        assert_eq!(app.phase, Phase::Chat, "the picker rides the chat frame");
        match &app.control {
            input::Control::Select { options, .. } => {
                assert_eq!(options, &["AI agent".to_string()]);
            }
            other => panic!("expected the type-picker Select, got {other:?}"),
        }
    }

    #[test]
    fn picking_ai_agent_starts_the_connect_flow() {
        let mut app = test_app();
        open_add_agent_picker(&mut app);
        handle_input(&mut app, key(KeyCode::Enter)); // "AI agent" (index 0)
        assert!(!app.add_agent_picker, "the picker is consumed");
        assert!(app.flow.is_some(), "the connect wizard is running");
        match &app.control {
            input::Control::Select { options, .. } => {
                assert_eq!(options[0], "ollama", "the provider list, ollama first")
            }
            other => panic!("expected the provider Select, got {other:?}"),
        }
    }

    /// The full ollama connect flow through the REAL key path: provider → port →
    /// probe → (failure → reconnect to the port) → probe → model autocomplete.
    /// Stops before `Done` — finishing would write the real settings/keychain.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connect_flow_ollama_port_probe_reconnect_then_autocomplete() {
        let mut app = test_app_async().await;
        open_add_agent_picker(&mut app);
        handle_input(&mut app, key(KeyCode::Enter)); // AI agent → provider Select
        handle_input(&mut app, key(KeyCode::Enter)); // ollama → the Port field

        // The port is a CONFIRMED value: prefilled 11434, editable.
        match &app.control {
            input::Control::Field { title, value, .. } => {
                assert_eq!(title, "ollama port");
                assert_eq!(value, "11434");
            }
            other => panic!("expected the ollama port field, got {other:?}"),
        }

        // Submitting the port probes it (async) — the UI shows a probing state.
        handle_input(&mut app, key(KeyCode::Enter));
        assert!(matches!(app.control, input::Control::Probing { .. }));
        let gen = app.probe_gen;

        // A STALE result (an older generation) is dropped, never advancing the flow.
        on_event(
            &mut app,
            OnboardEvent::Models(gen - 1, vec!["ghost".into()]),
        );
        assert!(matches!(app.control, input::Control::Probing { .. }));

        // Unreachable → an inline error and back to the PORT step (reconnect), with
        // no hang and no half-set connector.
        on_event(
            &mut app,
            OnboardEvent::ProbeError(gen, "ollama: can't reach http://127.0.0.1:11434".into()),
        );
        match &app.control {
            input::Control::Field { title, value, .. } => {
                assert_eq!(title, "ollama port");
                assert_eq!(value, "11434", "prefilled with the port that was tried");
            }
            other => panic!("a failed probe must return to the port step, got {other:?}"),
        }
        assert!(
            said(&app).contains("reconnect"),
            "an inline reconnect hint: {}",
            said(&app)
        );

        // Re-submit the port → a fresh probe; this one reaches the endpoint.
        handle_input(&mut app, key(KeyCode::Enter));
        assert!(matches!(app.control, input::Control::Probing { .. }));
        let gen = app.probe_gen;
        on_event(
            &mut app,
            OnboardEvent::Models(gen, vec!["gemma3:12b".into(), "gemma3:4b".into()]),
        );
        // The Model step is a LIVE autocomplete over the installed models.
        match &app.control {
            input::Control::Autocomplete { options, .. } => assert_eq!(options.len(), 2),
            other => panic!("expected the model autocomplete, got {other:?}"),
        }
        // Typing filters it (and Enter would pick the highlighted match).
        handle_input(&mut app, key(KeyCode::Char('4')));
        assert_eq!(app.control.submitted_value(), "gemma3:4b");
    }

    /// A reachable endpoint with NO models is still a dead end → reconnect, never an
    /// empty picker you can't escape.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn probe_with_zero_models_reconnects_to_the_port() {
        let mut app = test_app_async().await;
        open_add_agent_picker(&mut app);
        handle_input(&mut app, key(KeyCode::Enter)); // AI agent
        handle_input(&mut app, key(KeyCode::Enter)); // ollama
        handle_input(&mut app, key(KeyCode::Enter)); // port → probe
        let gen = app.probe_gen;
        on_event(&mut app, OnboardEvent::Models(gen, vec![]));
        assert!(
            matches!(&app.control, input::Control::Field { title, .. } if title == "ollama port"),
            "zero models reconnects to the port step"
        );
    }

    /// **Ctrl+C interrupts an `@ai` turn again.** The brain's `interrupt` verb is GONE;
    /// the key now takes the same `ps` → `cancel` path as the plan panel (design 27 ·
    /// [`crate::plan`]). Here the blocked caller is the TUI ITSELF (there is no driver
    /// agent), so the target is the brain's own in-flight dispatch: killing it resolves
    /// our `send` to `{"cancelled":true}` and the task leaves the table.
    /// Seed a stub `@ai` brain (an ollama backend pointed at a fake endpoint) with a
    /// history bridge, so a test can drive an in-flight AI turn. The turn never
    /// completes (stub), so no gated stream/persist send is exercised.
    async fn seed_brain(app: &App, endpoint: &str) {
        let dir = std::env::temp_dir().join(format!(
            "fantastic-tui-brain-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("the brain's history dir");
        let fs = app
            .kernel
            .send(
                &AgentId::from("kernel"),
                json!({
                    "type": "create_agent",
                    "handler_module": "file_bridge.tools",
                    "id": "ai_fs",
                    "root": dir.to_string_lossy(),
                    "ingress_rule": "allow_all_ingress",
                }),
            )
            .await;
        assert!(fs.get("error").is_none(), "seed the history bridge: {fs}");
        let brain = app
            .kernel
            .send(
                &AgentId::from("kernel"),
                json!({
                    "type": "create_agent",
                    "handler_module": "ollama_backend.tools",
                    "id": BRAIN_ID,
                    "model": "stub-model",
                    "endpoint": endpoint,
                    "file_bridge_id": "ai_fs",
                }),
            )
            .await;
        assert!(brain.get("error").is_none(), "seed the brain: {brain}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ctrl_c_cancels_the_in_flight_ai_turn() {
        let mut app = test_app_async().await;
        let mut onb = intercept(&mut app);
        let endpoint = stub_chat(&[HANG]).await; // the brain call never answers
        seed_brain(&app, &endpoint).await;
        app.brain_ready = true; // the brain is provisioned (seeded above)

        crate::fire_ai_turn(&mut app, "think about this forever".to_string());
        // The turn is in flight — the task table has the brain's dispatch.
        let mut rows: Vec<Value> = Vec::new();
        for _ in 0..200 {
            let ps = app
                .kernel
                .send(&AgentId::from("kernel"), json!({"type":"ps"}))
                .await;
            rows = ps
                .get("tasks")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if rows.iter().any(|r| r["agent"] == json!(BRAIN_ID)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            rows.iter().any(|r| r["agent"] == json!(BRAIN_ID)),
            "an @ai turn must be in flight before Ctrl+C: {rows:#?}"
        );

        handle_input(&mut app, ctrl(KeyCode::Char('c')));
        assert!(
            !app.chat_busy,
            "the turn is over as far as the UI is concerned"
        );
        let killed = match next_onboard(&mut onb).await {
            OnboardEvent::Cancelled(driver, killed) => {
                assert_eq!(
                    driver, "",
                    "a bare @ai turn has no driver agent — the TUI is"
                );
                killed
            }
            _ => panic!("Ctrl+C must report a cancel, not a probe"),
        };
        assert!(
            killed.iter().any(|t| t.starts_with("brain:")),
            "Ctrl+C kills the brain's in-flight dispatch: {killed:?}"
        );
        // …and the kernel's table proves it: nothing of that turn is left running.
        let ps = app
            .kernel
            .send(&AgentId::from("kernel"), json!({"type":"ps"}))
            .await;
        let live: Vec<&Value> = ps["tasks"]
            .as_array()
            .expect("tasks")
            .iter()
            .filter(|r| r["agent"] == json!(BRAIN_ID))
            .collect();
        assert!(
            live.is_empty(),
            "the brain call is gone from the table: {live:#?}"
        );
    }

    /// **Internal plumbing is never a user-facing chat.** The leak this guards: the
    /// wizard's endpoint probe dispatched `list_models` through the WATCHED door, so the
    /// kernel's watcher fanout mirrored the raw request payload straight back into the
    /// client inbox — and it rendered in `@ai` as a dim `[list_models] {"type":…}` note.
    /// The `/edit` record read (`get`) and the connector save are the same class.
    ///
    /// The discriminator is the SUBSCRIPTION, not the verb name: this module dispatches
    /// through the quiet `ActiveKernel::send` (no watch → no mirror → nothing to
    /// render), while a user-invoked `@<id> <verb>` still watches + renders its activity
    /// note — even when the user types the very same verb.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn internal_plumbing_is_invisible_while_user_commands_still_render() {
        let mut app = test_app_async().await;
        // The ONE door into the chat (what the `select!` loop drains).
        let mut inbox = client_inbox(&app);
        let mut onb = intercept(&mut app);

        // ── The endpoint probe (create_agent + list_models + delete_agent) …
        let endpoint = stub_ollama(&["gemma3:12b"]).await;
        start_probe(&mut app, endpoint);
        pump(&mut app, &mut onb).await; // it reported back → its kernel sends are done

        // NOT ONE event reached the UI event stream — none of that was the user's.
        let leaked = drain(&mut app, &mut inbox);
        assert!(
            leaked.is_empty(),
            "onboarding plumbing must never mirror into the chat, got: {leaked:?}"
        );
        assert!(
            !notes(&app, "ai").iter().any(|n| n.starts_with('[')),
            "no activity note from internal plumbing: {:?}",
            notes(&app, "ai")
        );

        // ── A USER-invoked command with the SAME verb still renders its activity note.
        app.tabs.focus("ai");
        type_line(&mut app, "@core list_models");
        let ev = tokio::time::timeout(Duration::from_secs(10), inbox.recv())
            .await
            .expect("a user's command mirrors its dispatch into the chat")
            .expect("the client inbox stays open");
        on_brain_event(&mut app, ev);
        assert!(
            notes(&app, "ai")
                .iter()
                .any(|n| n.starts_with("[list_models]")),
            "`@core list_models` still renders its dim activity note: {:?}",
            notes(&app, "ai")
        );
    }

    /// The throwaway prober is plumbing too — it never outlives its probe. BOTH outcomes
    /// (a reachable endpoint and a dead one) leave the tree exactly as they found it, so
    /// `ollama_probe` is never a node in the user's hub.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_prober_never_outlives_the_probe() {
        let mut app = test_app_async().await;
        let mut onb = intercept(&mut app);
        let probe_id = OLLAMA_PROBE_ID.to_string();
        assert!(!hub_agents(&app).await.contains(&probe_id));

        // SUCCESS: a reachable endpoint → the installed models come back…
        let endpoint = stub_ollama(&["gemma3:12b", "smollm2:135m"]).await;
        start_probe(&mut app, endpoint);
        match next_onboard(&mut onb).await {
            OnboardEvent::Models(_, models) => {
                assert_eq!(models, ["gemma3:12b", "smollm2:135m"], "the live /api/tags")
            }
            OnboardEvent::ProbeError(_, e) => panic!("the stub endpoint is reachable: {e}"),
            OnboardEvent::Cancelled(..) => panic!("a probe reports models, nothing else"),
        }
        // …and the prober is already gone (reaped before the result was reported).
        assert!(
            !hub_agents(&app).await.contains(&probe_id),
            "the prober is reaped on success — never a node in the hub"
        );

        // FAILURE: an unreachable endpoint reconnects — and still leaves nothing behind.
        start_probe(&mut app, "http://127.0.0.1:1".to_string());
        assert!(
            matches!(next_onboard(&mut onb).await, OnboardEvent::ProbeError(..)),
            "a dead endpoint errors"
        );
        assert!(
            !hub_agents(&app).await.contains(&probe_id),
            "the prober is reaped on failure too"
        );
    }
}
