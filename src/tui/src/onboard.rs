//! **Add an agent** — the whole onboarding feature in ONE module: the connector
//! wizard (`/setup` · `/model`, design `20_provider_onboarding.md`), the add-agent
//! type picker + the two authored JS agents it seeds (a custom connector · a
//! plan/execute MIND) with their in-frame editor (design
//! `28_add_agent_connectors.md`), and `/edit` — reopening an existing script agent's
//! code to read + edit it in place.
//!
//! Feature-shaped, like `question_card` / `editor`: its state transitions, its async
//! kernel work, its keys AND its render live here. `lib.rs` keeps only thin wiring —
//! the `App` fields, one `select!` arm ([`on_event`]), the key routes
//! ([`handle_flow_key`] · [`handle_editor_key`]) and the render calls
//! ([`render_flow`] · [`flow_height`]).
//!
//! **Async discipline** (one rule, no exceptions): kernel work never runs inline. A
//! spawned task owns a cloned [`crate::kernels::ActiveKernel`] and reports back through
//! `app.onboard_tx` → the `select!` loop → [`on_event`], the ONLY place a result mutates
//! UI state. Both async doors ride that single channel: the ollama endpoint probe
//! (`list_models`) and the `/edit` record read (`get`).
//!
//! **Invisibility discipline** (the second rule): this module's kernel work is PLUMBING,
//! never a chat. Every dispatch here goes through the quiet
//! [`crate::kernels::ActiveKernel::send`], NOT `dispatch` — `dispatch` subscribes the
//! client inbox to the target (that watcher fanout is how a user's `@<id> <verb>` renders
//! its live activity), and the wizard's `create_agent` / `list_models` / `get` /
//! `update_agent` / `start` are not the user's commands. What the user sees of this
//! module is only what it pushes deliberately: the wizard controls and its own notes.
//! The throwaway prober is reaped for the same reason — plumbing leaves no node behind.
//!
//! **Reading an agent's code**: `reflect` deliberately only exposes `has_code` (a
//! program is not a summary), so `/edit` reads the RECORD instead — the kernel's
//! reserved `get` verb returns the whole `AgentRecord` with its `meta` serde-flattened,
//! so a script agent's durable `code` comes back verbatim at the top level.

use fantastic_kernel::AgentId;
use ratatui::crossterm::event::{KeyCode, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::chat::{Body, State};
use crate::{editor, input, App, Phase, BRAIN_ID};

/// The throwaway ollama agent used to probe an endpoint's `/api/tags` (created on
/// demand; any `ollama_backend.tools` agent serves `list_models`).
const OLLAMA_PROBE_ID: &str = "ollama_probe";

/// The starter code seeded into a new **custom JS connector** (design 28): an
/// `on_message` loop that receives turns and is TODO'd to call a provider, then
/// emits `say` + `done`. Authored in-frame (or handed to `@sh` for a coder). Kept
/// verbatim — the editor opens on exactly this.
pub(crate) const CONNECTOR_TEMPLATE: &str = r#"// Your connector — receive turns, call a provider, emit say + done.
// Author it however you like (or drop into @sh and let a coder write it).
// Every emit carries `source: kernel.self_id` — that is what makes it render in
// THIS agent's chat (an event with no source is attributed to the brain).
const kernel = require('kernel');
async function main() {
  while (true) {
    const m = await kernel.on_message();
    if (m === null) break;
    if (m.type !== 'send') continue;
    const text = m.text || '';
    // TODO: call your provider with `text`, then reply below.
    const reply = '(stub connector) you said: ' + text;
    kernel.emit({ type: 'say', text: reply, source: kernel.self_id, client_id: m.client_id });
    kernel.emit({ type: 'done', source: kernel.self_id, client_id: m.client_id });
  }
}
main().catch(e => kernel.emit({ type: 'say', text: 'connector error: ' + String(e), source: kernel.self_id }));"#;

/// The starter code seeded into a new **plan/execute mind** (design 28 · 27): an
/// authored JS agent that DRIVES the connected AI brain through a plan → execute loop
/// and NARRATES it, so the loop is neither a black box nor uninterruptible. Everything
/// the TUI renders, the mind EMITS — the shell hard-codes no loop:
///
/// - `{type:"plan", steps:[…]}` → the live plan panel ([`crate::plan`]),
/// - `{type:"step", index, total, text, status}` → the panel advances,
/// - `{type:"question", …}` → the live question card ([`crate::question_card`]), the
///   answer coming back as the mind's next `on_message`,
/// - `{type:"say"}` + `{type:"done"}` → the answer, and the end of the turn.
///
/// The model's half of the convention is deliberately **one line, never JSON**:
/// `PLAN:` / `ASK:` / `FINAL:`, with the ASK options pipe-separated. That is not a
/// style choice — a live gemma4:12b reliably emits these three forms and reliably
/// BREAKS on nested JSON (it closed an options array with `…"]]}` for `…"}]}`), so
/// the MODEL speaks a pipe format a regex can't get wrong and the MIND builds the
/// well-formed events itself. The **PLAN line is demanded FIRST** (verified live:
/// gemma4:12b answers `PLAN: identify_requirements, evaluate_options, …` when the
/// prompt asks for it explicitly, and otherwise skips straight to `ASK:`).
///
/// **Cancellation.** The mind is BLOCKED inside its `kernel.send('brain', …)`, so it
/// cannot cancel itself — the UI kills that task (`ps` → `cancel`, see [`crate::plan`]),
/// and the kernel resolves the blocked send to `{"cancelled": true}`. Every brain call
/// here funnels through one helper that turns that into a `null`, and every caller of
/// it STOPS on `null`: the loop ends with `(cancelled)` + `done`, and NEVER fabricates
/// a result. The script itself is untouched and takes the next turn.
///
/// Kept verbatim — the editor opens on exactly this, and it is the same code the live
/// test drives against the real model.
pub(crate) const MIND_TEMPLATE: &str = r#"// A plan/execute MIND — it PLANS OUT LOUD, executes step by step, and asks YOU when
// the call is yours. The model only ever replies with ONE line: PLAN: / ASK: / FINAL:
// (no JSON — it breaks on braces). This mind turns those lines into the events the TUI
// renders: PLAN → the live plan panel · ASK → the question card · FINAL → the answer.
// Esc (or Ctrl+C) CANCELS: the kernel kills the brain call this mind is blocked in, the
// send below resolves {cancelled:true}, and the loop stops — no invented result.
// Just talk to it: type a sentence in this chat and it arrives as a `send` turn.
// Every emit carries `source: kernel.self_id` — that is what makes it render HERE.
const kernel = require('kernel');
// Keep the plan SHORT: every step is another full model turn (a local 12b takes tens of
// seconds each), and a plan nobody can wait out is a plan nobody can watch.
const SYSTEM = "You drive a kernel with a plan/execute loop. Reply with EXACTLY ONE line and nothing else - no prose, no JSON, no code fences. The only three forms: 'PLAN: <2 or 3 short comma-separated steps>' or 'ASK: <question> | <option> | <option>' or 'FINAL: <the answer>'. Your FIRST reply to a new task MUST be the PLAN line. Use ASK when the decision is the human's to make.";
let n = 0, cid = null;

// One brain turn. Returns the model's line — or NULL when the kernel CANCELLED this
// call (you pressed Esc): every caller then stops, and nothing is made up.
async function brain(text) {
  const r = await kernel.send('brain', { type: 'send', text: text, system_prompt: SYSTEM, client_id: cid });
  if (r && r.cancelled) return null;
  return ((r && (r.response || r.error)) || '').trim();
}
async function emit(ev) { ev.source = kernel.self_id; ev.client_id = cid; await kernel.emit(ev); }
async function stop(text) { await emit({ type: 'say', text: text }); await emit({ type: 'done' }); }

// ASK: <question> | <option> | <option>  →  the live question card. Suspends until you
// answer; the answer comes back as this agent's next message.
async function ask(line) {
  const p = line.slice(4).split('|').map(x => x.trim()).filter(Boolean);
  n += 1;
  await emit({ type: 'question', request_id: 'q_' + n, kind: 'single', question: p[0],
               options: p.slice(1).map(v => ({ value: v })) });
  const a = await kernel.on_message();          // suspends until you answer
  if (a === null) return null;
  return (a.action && a.action !== 'accept')
    ? 'The human declined to answer.'
    : 'The human answered: ' + a.value + '.';
}

async function turn(text) {
  // ① PLAN FIRST — the loop must never be a black box.
  let s = await brain('Task: ' + text + '\nReply with the PLAN line.');
  for (let t = 0; s !== null && s.indexOf('PLAN:') !== 0 && t < 2; t++) {
    if (s.indexOf('ASK:') === 0) {              // it needs YOU before it can plan
      const a = await ask(s);
      if (a === null) return;
      s = await brain(a + ' Now reply with the PLAN line.');
    } else {
      s = await brain('Reply with the PLAN line for that task now, nothing else.');
    }
  }
  if (s === null) return stop('(cancelled)');
  if (s.indexOf('PLAN:') !== 0) return stop(s.replace(/^FINAL:\s*/, ''));  // it won't plan: say what it said
  const steps = s.slice(5).split(',').map(x => x.trim()).filter(Boolean);
  await emit({ type: 'plan', steps: steps });   // → the live plan panel

  // ② EXECUTE the steps, narrating each one as it runs.
  for (let i = 0; i < steps.length; i++) {
    await emit({ type: 'step', index: i, total: steps.length, text: steps[i], status: 'running' });
    let r = await brain('Execute step ' + (i + 1) + ' of the plan: ' + steps[i] +
      '. If the decision is mine to make, use the ASK form. If the task is finished, use the FINAL form.');
    if (r === null) return stop('(cancelled)');
    // It may ask — but a step that keeps asking is a loop: answer at most twice, then
    // it must produce a result (a small model will otherwise ask forever).
    for (let k = 0; k < 2 && r.indexOf('ASK:') === 0; k++) {
      const a = await ask(r);
      if (a === null) return;
      r = await brain(a + ' Continue with step ' + (i + 1) + ': ' + steps[i] +
        '. Do NOT use the ASK form again for this step - give the result, or the FINAL line if the task is finished.');
      if (r === null) return stop('(cancelled)');
    }
    await emit({ type: 'step', index: i, total: steps.length, text: steps[i], status: 'done' });
    if (r.indexOf('FINAL:') === 0) return stop(r.replace(/^FINAL:\s*/, ''));
  }
  // ③ Every step is done — close the turn with the answer.
  const f = await brain('All steps are done. Reply with the FINAL line.');
  if (f === null) return stop('(cancelled)');
  return stop(f.replace(/^FINAL:\s*/, ''));
}

async function main() {
  while (true) {
    const m = await kernel.on_message();
    if (m === null) break;
    if (m.type !== 'send') continue;
    cid = m.client_id;
    await turn(m.text || '');
  }
}
main().catch(e => kernel.emit({ type: 'say', text: 'mind error: ' + String(e), source: kernel.self_id }));"#;

/// The editor's title bar — one file, one agent: `connector.js — <id>`.
const EDITOR_TITLE: &str = "connector.js";

/// The three kinds the `+ agent` picker offers (design 28). The labels are the
/// picker's option strings AND the submitted values it matches on.
const KIND_AI: &str = "AI agent";
const KIND_CONNECTOR: &str = "Custom connector";
const KIND_MIND: &str = "Plan/execute mind";

/// A fresh custom-connector agent id — `conn_` + 6 hex chars from `seed`. Pure +
/// unit-tested; the caller seeds it from a time-derived value.
pub(crate) fn conn_id(seed: u64) -> String {
    format!("conn_{:06x}", seed & 0x00ff_ffff)
}

/// A fresh plan/execute-mind agent id — `mind_` + 6 hex chars from `seed`. Same
/// shape as [`conn_id`]; the PREFIX is what tells the two authored kinds apart in
/// the hub / the palette.
pub(crate) fn mind_id(seed: u64) -> String {
    format!("mind_{:06x}", seed & 0x00ff_ffff)
}

/// A time-derived seed for a fresh agent id (the low bits of "now").
fn now_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
        .unwrap_or(0)
}

/// Async results from the onboarding tasks, delivered back into the `select!` loop
/// (see the module async discipline). One channel for both doors.
pub(crate) enum OnboardEvent {
    /// The ollama endpoint is reachable — its installed model names. Tagged with the
    /// probe generation so a stale result (from a cancelled/restarted flow) is dropped.
    Models(u64, Vec<String>),
    /// The endpoint is unreachable / errored — the message names the URL.
    ProbeError(u64, String),
    /// `/edit` — the agent's record came back: `(agent id, its durable `code`)`. `None`
    /// when the record carries no (non-empty) `code`, i.e. it isn't a script agent.
    Code(String, Option<String>),
    /// A freshly-seeded mind asked whether the brain it drives exists yet:
    /// `(the mind's id, is the brain provisioned?)`. Only the `false` case is
    /// user-visible (a note telling them to connect one first) — but BOTH are
    /// reported, so the answer is never a silent absence.
    Brain(String, bool),
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
        OnboardEvent::Code(id, code) => on_code(app, id, code),
        OnboardEvent::Brain(mind, present) => on_brain_probe(app, mind, present),
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
                    KIND_CONNECTOR => start_custom_connector(app),
                    KIND_MIND => start_plan_execute_mind(app),
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
/// the composer (`AI agent` · `Custom connector` · `Plan/execute mind`) on the
/// existing flow gate.
pub(crate) fn open_add_agent_picker(app: &mut App) {
    app.phase = Phase::Chat;
    app.scroll = 0;
    app.tabs.focus("ai");
    app.composer.set_sender("ai");
    app.add_agent_picker = true;
    app.flow = None;
    app.control = input::Control::Select {
        title: "add an agent under root".to_string(),
        options: vec![
            KIND_AI.to_string(),
            KIND_CONNECTOR.to_string(),
            KIND_MIND.to_string(),
        ],
        cursor: 0,
    };
}

/// `+ agent → Custom connector`: seed a durable JS agent under the root
/// (`js_runtime.tools`, its `code` is a script run top to bottom) with the starter
/// open the in-frame editor on it. Editing + saving is just the universal `send`.
fn start_custom_connector(app: &mut App) {
    let id = conn_id(now_seed());
    seed_script_agent(app, &id, CONNECTOR_TEMPLATE);
    app.tabs.deliver(&id).push(
        "system",
        "you",
        Body::Note(format!(
            "{id}: a JS connector agent — its `code` is a script, run top to bottom. ^S saves it live · /edit reopens it."
        )),
        State::Done,
    );
}

/// `+ agent → Plan/execute mind`: the SAME door as the custom connector (a durable
/// `js_runtime` agent seeded with a template + the editor open on it) — a different
/// template and a different id prefix, nothing more. What makes it a *mind* is the
/// code: it drives the connected `brain` through a plan → execute loop and, when the
/// decision is the human's, emits a `{type:"question"}` event that the TUI renders as
/// the live question card (design 27); the answer returns as its next `on_message`.
///
/// A mind is only half a system without a brain to drive, so the seed also **probes
/// for one** (async, on the module's discipline) and says so plainly when there is
/// none yet — the `send` to a missing `brain` would otherwise just come back as an
/// error string the mind dutifully says out loud.
fn start_plan_execute_mind(app: &mut App) {
    let id = mind_id(now_seed());
    seed_script_agent(app, &id, MIND_TEMPLATE);
    app.tabs.deliver(&id).push(
        "system",
        "you",
        Body::Note(format!(
            "{id}: a plan/execute MIND — it drives @ai's brain and ASKS you (a live question card) when the call is yours. Just TYPE A SENTENCE here to give it a turn · ^S saves its code live · /edit reopens it."
        )),
        State::Done,
    );
    probe_brain(app, id);
}

/// Seed a durable JS script agent under the root (`js_runtime.tools` — its `code` IS
/// the program, `boot`-rehydrated) with `code`, and open the in-frame editor on it.
/// The one seam both authored kinds go through; the QUIET `send` (module rule) —
/// creating an agent is plumbing, not a chat.
///
/// It also joins `app.scripts` at once: a script agent TAKES PROSE (design 12 · 14), and
/// the agent you just authored must be speakable the moment its editor closes — not
/// after the next reflect of the tree.
fn seed_script_agent(app: &mut App, id: &str, code: &str) {
    app.control = input::Control::Chat;
    if !app.scripts.iter().any(|s| s == id) {
        app.scripts.push(id.to_string());
    }
    let active = app.active.clone();
    let create = json!({
        "type": "create_agent",
        "handler_module": "js_runtime.tools",
        "id": id,
        "code": code,
    });
    tokio::spawn(async move {
        active.send(&AgentId::from("kernel"), create).await;
    });
    // Open its chat + the editor, pre-filled with the starter it was seeded with.
    open_editor(app, id, code);
    app.hub_dirty = true; // the tree gained a node
}

/// Ask the kernel whether the `brain` a fresh mind drives exists yet (a quiet
/// `reflect`: an error means it isn't provisioned). ASYNC on the module's discipline
/// — the answer rides `onboard_tx` → the `select!` loop → [`on_brain_probe`], the
/// only place it touches the UI.
fn probe_brain(app: &mut App, mind: String) {
    let active = app.active.clone();
    let tx = app.onboard_tx.clone();
    tokio::spawn(async move {
        let probe = active
            .send(&AgentId::from(BRAIN_ID), json!({"type": "reflect"}))
            .await;
        let present = probe.get("error").is_none();
        let _ = tx.send(OnboardEvent::Brain(mind, present));
    });
}

/// The brain probe for a fresh mind came back. With a brain: nothing to say (the
/// mind is ready to drive it). Without: a note naming the two steps that give it one
/// — the mind is inert until then, and a silent error string is a worse answer.
fn on_brain_probe(app: &mut App, mind: String, present: bool) {
    if present {
        return;
    }
    app.tabs.deliver(&mind).push(
        "system",
        "you",
        Body::Note(
            "no brain yet — connect one first (`+ agent → AI agent`), take one `@ai` turn to provision it, and this mind will drive it.".to_string(),
        ),
        State::Done,
    );
}

/// `/edit` — **reopen an existing connector in place**: read the ACTIVE agent's code
/// off its record and open the editor on it. ASYNC, on the same discipline as the
/// probe (spawned task + cloned kernel → `onboard_tx` → the `select!` loop).
///
/// The read is the kernel's reserved **`get`** verb: it returns the whole
/// `AgentRecord` with its `meta` flattened, so a script agent's durable `code` sits at
/// the top level. (`reflect` deliberately says only `has_code` — a program is not a
/// summary — so it can't serve this.) An agent with no `code` isn't a script/connector
/// agent: [`on_code`] answers with a note rather than opening an empty editor.
pub(crate) fn start_edit(app: &mut App) {
    let id = app.tabs.active_id().to_string();
    let active = app.active.clone();
    let tx = app.onboard_tx.clone();
    tokio::spawn(async move {
        let record = active
            .send(&AgentId::from("kernel"), json!({"type":"get","id": id}))
            .await;
        let code = record
            .get("code")
            .and_then(Value::as_str)
            .filter(|c| !c.trim().is_empty())
            .map(str::to_string);
        let _ = tx.send(OnboardEvent::Code(id, code));
    });
}

/// The `/edit` record read came back: with `code`, open the editor pre-filled with it
/// (read + edit in place — saving takes the same durable path as authoring); without,
/// a dim note saying what `/edit` is for. Never an error — `/edit` on `@ai` is a
/// mis-aim, not a failure.
fn on_code(app: &mut App, id: String, code: Option<String>) {
    // An editor is already up (a second `/edit` fired before this read landed, or a
    // connector was authored meanwhile): the open buffer wins. Re-seeding it here would
    // silently discard whatever has been typed into it.
    if app.editor.is_some() {
        return;
    }
    match code {
        Some(code) => {
            open_editor(app, &id, &code);
            app.tabs.deliver(&id).push(
                "system",
                "you",
                Body::Note(format!("editing {id} — ^S saves it live · esc closes")),
                State::Done,
            );
        }
        None => {
            app.tabs.deliver(&id).push(
                "system",
                "you",
                Body::Note(format!(
                    "/edit opens a script agent's code — {id} carries none (add one from the hub: `+ agent → Custom connector`)"
                )),
                State::Done,
            );
        }
    }
}

/// Open the in-frame editor on `id`'s `code`, in that agent's chat — the one door
/// both authoring (`+ agent → Custom connector`) and re-editing (`/edit`) go through.
fn open_editor(app: &mut App, id: &str, code: &str) {
    app.phase = Phase::Chat;
    app.scroll = 0;
    app.tabs.focus(id);
    app.composer.set_sender(id);
    app.editor = Some(editor::CodeEditor::from_code(id, EDITOR_TITLE, code));
}

/// Drive the in-frame code editor from a key. The editor owns the modal logic
/// ([`editor::CodeEditor::on_key`]); this shim only forwards the key and, on
/// `^S`/`Esc`, resolves it. Cancel closes with nothing saved.
///
/// **Save is a `send`** — two of them, because the two halves of "saved" are
/// distinct verbs (design 28 wants BOTH: the code persists AND the connector is
/// live at once):
/// 1. `update_agent {id, code}` to the ROOT — patches `code` onto the record and
///    persists it through the provider, so a later `boot` rehydrates THIS code.
///    (`js_runtime`'s own reflect says it plainly: a `code` arg to `start` runs
///    EPHEMERALLY; only a record write — `create_agent`/`update_agent` — is durable.)
/// 2. `start {code}` to the agent — (re)spawns the script so the edit is live now.
///
/// The same path serves a freshly-authored connector and one reopened with `/edit`.
pub(crate) fn handle_editor_key(app: &mut App, code: KeyCode, mods: KeyModifiers) {
    let Some(ed) = app.editor.as_mut() else {
        return;
    };
    let id = ed.agent_id.clone();
    match ed.on_key(code, mods) {
        // The key edited the buffer — the editor stays open.
        editor::EditorAction::Pending => {}
        editor::EditorAction::Save(src) => {
            app.editor = None;
            let active = app.active.clone();
            let target = id.clone();
            tokio::spawn(async move {
                // Persist FIRST (durable), then run it — so a save is never live-only.
                active
                    .send(
                        &AgentId::from("kernel"),
                        json!({"type": "update_agent", "id": target, "code": src}),
                    )
                    .await;
                active
                    .send(
                        &AgentId::from(target.as_str()),
                        json!({"type": "start", "code": src}),
                    )
                    .await;
            });
            app.tabs.deliver(&id).push(
                "system",
                "you",
                Body::Note(format!("saved → {id} is live with your code")),
                State::Done,
            );
        }
        editor::EditorAction::Cancel => {
            app.editor = None;
            app.tabs.deliver(&id).push(
                "system",
                "you",
                Body::Note("editor closed — nothing saved".to_string()),
                State::Done,
            );
        }
    }
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

    /// A REAL `brain` for a mind to drive — the `ollama_backend` agent bound to a
    /// history file_bridge (in a temp dir) and to `endpoint`. Exactly what
    /// `ensure_brain` provisions after `+ agent → AI agent` + one `@ai` turn, done
    /// directly.
    async fn seed_brain(app: &App, endpoint: &str) {
        let dir = std::env::temp_dir().join(format!(
            "fantastic-tui-mind-{}-{}",
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
                    "ingress_rule": "allow_all",
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
    fn conn_id_is_conn_plus_six_hex() {
        assert_eq!(conn_id(0xab12), "conn_00ab12");
        assert_eq!(conn_id(0x00ff_ffff), "conn_ffffff");
        // Always the same shape, whatever the seed (masked to 24 bits).
        assert_eq!(conn_id(u64::MAX), "conn_ffffff");
        assert_eq!(conn_id(0).len(), "conn_".len() + 6);
    }

    #[test]
    fn connector_template_is_a_runnable_starter() {
        // The seed the editor opens on: an on_message LOOP that receives a turn and
        // closes it with say + done — TODO'd where the provider call goes.
        assert!(CONNECTOR_TEMPLATE.contains("require('kernel')"));
        assert!(CONNECTOR_TEMPLATE.contains("await kernel.on_message()"));
        assert!(CONNECTOR_TEMPLATE.contains("if (m === null) break;"));
        assert!(CONNECTOR_TEMPLATE.contains("type: 'say'"));
        assert!(CONNECTOR_TEMPLATE.contains("type: 'done'"));
        assert!(CONNECTOR_TEMPLATE.contains("TODO"));
    }

    #[test]
    fn mind_id_is_mind_plus_six_hex() {
        assert_eq!(mind_id(0xab12), "mind_00ab12");
        assert_eq!(mind_id(u64::MAX), "mind_ffffff");
        assert_eq!(mind_id(0).len(), "mind_".len() + 6);
        // The PREFIX is what tells the two authored kinds apart.
        assert_ne!(mind_id(1)[..5].to_string(), conn_id(1)[..5].to_string());
    }

    /// The mind template IS the convention (design 27 · 28): it drives the `brain`
    /// with a caller-supplied system prompt, and turns the model's ONE-LINE `ASK:`
    /// into a well-formed `{type:"question"}` event — the model never writes JSON
    /// (a live gemma4:12b breaks on the braces), the MIND builds the event.
    #[test]
    fn mind_template_drives_the_brain_and_asks_in_the_card_shape() {
        // It drives the brain the `+ agent → AI agent` flow provisions.
        assert!(MIND_TEMPLATE.contains("kernel.send('brain'"));
        assert!(MIND_TEMPLATE.contains("system_prompt: SYSTEM"));
        // The model's half: one line, three forms, no JSON.
        assert!(MIND_TEMPLATE.contains("'PLAN:"));
        assert!(MIND_TEMPLATE.contains("'ASK:"));
        assert!(MIND_TEMPLATE.contains("'FINAL:"));
        assert!(MIND_TEMPLATE.contains("no JSON"));
        // The mind's half: the pipe-split ASK becomes the card's event shape…
        assert!(MIND_TEMPLATE.contains("split('|')"));
        assert!(MIND_TEMPLATE.contains("type: 'question'"));
        assert!(MIND_TEMPLATE.contains("kind: 'single'"));
        assert!(MIND_TEMPLATE.contains("source: kernel.self_id"));
        // …and it SUSPENDS on the answer (the human's turn is the next message).
        assert!(MIND_TEMPLATE.contains("await kernel.on_message();          // suspends"));
        assert!(MIND_TEMPLATE.contains("a.action !== 'accept'"));
        // Every field the card demands parses out of the emitted event.
        let ev = json!({
            "type":"question","request_id":"q_1","kind":"single",
            "question":"Which database?","source":"mind_abc123","client_id":"fantastic",
            "options":[{"value":"Postgres"},{"value":"SQLite"}]
        });
        let card = crate::question_card::QuestionCard::from_event(&ev)
            .expect("the shape the template emits IS a renderable card");
        assert_eq!(card.options.len(), 2);
    }

    /// The loop the template runs is VISIBLE and CANCELABLE — the two halves of design
    /// 27's plan/execute contract that the TUI renders but never hard-codes.
    #[test]
    fn mind_template_narrates_its_loop_and_stops_when_cancelled() {
        // The PLAN comes FIRST — demanded of the model, and emitted as the panel event.
        assert!(MIND_TEMPLATE.contains("FIRST reply to a new task MUST be the PLAN line"));
        assert!(MIND_TEMPLATE.contains("Reply with the PLAN line."));
        assert!(MIND_TEMPLATE.contains("type: 'plan', steps: steps"));
        // Each step is narrated before and after it runs.
        assert!(MIND_TEMPLATE.contains("type: 'step', index: i, total: steps.length"));
        assert!(MIND_TEMPLATE.contains("status: 'running'"));
        assert!(MIND_TEMPLATE.contains("status: 'done'"));
        // A CANCELLED brain call comes back as `{cancelled:true}` → the mind stops the
        // loop and says so; it never fabricates a result.
        assert!(MIND_TEMPLATE.contains("if (r && r.cancelled) return null;"));
        assert!(MIND_TEMPLATE.contains("return stop('(cancelled)')"));
        // The events the template emits are exactly the ones the panel renders.
        let panel = crate::plan::PlanPanel::from_event(&json!({
            "type":"plan","steps":["identify_requirements","evaluate_options"],
            "source":"mind_abc123","client_id":"fantastic"
        }))
        .expect("the shape the template emits IS a renderable panel");
        assert_eq!(panel.steps.len(), 2);
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
                assert_eq!(
                    options,
                    &[
                        "AI agent".to_string(),
                        "Custom connector".to_string(),
                        "Plan/execute mind".to_string(),
                    ]
                );
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

    /// `+ agent → Custom connector`: a real `js_runtime` agent is created under the
    /// root, seeded with the starter template, and the in-frame editor opens on it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn custom_connector_seeds_a_js_agent_and_opens_the_editor() {
        let mut app = test_app_async().await;
        open_add_agent_picker(&mut app);
        handle_input(&mut app, key(KeyCode::Down)); // → "Custom connector"
        handle_input(&mut app, key(KeyCode::Enter));

        assert!(!app.add_agent_picker);
        assert!(matches!(app.control, input::Control::Chat));
        let ed = app
            .editor
            .as_ref()
            .expect("the editor opens on the connector");
        let id = ed.agent_id.clone();
        assert!(id.starts_with("conn_"), "a fresh connector id: {id}");
        assert_eq!(
            ed.text(),
            CONNECTOR_TEMPLATE,
            "the editor opens pre-filled with the starter, verbatim"
        );

        // The create_agent lands under the root: a real js_runtime agent whose
        // `code` is a SCRIPT run top to bottom (durable, boot-rehydrated).
        let mut reflect = json!({});
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            reflect = app
                .kernel
                .send(&AgentId::from(id.as_str()), json!({"type":"reflect"}))
                .await;
            if reflect.get("error").is_none() {
                break;
            }
        }
        assert!(
            reflect.get("error").is_none(),
            "the connector agent exists under the root: {reflect}"
        );
        assert_eq!(
            reflect.get("has_code").and_then(Value::as_bool),
            Some(true),
            "seeded with its `code` — a script run top to bottom: {reflect}"
        );
    }

    /// `+ agent → Plan/execute mind`: the third picker option seeds a real
    /// `js_runtime` agent under the root — id `mind_*`, `code` = the plan/execute
    /// template — and opens the same in-frame editor on it. With no brain provisioned
    /// yet, the seed SAYS SO (a mind with nothing to drive is inert).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn plan_execute_mind_seeds_a_js_agent_and_opens_the_editor() {
        let mut app = test_app_async().await;
        let mut onb = intercept(&mut app); // the brain probe reports here
        open_add_agent_picker(&mut app);
        handle_input(&mut app, key(KeyCode::Down)); // → "Custom connector"
        handle_input(&mut app, key(KeyCode::Down)); // → "Plan/execute mind"
        handle_input(&mut app, key(KeyCode::Enter));

        assert!(!app.add_agent_picker);
        assert!(matches!(app.control, input::Control::Chat));
        let ed = app.editor.as_ref().expect("the editor opens on the mind");
        let id = ed.agent_id.clone();
        assert!(id.starts_with("mind_"), "a fresh mind id: {id}");
        assert_eq!(
            ed.text(),
            MIND_TEMPLATE,
            "the editor opens pre-filled with the plan/execute template, verbatim"
        );

        // A real js_runtime agent under the root, its `code` the template (durable).
        let mut reflect = json!({});
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            reflect = app
                .kernel
                .send(&AgentId::from(id.as_str()), json!({"type":"reflect"}))
                .await;
            if reflect.get("error").is_none() {
                break;
            }
        }
        assert!(
            reflect.get("error").is_none(),
            "the mind exists under the root: {reflect}"
        );
        assert_eq!(
            reflect.get("has_code").and_then(Value::as_bool),
            Some(true),
            "seeded with its `code` — the mind is a script run top to bottom: {reflect}"
        );

        // No brain yet → the seed tells the user how to give it one (rather than
        // letting the mind's first `send` come back as an error string).
        pump(&mut app, &mut onb).await;
        let ns = notes(&app, &id);
        assert!(
            ns.iter().any(|n| n.contains("no brain yet")),
            "a mind with no brain says so: {ns:?}"
        );
        assert!(
            ns.iter().any(|n| n.contains("+ agent → AI agent")),
            "…and names the door that provisions one: {ns:?}"
        );
    }

    /// **The whole feature, through the real keys.** `+ agent → Plan/execute mind` →
    /// Esc the editor → **type a SENTENCE at the mind** (prose to a script agent is a
    /// `send` turn — design 12 · 14) → it drives a REAL `brain` (the ollama backend
    /// agent, pointed at a scripted stub endpoint: no network, no model) → its one-line
    /// `ASK:` becomes a `{type:"question"}` event → the kernel's watcher fanout mirrors
    /// it into the client inbox → `on_brain_event` opens the LIVE question card (design
    /// 27) → Enter answers → the mind resumes with the human's answer and finishes.
    ///
    /// And everything the mind SAYS renders in the mind's own chat (its emits carry
    /// `source: kernel.self_id`), never in `@ai`'s.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_seeded_minds_question_opens_the_live_card_and_the_answer_resumes_it() {
        let mut app = test_app_async().await;
        // The brain speaks the mind's convention: plan, then ask, then finish.
        let endpoint = stub_chat(&[
            "PLAN: compare the candidates, then ask the human",
            "ASK: Which database? | Postgres | SQLite",
            "FINAL: go with Postgres",
        ])
        .await;
        seed_brain(&app, &endpoint).await;

        // Seed the mind exactly as the picker does, then Esc out of the editor.
        open_add_agent_picker(&mut app);
        handle_input(&mut app, key(KeyCode::Down));
        handle_input(&mut app, key(KeyCode::Down));
        handle_input(&mut app, key(KeyCode::Enter));
        let id = app
            .editor
            .as_ref()
            .expect("the mind's editor")
            .agent_id
            .clone();
        handle_input(&mut app, key(KeyCode::Esc)); // close the editor, keep the agent
        assert!(app.editor.is_none());
        // Let its script reach its first `on_message` before addressing it: `js_runtime`
        // decides "living tool vs one-shot script" by sampling whether the script is
        // parked on its inbox shortly after spawn, so a turn fired INTO that window
        // would find a mind that is busy-but-not-parked. A human types; a test waits.
        await_running(&app, &id).await;
        let mut inbox = client_inbox(&app);

        // ── The line a user actually types: a SENTENCE, in the mind's chat.
        assert_eq!(
            app.composer.sender, id,
            "the seed leaves you standing in it"
        );
        type_line(&mut app, "Help me pick a database for a new service.");
        assert!(
            said(&app).contains("Help me pick a database for a new service."),
            "the prose is rendered as your line in the mind's chat: {}",
            said(&app)
        );

        // The mind plans, then ASKS — and the ask opens the card.
        let mut seen: Vec<Value> = Vec::new();
        for _ in 0..200 {
            seen.extend(drain(&mut app, &mut inbox));
            if app.question.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            app.question.is_some(),
            "the mind's `question` event must open the live card; saw: {seen:#?}"
        );
        let card = app.question.as_ref().expect("the card");
        assert_eq!(card.source, id, "the card answers back to the asking mind");
        assert_eq!(card.kind, crate::question_card::Kind::Single);
        assert_eq!(card.question, "Which database?");
        assert_eq!(
            card.options
                .iter()
                .map(|o| o.value.as_str())
                .collect::<Vec<_>>(),
            ["Postgres", "SQLite"],
            "the pipe-split ASK became the card's options"
        );
        // The turn reached the script as SPEECH — the whole sentence, not a verb.
        assert!(
            seen.iter().any(|e| e["type"] == "send"
                && e["text"].as_str() == Some("Help me pick a database for a new service.")),
            "the prose line arrived as a `send` turn: {seen:#?}"
        );
        // The plan was NARRATED on the way — as the live panel (design 27), and the
        // question is being asked IN THE CONTEXT of that plan: both are up at once.
        let panel = app
            .plan
            .as_ref()
            .expect("the mind's PLAN line opened the live panel");
        assert_eq!(panel.source, id, "it is the MIND's loop (self-attributed)");
        assert_eq!(
            panel
                .steps
                .iter()
                .map(|s| s.text.as_str())
                .collect::<Vec<_>>(),
            ["compare the candidates", "then ask the human"],
            "the comma-split PLAN line became the panel's steps"
        );
        assert_eq!(
            panel.steps[0].status,
            crate::plan::Status::Running,
            "the step the mind is executing is the one it asked from"
        );
        // The plan is the PANEL now, not a line of chatter — in nobody's transcript.
        assert!(
            !notes(&app, &id).iter().any(|n| n.starts_with("PLAN:")),
            "the plan renders in the panel, not as a note: {:?}",
            notes(&app, &id)
        );
        assert!(
            !notes(&app, "ai").iter().any(|n| n.starts_with("PLAN:")),
            "…and never in @ai's chat: {:?}",
            notes(&app, "ai")
        );

        // Enter accepts the highlighted option → the answer is sent BACK to the mind,
        // which resumes (the human's answer is its next turn) and finishes.
        handle_input(&mut app, key(KeyCode::Enter));
        assert!(app.question.is_none(), "answering closes the card");
        let mut done = false;
        for _ in 0..200 {
            let evs = drain(&mut app, &mut inbox);
            done |= evs
                .iter()
                .any(|e| e["type"] == "say" && e["text"].as_str() == Some("go with Postgres"));
            if done {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            done,
            "the answer resumes the mind — it finishes with the FINAL line"
        );
        // The FINAL line, too, lands in the mind's chat.
        assert!(
            notes(&app, &id).iter().any(|n| n == "go with Postgres"),
            "the mind's FINAL renders in its own chat: {:?}",
            notes(&app, &id)
        );
    }

    /// **CANCEL, through the real key.** A mind is BLOCKED inside its own
    /// `kernel.send('brain', …)` (the stub accepts the request and never answers), so it
    /// can neither cancel itself nor learn its own task id. **Esc** on the live plan
    /// panel therefore does what a shell does: `ps` the kernel's task table → find the
    /// in-flight work → `cancel` it (design 27 · [`crate::plan`]).
    ///
    /// What is asserted is the whole contract:
    /// - the kill lands on the BRAIN CALL (`brain:<n>`), never on the mind itself;
    /// - the mind's blocked `send` resolves `{"cancelled":true}` → it says `(cancelled)`
    ///   and stops the loop, WITHOUT fabricating a result;
    /// - the mind's script SURVIVES — it is still `running` and takes the next turn to
    ///   completion (the proof that the cancel killed the call, not the agent).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn esc_cancels_the_minds_brain_call_and_the_mind_survives() {
        let mut app = test_app_async().await;
        let mut onb = intercept(&mut app); // the brain probe + the cancel report land here
                                           // Turn 1: it plans, then HANGS executing step 1 — the state a cancel must find.
                                           // Turn 2 (after the cancel): it plans and finishes, proving the mind is alive.
        let endpoint = stub_chat(&[
            "PLAN: check the disk, report the result",
            HANG,
            "PLAN: retry the check, report the result",
            "FINAL: all good",
        ])
        .await;
        seed_brain(&app, &endpoint).await;

        open_add_agent_picker(&mut app);
        handle_input(&mut app, key(KeyCode::Down));
        handle_input(&mut app, key(KeyCode::Down));
        handle_input(&mut app, key(KeyCode::Enter));
        let id = app
            .editor
            .as_ref()
            .expect("the mind's editor")
            .agent_id
            .clone();
        handle_input(&mut app, key(KeyCode::Esc)); // close the editor, keep the agent
        await_running(&app, &id).await;
        let mut inbox = client_inbox(&app);

        // ── The turn: the mind plans OUT LOUD, and the panel opens.
        type_line(&mut app, "Check the disk.");
        for _ in 0..200 {
            drain(&mut app, &mut inbox);
            if app.plan.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let panel = app.plan.as_ref().expect("the mind's PLAN opens the panel");
        assert_eq!(panel.source, id, "the panel is the MIND's loop");
        assert_eq!(
            panel
                .steps
                .iter()
                .map(|s| s.text.as_str())
                .collect::<Vec<_>>(),
            ["check the disk", "report the result"],
            "the comma-split PLAN line became the panel's steps"
        );

        // The mind is now BLOCKED in the brain call for step 1 — the task table says so.
        let mut rows: Vec<Value> = Vec::new();
        for _ in 0..200 {
            drain(&mut app, &mut inbox);
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
            "the mind's brain call must be in flight before we cancel it: {rows:#?}"
        );
        assert_eq!(
            app.plan.as_ref().map(|p| p.steps[0].status),
            Some(crate::plan::Status::Running),
            "step 1 is the one in flight"
        );

        // ── Esc: `ps` → `cancel`, exactly like a shell.
        handle_input(&mut app, key(KeyCode::Esc));
        assert_eq!(
            app.plan.as_ref().map(|p| p.state),
            Some(crate::plan::PanelState::Cancelling),
            "the panel says the kill is in flight"
        );
        // The cancel task reports back on the onboard channel (the loop's only door).
        let mut killed: Vec<String> = Vec::new();
        for _ in 0..8 {
            let ev = next_onboard(&mut onb).await;
            if let OnboardEvent::Cancelled(driver, ids) = &ev {
                assert_eq!(driver, &id, "the loop that was stopped is the MIND's");
                killed = ids.clone();
                on_event(&mut app, ev);
                break;
            }
            on_event(&mut app, ev); // the seed's brain probe — not ours
        }
        assert!(
            killed.iter().any(|t| t.starts_with("brain:")),
            "the kill lands on the BRAIN CALL the mind is blocked in: {killed:?}"
        );
        assert!(
            !killed.iter().any(|t| t.starts_with(&format!("{id}:"))),
            "…and NEVER on the mind's own job — that would kill the script: {killed:?}"
        );
        assert_eq!(
            app.plan.as_ref().map(|p| p.state),
            Some(crate::plan::PanelState::Cancelled),
            "the panel shows the cancel"
        );
        assert!(
            notes(&app, &id)
                .iter()
                .any(|n| n.starts_with("⊘ cancelled")),
            "…and the chat names the tasks that died: {:?}",
            notes(&app, &id)
        );

        // ── The mind's blocked `send` resolved `{"cancelled":true}`: it stops the loop
        // and says so — no invented result.
        let mut said_cancelled = false;
        for _ in 0..200 {
            said_cancelled |= drain(&mut app, &mut inbox)
                .iter()
                .any(|e| e["type"] == "say" && e["text"] == json!("(cancelled)"));
            if said_cancelled {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            said_cancelled,
            "the cancelled brain call stops the mind's loop: {:?}",
            notes(&app, &id)
        );

        // ── And the MIND IS STILL ALIVE: its script was never touched. It takes the
        // next turn and runs it to completion.
        let status = app
            .kernel
            .send(&AgentId::from(id.as_str()), json!({"type":"status"}))
            .await;
        assert_eq!(
            status.get("running").and_then(Value::as_bool),
            Some(true),
            "the cancel killed the CALL, not the agent: {status}"
        );
        type_line(&mut app, "Try again.");
        let mut finished = false;
        for _ in 0..200 {
            finished |= drain(&mut app, &mut inbox)
                .iter()
                .any(|e| e["type"] == "say" && e["text"] == json!("all good"));
            if finished {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            finished,
            "a cancelled mind stays alive for the next turn: {:?}",
            notes(&app, &id)
        );
    }

    /// **Ctrl+C interrupts an `@ai` turn again.** The brain's `interrupt` verb is GONE;
    /// the key now takes the same `ps` → `cancel` path as the plan panel (design 27 ·
    /// [`crate::plan`]). Here the blocked caller is the TUI ITSELF (there is no driver
    /// agent), so the target is the brain's own in-flight dispatch: killing it resolves
    /// our `send` to `{"cancelled":true}` and the task leaves the table.
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

    /// **LIVE** — the same mind, the same panel, the same card, against a REAL model.
    /// Ignored by default (it needs a running ollama with `gemma4:12b`, and a turn takes
    /// tens of seconds); it is the gate that says the template's convention actually
    /// holds for the model it was written for — that is why the model speaks ONE LINE
    /// and the MIND builds the JSON (gemma4 emits `PLAN:` / `ASK:` / `FINAL:` reliably
    /// and mangles nested JSON: it closed an options array with `…"]]}`).
    ///
    /// What is ASSERTED is the whole visible loop, end to end:
    /// - the model returns a REAL `PLAN:` line FIRST → the live plan panel opens with
    ///   its steps (the template now demands the plan before anything executes — asking
    ///   for it explicitly is what makes gemma4 plan instead of skipping to `ASK:`),
    /// - executing a step, the model raises a real `ASK:` → the card opens OVER the
    ///   panel with answerable options,
    /// - the human's answer goes back in → the model finishes.
    ///
    ///   cargo test -p fantastic-tui live_mind_asks -- --ignored --nocapture
    #[ignore = "live: needs ollama on :11434 with gemma4:12b (tens of seconds per turn)"]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn live_mind_asks_the_human_through_the_card() {
        let mut app = test_app_async().await;
        seed_live_brain(&app, "http://127.0.0.1:11434", "gemma4:12b").await;

        // Seeded through the picker and driven through the COMPOSER — the exact keys a
        // user presses: `+ agent` → Plan/execute mind → Esc → type the sentence.
        open_add_agent_picker(&mut app);
        handle_input(&mut app, key(KeyCode::Down));
        handle_input(&mut app, key(KeyCode::Down));
        handle_input(&mut app, key(KeyCode::Enter));
        let id = app
            .editor
            .as_ref()
            .expect("the mind's editor")
            .agent_id
            .clone();
        handle_input(&mut app, key(KeyCode::Esc));
        await_running(&app, &id).await;
        let mut inbox = client_inbox(&app);
        type_line(&mut app, "Help me pick a database for a new service.");

        // ── ① The PLAN comes first, and it OPENS THE PANEL.
        // Turns take 20-40s on a local 12b — wait generously (300 × 500ms = 150s).
        let mut seen: Vec<Value> = Vec::new();
        for _ in 0..300 {
            seen.extend(drain(&mut app, &mut inbox));
            if app.plan.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let panel = app
            .plan
            .as_ref()
            .unwrap_or_else(|| panic!("the live model must PLAN first; saw: {seen:#?}"));
        println!(
            "── live PLAN: {:?}",
            panel.steps.iter().map(|s| &s.text).collect::<Vec<_>>()
        );
        assert_eq!(panel.source, id, "the panel is the MIND's loop");
        assert!(
            !panel.steps.is_empty(),
            "a PLAN line the panel can render has steps"
        );

        // ── ② Executing a step, the model ASKS — the card floats over the live panel.
        for _ in 0..600 {
            seen.extend(drain(&mut app, &mut inbox));
            if app.question.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        assert!(
            app.plan.as_ref().is_some_and(|p| p.is_live()),
            "the plan panel is still live UNDER the card — the question is asked in context"
        );
        let card = app
            .question
            .as_ref()
            .unwrap_or_else(|| panic!("the live model must ASK; saw: {seen:#?}"));
        println!(
            "── live ASK: {:?} options={:?}",
            card.question,
            card.options.iter().map(|o| &o.value).collect::<Vec<_>>()
        );
        assert_eq!(card.source, id, "the card answers back to the asking mind");
        assert!(
            card.options.len() >= 2,
            "an ASK the human can answer needs >=2 options: {:?}",
            card.options
        );

        // ── ③ The human answers → the mind resumes with it and the model closes the loop.
        // A real human answers whatever the model raises, so the loop below answers EVERY
        // card that comes up (a 12b executing a step often wants a second decision) — the
        // gate is that the loop CLOSES, not that it asks exactly once.
        handle_input(&mut app, key(KeyCode::Enter));
        assert!(app.question.is_none(), "answering closes the card");
        // Every remaining step is another full model turn (tens of seconds each on a
        // local 12b), so the tail gets a real budget: 1200 × 500ms = 600s.
        let mut final_say: Option<String> = None;
        for _ in 0..1200 {
            for ev in drain(&mut app, &mut inbox) {
                if ev["type"] == "step" {
                    println!(
                        "── live STEP {} {}: {:?}",
                        ev["index"], ev["status"], ev["text"]
                    );
                }
                if ev["type"] == "say" {
                    if let Some(t) = ev["text"].as_str() {
                        final_say = Some(t.to_string());
                    }
                }
            }
            if final_say.is_some() {
                break;
            }
            if let Some(c) = app.question.as_ref() {
                println!("── live ASK (again): {:?}", c.question);
                handle_input(&mut app, key(KeyCode::Enter));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        println!("── live FINAL: {final_say:?}");
        assert!(
            final_say.is_some(),
            "the human's answer resumes the mind — the model finishes with a FINAL line"
        );
        assert_ne!(
            final_say.as_deref(),
            Some("(cancelled)"),
            "nothing cancelled this turn"
        );
    }

    /// A REAL brain against a REAL provider — the live gate's only unhermetic seam.
    async fn seed_live_brain(app: &App, endpoint: &str, model: &str) {
        let dir = std::env::temp_dir().join("fantastic-tui-live-mind");
        std::fs::create_dir_all(&dir).expect("the brain's history dir");
        app.kernel
            .send(
                &AgentId::from("kernel"),
                json!({
                    "type": "create_agent",
                    "handler_module": "file_bridge.tools",
                    "id": "ai_fs",
                    "root": dir.to_string_lossy(),
                    "ingress_rule": "allow_all",
                }),
            )
            .await;
        let brain = app
            .kernel
            .send(
                &AgentId::from("kernel"),
                json!({
                    "type": "create_agent",
                    "handler_module": "ollama_backend.tools",
                    "id": BRAIN_ID,
                    "model": model,
                    "endpoint": endpoint,
                    "file_bridge_id": "ai_fs",
                    "num_ctx": 16384,
                }),
            )
            .await;
        assert!(brain.get("error").is_none(), "seed the live brain: {brain}");
    }

    /// The editor is modal: keys edit it, `^S` saves + closes, `Esc` closes with
    /// nothing saved. The save must be DURABLE — the edited `code` lands on the agent
    /// RECORD (design 28: "a subsequent reflect/boot shows the new code"), not merely
    /// get run once (a `start` `code` arg alone is ephemeral).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn editor_ctrl_s_saves_durably_and_esc_discards() {
        let mut app = test_app_async().await;
        // A real js_runtime connector to edit (what `+ agent → Custom connector` seeds).
        let id = "conn_abc123";
        seed_connector(&app, id, "// seed").await;

        // The editor always opens on its agent's chat (as `start_custom_connector` does).
        app.tabs.focus(id);
        app.editor = Some(editor::CodeEditor::from_code(id, EDITOR_TITLE, "// seed"));
        // A typed char edits the EDITOR, never the composer beneath it.
        handle_input(&mut app, key(KeyCode::Char('x')));
        assert!(
            app.composer.message.is_empty(),
            "keys never leak to the composer"
        );
        assert_eq!(app.editor.as_ref().unwrap().text(), "x// seed");

        // ^S saves + closes.
        handle_input(&mut app, ctrl(KeyCode::Char('s')));
        assert!(app.editor.is_none(), "^S closes the editor");
        assert!(
            said(&app).contains("saved"),
            "the save is confirmed: {}",
            said(&app)
        );

        // …and the edited code is PERSISTED onto the record, so a later boot
        // rehydrates THIS code (the whole point of a durable authored script).
        assert_eq!(
            record_code(&app, id, Some("x// seed")).await.as_deref(),
            Some("x// seed"),
            "^S must PERSIST the edited code onto the agent record (durable)"
        );

        // Esc closes without saving.
        app.editor = Some(editor::CodeEditor::from_code(id, EDITOR_TITLE, "// seed"));
        handle_input(&mut app, key(KeyCode::Esc));
        assert!(app.editor.is_none(), "Esc closes the editor");
    }

    /// `/edit` **reopens an existing connector in place**: the code is read off the
    /// agent RECORD (the reserved `get` verb) and the editor opens pre-filled with it,
    /// verbatim — the door that was missing (you could only author a FRESH connector).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slash_edit_reopens_an_existing_connector_prefilled() {
        let mut app = test_app_async().await;
        let id = "conn_ed1701";
        let code = "// authored earlier\nconst kernel = require('kernel');\n// … my connector";
        seed_connector(&app, id, code).await;
        // Stand in the connector's chat (where `/edit` is aimed) and type it.
        app.tabs.focus(id);
        app.composer.set_sender(id);

        let mut rx = intercept(&mut app);
        type_line(&mut app, "/edit");
        assert!(
            app.composer.message.is_empty(),
            "`/edit` is a command, never routed to the agent as text"
        );
        assert!(
            app.editor.is_none(),
            "the editor opens only once the record READ comes back (async)"
        );
        // The `select!` loop's job: the record read lands → the editor opens.
        pump(&mut app, &mut rx).await;

        let ed = app.editor.as_ref().expect("/edit reopens the editor");
        assert_eq!(ed.agent_id, id);
        assert_eq!(
            ed.text(),
            code,
            "pre-filled with the agent's stored code, verbatim"
        );

        // …and it is the SAME editor: editing + ^S persists back onto the record.
        handle_input(&mut app, key(KeyCode::Char('!')));
        handle_input(&mut app, ctrl(KeyCode::Char('s')));
        assert!(app.editor.is_none(), "^S closes the reopened editor");
        let want = format!("!{code}");
        assert_eq!(
            record_code(&app, id, Some(&want)).await.as_deref(),
            Some(want.as_str()),
            "a reopened editor saves through the same durable path"
        );
    }

    /// `/edit` on an agent that carries no `code` is a mis-aim, not an error: a dim
    /// note explains what it's for and NO editor opens (never an empty buffer that
    /// would overwrite a non-script agent on save).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slash_edit_on_a_codeless_agent_notes_instead_of_opening() {
        let mut app = test_app_async().await;
        // `@ai` is the brain chat — not a script agent, so it has no `code`.
        assert_eq!(app.tabs.active_id(), "ai");

        let mut rx = intercept(&mut app);
        type_line(&mut app, "/edit");
        pump(&mut app, &mut rx).await;

        assert!(
            app.editor.is_none(),
            "no editor opens on an agent with no code"
        );
        let note = said(&app);
        assert!(
            note.contains("/edit") && note.contains("script agent"),
            "a dim note explains what /edit is for: {note}"
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

        // ── …and the `/edit` record read (`get`, on the codeless `@ai`).
        start_edit(&mut app);
        pump(&mut app, &mut onb).await;

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
            OnboardEvent::Code(..) | OnboardEvent::Brain(..) | OnboardEvent::Cancelled(..) => {
                panic!("a probe reports models, nothing else")
            }
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

    /// Seed a real `js_runtime` connector agent with `code` (what
    /// `+ agent → Custom connector` does, done directly).
    async fn seed_connector(app: &App, id: &str, code: &str) {
        let made = app
            .kernel
            .send(
                &AgentId::from("kernel"),
                json!({
                    "type": "create_agent",
                    "handler_module": "js_runtime.tools",
                    "id": id,
                    "code": code,
                }),
            )
            .await;
        assert!(made.get("error").is_none(), "seed the connector: {made}");
    }

    /// Wait until a seeded script agent is LIVE and parked on its inbox: `status`
    /// reports `running`, plus the grace the `js_runtime` driver needs to see the
    /// script waiting for mail (that's the sample that keeps a long-lived tool alive).
    async fn await_running(app: &App, id: &str) {
        for _ in 0..50 {
            let st = app
                .kernel
                .send(&AgentId::from(id), json!({"type":"status"}))
                .await;
            if st.get("running").and_then(Value::as_bool) == Some(true) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    /// The `code` on an agent's RECORD, polled until it equals `want` (the save path
    /// is a spawned task). `get` returns the record with its `meta` serde-FLATTENED, so
    /// the bundle's own keys (here `code`) sit at the top level.
    async fn record_code(app: &App, id: &str, want: Option<&str>) -> Option<String> {
        let mut code = None;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let record = app
                .kernel
                .send(&AgentId::from("kernel"), json!({"type":"get","id":id}))
                .await;
            code = record
                .get("code")
                .and_then(Value::as_str)
                .map(str::to_string);
            if code.as_deref() == want {
                break;
            }
        }
        code
    }
}
