//! A MODEL NEVER RECEIVES A SECRET (owner, 2026-10-02, option B) — the twin of
//! python `ai_core/tests/test_secret_holders.py`.
//!
//! An AI agent reads its key from a credential store as ITSELF, so the same grant
//! would let its MODEL `send` the store's `get` and read the value into its
//! prompt and memory. `holds_secrets: true` on a record makes that agent
//! unreachable from a model turn: [`crate::route`] — the one place a model-parsed
//! call is dispatched — refuses every call whose target or `reply_to` is such an
//! agent, loud, and sends nothing. The backend's own key read
//! ([`crate::credentials::KeyReader`]) calls the kernel directly and is untouched.
//!
//! A child of `tests`, so it drives the same harness. Python runs the route-level
//! cases against a stub kernel; here they run against a real kernel whose probe
//! agents record what reached them, with `route_reply` called directly, and the
//! deliveries read off the kernel's `send` call rows (python's `k.sent`). The
//! end-to-end cases run a real credential_store / file_bridge and a scripted model
//! that reads its key before every pass.

use super::*;
use crate::credentials::{key_reader, secret_holder_text, secret_join_text};
use fantastic_kernel::{PeerRoute, Status};

const EP: &str = "https://api.example.com/v1/responses";
const KEY: &str = "sk-live-holder-0123456789";
const DONE: &str = r#"<call name="done"></call>"#;
const PROBE: &str = "sh_probe.tools";
const KEYED: &str = "sh_keyed.tools";

fn refused(agent: &str) -> String {
    secret_holder_text(agent)
}

/// python `_call`: a `send` call to `target` with `{type: verb, **args}`.
fn call(target: &str, verb: &str, reply_to: &str, args: Value) -> String {
    let mut payload = json!({"type": verb});
    for (k, v) in args.as_object().cloned().unwrap_or_default() {
        payload[k] = v;
    }
    let mut a = Map::new();
    a.insert("target_id".into(), json!(target));
    a.insert("payload".into(), payload);
    a.insert("reply_to".into(), json!(reply_to));
    crate::tool_parse::render_call("send", &a)
}

#[test]
fn the_refusal_text_is_pythons() {
    assert_eq!(
        refused("creds"),
        "creds holds secrets — a secret holder answers backends, never a model turn; this \
         call was not dispatched"
    );
}

// ─── the choke point ─────────────────────────────────────────────────────────

/// What the probe agents were handed: `(agent, payload)`, in order — python's
/// `batches`, by the agent each call reached.
type Seen = Arc<std::sync::Mutex<Vec<(String, Value)>>>;

/// A plain agent: answers `{ok: <its id>}` and records every call it handles.
struct Probe(Seen);

#[async_trait]
impl Bundle for Probe {
    fn name(&self) -> &str {
        "sh_probe"
    }
    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        if matches!(payload["type"].as_str(), Some("boot" | "shutdown")) {
            return Ok(None);
        }
        self.0
            .lock()
            .unwrap()
            .push((agent_id.as_str().to_string(), payload.clone()));
        Ok(Some(json!({"ok": agent_id.as_str()})))
    }
}

async fn op(kernel: &Arc<Kernel>, target: &str, payload: Value) -> fantastic_kernel::Outcome {
    kernel
        .send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
}

async fn create(kernel: &Arc<Kernel>, rec: Value) {
    let mut p = json!({"type": "create_agent", "parent_id": "kernel"});
    for (k, v) in rec.as_object().cloned().unwrap() {
        p[k] = v;
    }
    let id = p["id"].clone();
    let made = op(kernel, "kernel_state", p).await.result;
    assert_eq!(made["id"], id, "{made}");
}

/// An AI agent of `handler` whose `can_call` is `grants` (plus its memory store).
async fn add_ai(kernel: &Arc<Kernel>, id: &str, handler: &str, grants: Value, extra: Value) {
    let memory = fantastic_ram_store::test_support::memory_meta(kernel, id, grants).await;
    let mut rec = json!({"handler_module": handler, "id": id, "model": "test-model",
                         "context_window": 32768});
    for (k, v) in memory
        .into_iter()
        .chain(extra.as_object().cloned().unwrap())
    {
        rec[k] = v;
    }
    create(kernel, rec).await;
}

/// python's stub kernel: `creds` holds secrets; `notes` and `peer` are plain
/// agents; the AI may call all three.
async fn choke_kernel(tmp: &TempDir, tag: &str) -> (Arc<Kernel>, AgentId, Seen) {
    let seen = Seen::default();
    let probe = Arc::clone(&seen);
    let kernel = base_kernel(tmp, move |k| {
        k.bundles.register(PROBE, Probe(probe));
    })
    .await;
    let ai = id_for(tmp, tag);
    for (id, holds) in [("creds", true), ("notes", false), ("peer", false)] {
        let mut rec = json!({"handler_module": PROBE, "id": id, "callable_by": {ai.as_str(): "*"}});
        if holds {
            rec["holds_secrets"] = json!(true);
        }
        create(&kernel, rec).await;
    }
    add_ai(
        &kernel,
        &ai,
        HANDLER_MODULE,
        json!({"creds": "*", "notes": "*", "peer": "*", "g1|creds": "*", "cli": "*"}),
        json!({}),
    )
    .await;
    (kernel, AgentId::from(ai), seen)
}

/// python `_route_reply("ai", k, text, "cli", None, None, 9.0, "", message_id=…)`.
async fn route(kernel: &Arc<Kernel>, ai: &AgentId, text: String, message_id: Option<i64>) {
    crate::route::route_reply(
        ai.clone(),
        Arc::clone(kernel),
        text,
        "cli".to_string(),
        None,
        None,
        String::new(),
        9.0,
        crate::constants::DEFAULT_CONTEXT_WINDOW,
        message_id,
        false,
    )
    .await;
}

/// The kernel's execution rows (it is never started here, so they stay in RAM).
fn all_rows(kernel: &Kernel) -> Vec<Value> {
    kernel.rows.all().into_iter().map(Value::Object).collect()
}

/// Every turn delivered to `agent` — the `send` call rows the kernel opened for
/// it, in id order (python's `k.sent` for that target).
fn delivered(kernel: &Kernel, agent: &AgentId) -> Vec<String> {
    all_rows(kernel)
        .into_iter()
        .filter(|r| r["kind"] == "call" && r["agent"] == agent.as_str() && r["verb"] == "send")
        .map(|r| r["payload"]["text"].as_str().unwrap_or("").to_string())
        .collect()
}

/// python `_settle`: wait for `want` deliveries to `agent`, then a beat more.
async fn settle(kernel: &Kernel, agent: &AgentId, want: usize) {
    for _ in 0..100 {
        if delivered(kernel, agent).len() >= want {
            tokio::time::sleep(Duration::from_millis(50)).await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "delivered {}, wanted {want}",
        delivered(kernel, agent).len()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_single_call_to_a_holder_is_refused_and_nothing_is_dispatched() {
    let tmp = TempDir::new().unwrap();
    let (kernel, ai, seen) = choke_kernel(&tmp, "sh_one").await;
    let _s = script(&ai, vec![pass(DONE)], Duration::ZERO);
    let text = call("creds", "get", "self", json!({"ref": "main"}));
    route(&kernel, &ai, text, None).await;
    settle(&kernel, &ai, 1).await;
    assert!(
        seen.lock().unwrap().is_empty(),
        "nothing may reach the holder"
    );
    let got = delivered(&kernel, &ai);
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(
        got[0].starts_with("[reply to - \u{b7} creds get"),
        "{}",
        got[0]
    );
    assert!(got[0].contains(&refused("creds")), "{}", got[0]);
    assert!(got[0].contains("unauthorized"), "{}", got[0]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_a_batch_only_the_holder_is_refused() {
    let tmp = TempDir::new().unwrap();
    let (kernel, ai, seen) = choke_kernel(&tmp, "sh_batch").await;
    let _s = script(&ai, vec![pass(DONE)], Duration::ZERO);
    let text = call("creds", "get", "self", json!({"ref": "main"}))
        + &call("notes", "reflect", "self", json!({}));
    route(&kernel, &ai, text, None).await;
    settle(&kernel, &ai, 1).await;
    // python's `batches == [["notes"]]`. A reflect here also asks the agent for
    // its own roster node, so `notes` may be handed `reflect` more than once; what
    // matters is that only `notes` was reached.
    let mut reached: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|(a, _)| a.clone())
        .collect();
    reached.dedup();
    assert_eq!(reached, vec!["notes".to_string()]);
    let got = delivered(&kernel, &ai);
    let blocks: Vec<&str> = got[0].split("\n\n").collect();
    assert_eq!(blocks.len(), 2, "{blocks:?}");
    assert!(
        blocks[0].starts_with("[reply to - \u{b7} creds get \u{b7} 1 of 2"),
        "{}",
        blocks[0]
    );
    assert!(blocks[0].contains(&refused("creds")), "{}", blocks[0]);
    assert!(blocks[1].starts_with("[reply to "), "{}", blocks[1]);
    assert!(
        blocks[1].contains(" \u{b7} notes reflect \u{b7} 2 of 2"),
        "{}",
        blocks[1]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_to_fan_out_drops_only_the_holder() {
    let tmp = TempDir::new().unwrap();
    let (kernel, ai, seen) = choke_kernel(&tmp, "sh_fan").await;
    let _s = script(&ai, vec![pass(DONE)], Duration::ZERO);
    let mut a = Map::new();
    a.insert("to".into(), json!(["peer", "creds"]));
    a.insert("reply_to".into(), json!("self"));
    a.insert("payload".into(), json!({"type": "send", "text": "hello"}));
    let text = crate::tool_parse::render_call("send", &a);
    route(&kernel, &ai, text, Some(7)).await;
    settle(&kernel, &ai, 1).await;
    let dispatched: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, p)| p["text"] == "hello")
        .map(|(a, _)| a.clone())
        .collect();
    assert_eq!(dispatched, vec!["peer".to_string()]);
    let own = delivered(&kernel, &ai);
    assert!(
        own.len() == 1 && own[0].contains(&refused("creds")),
        "{own:?}"
    );
}

/// A delivery to a holder is a dispatch on the model's behalf too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reply_to_a_holder_is_refused_to_self() {
    let tmp = TempDir::new().unwrap();
    let (kernel, ai, seen) = choke_kernel(&tmp, "sh_rt").await;
    let _s = script(&ai, vec![pass(DONE)], Duration::ZERO);
    let text = call("notes", "reflect", "creds", json!({}));
    route(&kernel, &ai, text, None).await;
    settle(&kernel, &ai, 1).await;
    assert!(
        seen.lock().unwrap().is_empty(),
        "the call is refused whole, not half-run"
    );
    let sends: Vec<String> = all_rows(&kernel)
        .into_iter()
        .filter(|r| r["kind"] == "call" && r["verb"] == "send")
        .map(|r| r["agent"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(sends, vec![ai.as_str().to_string()]);
    assert!(delivered(&kernel, &ai)[0].contains(&refused("creds")));
}

/// The refused call is not in flight, so `await` is the stall it is. ONE thread:
/// python reads `pending` before any delivery task runs, and so does this.
#[tokio::test]
async fn await_beside_a_refused_call_has_nothing_in_flight() {
    let tmp = TempDir::new().unwrap();
    let (kernel, ai, seen) = choke_kernel(&tmp, "sh_await").await;
    let _s = script(&ai, vec![pass(DONE), pass(DONE)], Duration::ZERO);
    let text =
        call("creds", "get", "self", json!({"ref": "main"})) + r#"<call name="await"></call>"#;
    route(&kernel, &ai, text, None).await;
    settle(&kernel, &ai, 2).await;
    assert!(seen.lock().unwrap().is_empty());
    let texts = delivered(&kernel, &ai);
    assert!(
        texts.iter().any(|t| t.contains(&refused("creds"))),
        "{texts:?}"
    );
    assert!(
        texts.iter().any(|t| t.starts_with("[await refused]")),
        "{texts:?}"
    );
}

/// `<guid>|creds` lives on another kernel: its record is not here, so this layer
/// cannot see it — the call goes, and that kernel guards its own holders.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_qualified_target_is_not_judged_here() {
    let tmp = TempDir::new().unwrap();
    let (kernel, ai, _seen) = choke_kernel(&tmp, "sh_peer").await;
    let _s = script(&ai, vec![pass(DONE)], Duration::ZERO);
    let calls: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let log = Arc::clone(&calls);
    let r = PeerRoute::new(
        "leg",
        move |agent_id, _payload, _caller, _secret, _body| {
            log.lock().unwrap().push(agent_id.clone());
            Box::pin(async move { (json!({"ok": agent_id}), None) })
        },
        |_, _, _, _| Box::pin(async {}),
    );
    kernel.register_peer("g1", &r);
    let text = call("g1|creds", "get", "self", json!({"ref": "main"}));
    route(&kernel, &ai, text, None).await;
    settle(&kernel, &ai, 1).await;
    assert_eq!(*calls.lock().unwrap(), vec!["creds".to_string()]);
    assert!(!delivered(&kernel, &ai)[0].contains(&refused("g1|creds")));
}

/// This kernel's guid (python's `OWN_GUID`), set on the root by [`set_guid`].
const OWN_GUID: &str = "own-guid";

/// Mint the root's `guid`, as its loader does at boot.
fn set_guid(kernel: &Kernel, guid: &str) {
    let mut g = Map::new();
    g.insert("guid".into(), json!(guid));
    kernel.update(&AgentId::from("kernel_state"), g);
}

/// `<this kernel's guid>|creds` is not a foreign address: the kernel resolves it
/// to the local `creds` and would deliver the call. The address is resolved the
/// same way BEFORE the holder check, so it is refused like the bare id — as a
/// target and as a `reply_to`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn this_kernels_own_guid_names_the_local_holder() {
    let tmp = TempDir::new().unwrap();
    let (kernel, ai, seen) = choke_kernel(&tmp, "sh_own").await;
    set_guid(&kernel, OWN_GUID);
    let _s = script(&ai, vec![pass(DONE)], Duration::ZERO);
    let own = format!("{OWN_GUID}|creds");
    let text = call(&own, "get", "self", json!({"ref": "main"}))
        + &call("notes", "reflect", &own, json!({}));
    route(&kernel, &ai, text, None).await;
    settle(&kernel, &ai, 1).await;
    assert!(
        seen.lock().unwrap().is_empty(),
        "nothing may reach the holder"
    );
    let sends: Vec<String> = all_rows(&kernel)
        .into_iter()
        .filter(|r| r["kind"] == "call" && r["verb"] == "send")
        .map(|r| r["agent"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(sends, vec![ai.as_str().to_string()]);
    let got = delivered(&kernel, &ai);
    let blocks: Vec<&str> = got[0].split("\n\n").collect();
    assert_eq!(blocks.len(), 2, "{blocks:?}");
    assert!(
        blocks.iter().all(|b| b.contains(&refused(&own))),
        "{blocks:?}"
    );
}

// ─── end to end: a real kernel, a real store, a scripted model ───────────────

/// The keys a [`KeyedProvider`] read, one per pass.
type Keys = Arc<std::sync::Mutex<Vec<Option<String>>>>;

/// A scripted mind that, like a real backend, reads its KEY before every pass
/// (through `key_reader`, as the agent) and replays its agent's script, which logs
/// every prompt it was sent (python `_Model`).
struct KeyedProvider {
    read: crate::credentials::KeyReader,
    keys: Keys,
    script: Arc<MockScript>,
    /// python `_JoiningModel`: on its FIRST pass the model JOINS the backend's
    /// latest key read of this target — a send naming that read's message id as
    /// `<exec_id>` (ids are sequential: a model can scan them).
    join: Option<(Arc<Kernel>, AgentId, String)>,
}

#[async_trait]
impl Provider for KeyedProvider {
    async fn chat(
        &self,
        messages: &[Value],
        max_tokens: Option<i64>,
    ) -> Result<ProviderStream, ProviderError> {
        let key = self.read.read(EP).await?;
        self.keys.lock().unwrap().push(key);
        if let Some((kernel, agent, target)) = &self.join {
            if self.script.passes_served() == 0 {
                let joined = rows(kernel, target, "get")
                    .iter()
                    .filter_map(|r| r["id"].as_i64())
                    .max()
                    .expect("the backend's key read is a row");
                joined_ids().insert(agent.as_str().to_string(), joined);
                let mut a = Map::new();
                a.insert("target_id".into(), json!(target));
                a.insert("payload".into(), json!({"type": "get", "ref": "main"}));
                a.insert("reply_to".into(), json!("self"));
                a.insert("exec_id".into(), json!(joined));
                let text = crate::tool_parse::render_call("send", &a);
                self.script.passes.lock().unwrap().push_front(pass(text));
            }
        }
        MockProvider {
            script: Arc::clone(&self.script),
        }
        .chat(messages, max_tokens)
        .await
    }
    fn model(&self) -> String {
        "test-model".to_string()
    }
}

/// The backend of the end-to-end cases: every verb through ai_core, its provider
/// a [`KeyedProvider`] over the agent's script.
struct KeyedBundle {
    required: bool,
    keys: Keys,
    /// The target a [`KeyedProvider`] JOINS on its first pass, when set.
    join: Option<String>,
}

/// The message id each joining model named as its `<exec_id>`, by agent.
fn joined_ids() -> std::sync::MutexGuard<'static, std::collections::HashMap<String, i64>> {
    static IDS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, i64>>> =
        std::sync::OnceLock::new();
    IDS.get_or_init(Default::default).lock().unwrap()
}

#[async_trait]
impl Bundle for KeyedBundle {
    fn name(&self) -> &str {
        "sh_keyed"
    }
    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let script = script_for(agent_id);
        let read = key_reader(agent_id, kernel, "responses_backend", self.required);
        let keys = Arc::clone(&self.keys);
        let join = self
            .join
            .clone()
            .map(|t| (Arc::clone(kernel), agent_id.clone(), t));
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let reply = verbs::handle_common(agent_id, payload, kernel, CFG, move |_client| {
            Arc::new(KeyedProvider {
                read: read.clone(),
                keys: Arc::clone(&keys),
                script: Arc::clone(&script),
                join: join.clone(),
            }) as Arc<dyn Provider>
        })
        .await?
        .unwrap_or_else(|| json!({"error": format!("sh_keyed: unknown type {verb:?}")}));
        Ok(Some(reply))
    }
}

async fn keyed_kernel(tmp: &TempDir, required: bool, keys: &Keys) -> Arc<Kernel> {
    keyed_kernel_joining(tmp, required, keys, None).await
}

/// [`keyed_kernel`] whose models JOIN the backend's latest key read of `join`
/// on their first pass.
async fn keyed_kernel_joining(
    tmp: &TempDir,
    required: bool,
    keys: &Keys,
    join: Option<&str>,
) -> Arc<Kernel> {
    let keys = Arc::clone(keys);
    let join = join.map(str::to_string);
    base_kernel(tmp, move |k| {
        k.bundles.register(
            fantastic_credential_store::HANDLER_MODULE,
            fantastic_credential_store::test_support::bundle(),
        );
        k.bundles.register(
            KEYED,
            KeyedBundle {
                required,
                keys,
                join,
            },
        );
    })
    .await
}

/// python `_rows`: the kernel's call rows to `agent` for `verb`.
fn rows(kernel: &Kernel, agent: &str, verb: &str) -> Vec<Value> {
    all_rows(kernel)
        .into_iter()
        .filter(|r| r["kind"] == "call" && r["agent"] == agent && r["verb"] == verb)
        .collect()
}

fn last_content(s: &MockScript, pass: usize) -> String {
    s.request_messages(pass)
        .last()
        .and_then(|m| m["content"].as_str())
        .unwrap_or("")
        .to_string()
}

/// python `_store_and_ai`: a real credential_store `creds` holding KEY at ref
/// `main` (readable by the AI) and an AI agent whose backend reads it (its
/// `can_call` defaults to `{creds: *, cli: *}`); returns the agent id.
async fn store_and_ai(
    kernel: &Arc<Kernel>,
    tmp: &TempDir,
    tag: &str,
    can_call: Option<Value>,
) -> AgentId {
    let aid = id_for(tmp, tag);
    create(
        kernel,
        json!({"handler_module": fantastic_credential_store::HANDLER_MODULE, "id": "creds",
               "file": tmp.path().join("creds.yaml").to_string_lossy(),
               "refs": {"main": {"host": "api.example.com", "readers": [aid.as_str()]}},
               "callable_by": {aid.as_str(): "*"}}),
    )
    .await;
    let creds = AgentId::from("creds");
    let meta = |k: &str| crate::helpers::agent_meta(&creds, kernel).get(k).cloned();
    assert_eq!(
        meta("holds_secrets"),
        Some(json!(true)),
        "the store stamps itself at boot"
    );
    let put = kernel
        .send(vec![Item::new(
            creds.clone(),
            json!({"type": "set", "ref": "main", "fields": {"api_key": KEY}}),
        )
        .with_secret(json!(["payload.fields"]))])
        .await
        .remove(0);
    assert_eq!(put.result["stored"], json!(true), "{}", put.result);
    add_ai(
        kernel,
        &aid,
        KEYED,
        can_call.unwrap_or_else(|| json!({"creds": "*", "cli": "*"})),
        json!({"credentials": {"store": "creds", "ref": "main"}}),
    )
    .await;
    AgentId::from(aid.as_str())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_model_call_to_a_credential_store_is_refused_and_the_key_stays_out() {
    let tmp = TempDir::new().unwrap();
    let keys = Keys::default();
    let kernel = keyed_kernel(&tmp, true, &keys).await;
    let ai = store_and_ai(&kernel, &tmp, "sh_e2e", None).await;
    let s = script(
        &ai,
        vec![
            pass(call("creds", "get", "self", json!({"ref": "main"}))),
            pass(DONE),
        ],
        Duration::ZERO,
    );
    let _ = op(
        &kernel,
        ai.as_str(),
        json!({"type": "send", "text": "fetch the key"}),
    )
    .await;
    assert!(until_passes(&s, 2).await, "the refusal never came back");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(s.passes_served(), 2);
    // the PROVIDER read its key on both passes — the backend path is untouched
    assert_eq!(
        *keys.lock().unwrap(),
        vec![Some(KEY.to_string()), Some(KEY.to_string())]
    );
    // the MODEL's call was refused, loud, in its next prompt
    let nxt = last_content(&s, 1);
    assert!(
        nxt.contains(&refused("creds")) && nxt.contains("unauthorized"),
        "{nxt}"
    );
    // the store answered the backend's two marked reads and nothing else
    let gets = rows(&kernel, "creds", "get");
    assert_eq!(gets.len(), 2, "{gets:?}");
    assert!(
        gets.iter().all(|r| r["secret"] == json!(["result"])),
        "{gets:?}"
    );
    // the key is in no prompt, no conversation row, no ledger
    assert!(!s.request(0).contains(KEY) && !s.request(1).contains(KEY));
    let dump = op(&kernel, MEMORY, json!({"type": "dump"}))
        .await
        .result
        .to_string();
    assert!(
        dump.contains(&refused("creds")),
        "the refusal is a conversation row"
    );
    assert!(!dump.contains(KEY));
    let ledger = op(&kernel, "kernel", json!({"type": "ledger"}))
        .await
        .result;
    assert!(!ledger.to_string().contains(KEY));
    assert!(!Value::Array(all_rows(&kernel)).to_string().contains(KEY));
}

/// The bypass this pins: `<this kernel's guid>|creds` has no record under that
/// literal id, yet the kernel delivers it to the local `creds` (and the agent's
/// `can_call` on `creds` grants it). The holder check resolves it the same way,
/// so the model's call is refused and never dispatched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_own_guid_address_of_a_holder_is_refused_from_a_model() {
    let tmp = TempDir::new().unwrap();
    let keys = Keys::default();
    let kernel = keyed_kernel(&tmp, true, &keys).await;
    set_guid(&kernel, OWN_GUID);
    let ai = store_and_ai(&kernel, &tmp, "sh_own_e2e", None).await;
    let own = format!("{OWN_GUID}|creds");
    let reached = op(&kernel, &own, json!({"type": "list"})).await;
    assert_eq!(reached.status, Status::Completed, "{}", reached.result);
    assert!(
        reached.result.get("refs").is_some(),
        "the kernel resolves the address to creds: {}",
        reached.result
    );
    let before = rows(&kernel, "creds", "get").len();
    let s = script(
        &ai,
        vec![
            pass(call(&own, "get", "self", json!({"ref": "main"}))),
            pass(DONE),
        ],
        Duration::ZERO,
    );
    let _ = op(
        &kernel,
        ai.as_str(),
        json!({"type": "send", "text": "fetch the key"}),
    )
    .await;
    assert!(until_passes(&s, 2).await, "the refusal never came back");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(s.passes_served(), 2);
    let nxt = last_content(&s, 1);
    assert!(
        nxt.contains(&refused(&own)) && nxt.contains("unauthorized"),
        "{nxt}"
    );
    // the store saw the backend's two marked reads and no call of the model's
    let gets = rows(&kernel, "creds", "get").split_off(before);
    assert_eq!(gets.len(), 2, "{gets:?}");
    assert!(
        gets.iter().all(|r| r["secret"] == json!(["result"])),
        "{gets:?}"
    );
    assert!(!s.request(0).contains(KEY) && !s.request(1).contains(KEY));
    assert!(!Value::Array(all_rows(&kernel)).to_string().contains(KEY));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_operator_marked_file_bridge_refuses_the_model() {
    let tmp = TempDir::new().unwrap();
    let keys = Keys::default();
    let kernel = keyed_kernel(&tmp, false, &keys).await;
    std::fs::write(tmp.path().join("vault.txt"), "TOP-SECRET-7f3a\n").unwrap();
    let aid = id_for(&tmp, "sh_fb");
    create(
        &kernel,
        json!({"handler_module": "file_bridge.tools", "id": "vault_fs", "allow": ["."],
               "root": tmp.path().to_string_lossy(),
               "ingress_rule": "allow_all_ingress", "callable_by": {aid.as_str(): "*"}}),
    )
    .await;
    let marked = op(
        &kernel,
        "kernel",
        json!({"type": "update_agent", "id": "vault_fs", "holds_secrets": true}),
    )
    .await
    .result;
    assert_eq!(marked["updated"], json!(true), "{marked}");
    let shown = op(&kernel, "vault_fs", json!({"type": "reflect"}))
        .await
        .result;
    assert_eq!(
        shown["holds_secrets"],
        json!(true),
        "a holder says so on its reflect"
    );
    add_ai(
        &kernel,
        &aid,
        KEYED,
        json!({"vault_fs": "*", "cli": "*"}),
        json!({}),
    )
    .await;
    let ai = AgentId::from(aid.as_str());
    let s = script(
        &ai,
        vec![
            pass(call(
                "vault_fs",
                "read",
                "self",
                json!({"path": "vault.txt"}),
            )),
            pass(DONE),
        ],
        Duration::ZERO,
    );
    let _ = op(
        &kernel,
        &aid,
        json!({"type": "send", "text": "read the vault"}),
    )
    .await;
    assert!(until_passes(&s, 2).await, "the refusal never came back");
    let nxt = last_content(&s, 1);
    assert!(nxt.contains(&refused("vault_fs")), "{nxt}");
    assert!(
        rows(&kernel, "vault_fs", "read").is_empty(),
        "the read never dispatched"
    );
    assert!(!s.request(0).contains("TOP-SECRET-7f3a") && !s.request(1).contains("TOP-SECRET-7f3a"));
}

/// The bypass this pins: a model whose agent holds `update_agent` writes
/// `holds_secrets: false` on the store, then calls it. The KERNEL's gate refuses
/// the unmark (only the operator lowers the mark), so the store stays a holder
/// and ai_core still refuses the call — the key never reaches the model.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_model_cannot_unmark_a_holder_then_call_it() {
    let tmp = TempDir::new().unwrap();
    let keys = Keys::default();
    let kernel = keyed_kernel(&tmp, true, &keys).await;
    let ai = store_and_ai(
        &kernel,
        &tmp,
        "sh_unmark",
        // the store's record is the operand: the agent names it with `update_agent`
        Some(json!({"creds": ["*", "update_agent"], "cli": "*"})),
    )
    .await;
    let unmark = call(
        "kernel",
        "update_agent",
        "self",
        json!({"id": "creds", "holds_secrets": false}),
    );
    let s = script(
        &ai,
        vec![
            pass(unmark),
            pass(call("creds", "get", "self", json!({"ref": "main"}))),
            pass(DONE),
        ],
        Duration::ZERO,
    );
    let _ = op(
        &kernel,
        ai.as_str(),
        json!({"type": "send", "text": "unmark the store, then read it"}),
    )
    .await;
    assert!(until_passes(&s, 3).await, "the third pass never came");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let refused_unmark = last_content(&s, 1);
    assert!(
        refused_unmark.contains(&format!(
            "{ai} may not clear holds_secrets on creds: it holds secrets, and only the \
             operator lowers that mark"
        )),
        "{refused_unmark}"
    );
    assert!(
        refused_unmark.contains("secret-holder") && refused_unmark.contains("unauthorized"),
        "{refused_unmark}"
    );
    assert_eq!(
        crate::helpers::agent_meta(&AgentId::from("creds"), &kernel).get("holds_secrets"),
        Some(&json!(true)),
        "the mark stayed"
    );
    let nxt = last_content(&s, 2);
    assert!(nxt.contains(&refused("creds")), "{nxt}");
    let gets = rows(&kernel, "creds", "get");
    assert!(
        gets.iter().all(|r| r["secret"] == json!(["result"])),
        "{gets:?}"
    );
    assert!((0..3).all(|i| !s.request(i).contains(KEY)));
    assert!(!Value::Array(all_rows(&kernel)).to_string().contains(KEY));
}

// ─── a model never JOINS an execution that carried secrets (audit F2) ────────

const PEER_KEY: &str = "sk-peer-0123456789abcdef";

fn join_refused(exec_id: &str) -> String {
    secret_join_text(exec_id)
}

#[test]
fn the_join_refusal_text_is_pythons() {
    assert_eq!(
        join_refused("7"),
        "exec_id 7 names an execution that carried secret values — a model never joins \
         one; this call was not dispatched"
    );
}

/// THE BYPASS (measured 2026-10-03): the key lives in a credential_store on a
/// CONNECTED PEER (`P-g|creds`) — a peer address this layer cannot judge as a
/// holder — and the backend reads it as the agent, marked. The model, the same
/// agent, sent `<exec_id>` = that read's pid: the kernel joined it (same caller,
/// no second dispatch, the peer never saw it) and the model got the key.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_model_cannot_join_its_backends_key_read_on_a_peer() {
    let tmp = TempDir::new().unwrap();
    let keys = Keys::default();
    let kernel = keyed_kernel_joining(&tmp, true, &keys, Some("P-g|creds")).await;
    set_guid(&kernel, "A-g");
    type PeerSeen = Arc<std::sync::Mutex<Vec<(String, String, String, Vec<Value>)>>>;
    let seen: PeerSeen = Arc::default();
    let log = Arc::clone(&seen);
    let peer = PeerRoute::new(
        "leg",
        move |agent_id, payload, caller, secret, _body| {
            let verb = payload["type"].as_str().unwrap_or("").to_string();
            log.lock()
                .unwrap()
                .push((agent_id, verb, caller, secret.clone()));
            let marked = secret
                .iter()
                .any(|m| *m == json!("result") || *m == json!(["result"]));
            Box::pin(async move {
                if !marked {
                    return (
                        json!({"error": "get: mark the answer secret", "reason": "bad_arg"}),
                        None,
                    );
                }
                (
                    json!({"ref": "main", "host": "api.example.com",
                       "fields": {"api_key": PEER_KEY}}),
                    None,
                )
            })
        },
        |_, _, _, _| Box::pin(async {}),
    );
    kernel.register_peer("P-g", &peer);
    let aid = id_for(&tmp, "sh_join_peer");
    add_ai(
        &kernel,
        &aid,
        KEYED,
        json!({"P-g|creds": ["get"]}),
        json!({"credentials": {"store": "P-g|creds", "ref": "main"}}),
    )
    .await;
    let ai = AgentId::from(aid.as_str());
    let s = script(&ai, vec![pass(DONE)], Duration::ZERO);
    let _ = op(
        &kernel,
        ai.as_str(),
        json!({"type": "send", "text": "read your key"}),
    )
    .await;
    assert!(until_passes(&s, 2).await, "the second pass never came");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        keys.lock().unwrap()[0].as_deref(),
        Some(PEER_KEY),
        "the backend still reads its key"
    );
    let joined = *joined_ids().get(ai.as_str()).expect("the model joined");
    let nxt = last_content(&s, 1);
    assert!(nxt.contains(&join_refused(&joined.to_string())), "{nxt}");
    assert!(nxt.contains("unauthorized"), "{nxt}");
    assert!(!s.request(0).contains(PEER_KEY) && !s.request(1).contains(PEER_KEY));
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .all(|(_, _, _, m)| *m == vec![json!("result")]),
        "only the backend's reads"
    );
}

/// By NAME, on a target that is no holder: an execution whose item carried
/// `secret` marks is refused to a model whoever opened it — the call is never
/// dispatched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_model_cannot_join_a_named_secret_execution() {
    let tmp = TempDir::new().unwrap();
    let keys = Keys::default();
    let kernel = keyed_kernel(&tmp, false, &keys).await;
    std::fs::write(tmp.path().join("vault.txt"), "TOP-SECRET-9c1d\n").unwrap();
    let aid = id_for(&tmp, "sh_join_name");
    create(
        &kernel,
        json!({"handler_module": "file_bridge.tools", "id": "notes_fs", "allow": ["."],
               "root": tmp.path().to_string_lossy(),
               "ingress_rule": "allow_all_ingress", "callable_by": {aid.as_str(): "*"}}),
    )
    .await;
    let read = kernel
        .send(vec![Item::new(
            AgentId::from("notes_fs"),
            json!({"type": "read", "path": "vault.txt"}),
        )
        .with_id("vault-read")
        .with_secret(json!(["result"]))])
        .await
        .remove(0);
    assert_eq!(read.status, Status::Completed, "{}", read.result);
    add_ai(
        &kernel,
        &aid,
        KEYED,
        json!({"notes_fs": "*", "cli": "*"}),
        json!({}),
    )
    .await;
    let ai = AgentId::from(aid.as_str());
    let mut a = Map::new();
    a.insert("target_id".into(), json!("notes_fs"));
    a.insert(
        "payload".into(),
        json!({"type": "read", "path": "vault.txt"}),
    );
    a.insert("reply_to".into(), json!("self"));
    a.insert("exec_id".into(), json!("vault-read"));
    let join = crate::tool_parse::render_call("send", &a);
    let s = script(&ai, vec![pass(join), pass(DONE)], Duration::ZERO);
    let _ = op(
        &kernel,
        &aid,
        json!({"type": "send", "text": "read the vault"}),
    )
    .await;
    assert!(until_passes(&s, 2).await, "the second pass never came");
    let nxt = last_content(&s, 1);
    assert!(nxt.contains(&join_refused("vault-read")), "{nxt}");
    assert!(!s.request(0).contains("TOP-SECRET-9c1d") && !s.request(1).contains("TOP-SECRET-9c1d"));
}
