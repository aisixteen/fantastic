//! Stateful fixture agents the shared e2e scenarios name.
//!
//! `e2e/tests/scenario/*/scenario.yaml` declares a roster by ROLE. Python and js
//! register these roles in process — a handler is thirty lines there. A compiled
//! kernel can only run what is compiled INTO it, and the e2e driver reaches it
//! over REST, so there is no way to inject a handler through a socket. Without
//! this crate `ledger`, `deadletter` and `user_actions` are SKIPPED on rust —
//! six of twenty scenario runs that never execute.
//!
//! TEST-ONLY, and gated so it cannot leak: `fantastic-cli` registers these
//! behind the `scenario-probes` feature, off by default. A production binary
//! carries none of it. Substituting a shipped bundle instead (`yaml_state` for
//! `dispenser`) was the alternative and is worse: it has no `dispense` verb and
//! no side-effect counter, so the row would go green while measuring something
//! else.
//!
//! Each role COUNTS its side effects, because that is what the scenarios assert
//! on — a model that claims it bought once cannot fake the desk's own tally.

use async_trait::async_trait;
use fantastic_bundle as _; // dep keeps the bundle ↔ kernel link explicit
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// A counter a scenario can read back through the agent's own `count` verb —
/// the runner asserts on THIS, never on the model's prose.
#[derive(Default)]
struct Counter(AtomicU64);

impl Counter {
    fn bump(&self) -> u64 {
        self.0.fetch_add(1, Ordering::SeqCst) + 1
    }
    fn get(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

fn token() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos()
        .wrapping_mul(2_654_435_761);
    format!("{n:08x}")
}

// ── dispenser ───────────────────────────────────────────────────────────────

/// `dispenser` — one coin per `dispense`. Issuing is a SIDE EFFECT: calling
/// twice dispenses twice, which is exactly what the double-fire scenario exists
/// to catch. A named execution must therefore return the FIRST run's outcome
/// rather than running the handler again.
#[derive(Default)]
pub struct DispenserProbe {
    count: Counter,
}

pub const DISPENSER_MODULE: &str = "dispenser_probe.tools";

#[async_trait]
impl Bundle for DispenserProbe {
    fn name(&self) -> &str {
        "dispenser_probe"
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let reply = match payload.get("type").and_then(Value::as_str).unwrap_or("") {
            "boot" | "shutdown" => Value::Null,
            "reflect" => json!({
                "id": agent_id.as_str(),
                "sentence": "Coin dispenser — one coin per `dispense` call. A coin must never be issued twice for one job.",
                "verbs": {
                    "dispense": "args: job:str — issues ONE coin, returns {coin, receipt}. Issuing is a side effect: calling twice dispenses twice.",
                    "count": "how many coins have ever been dispensed",
                },
            }),
            "dispense" => json!({
                "coin": self.count.bump(),
                "receipt": token(),
                "job": payload.get("job"),
            }),
            "count" => json!({ "count": self.count.get() }),
            other => json!({"error": format!("dispenser_probe: unknown verb {other:?}")}),
        };
        Ok(Some(reply))
    }
}

// ── flaky plant ─────────────────────────────────────────────────────────────

/// `flaky_plant` — a pump that fails twice then works, and a valve that never
/// does. Both RETURN AN ERROR: a terminal failure is what a retry budget and a
/// dead-letter row are for, and a role that quietly succeeded would make the
/// scenario unfalsifiable.
#[derive(Default)]
pub struct PlantProbe {
    runs: Counter,
}

pub const PLANT_MODULE: &str = "plant_probe.tools";

#[async_trait]
impl Bundle for PlantProbe {
    fn name(&self) -> &str {
        "plant_probe"
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        match payload.get("type").and_then(Value::as_str).unwrap_or("") {
            "boot" | "shutdown" => Ok(Some(Value::Null)),
            "reflect" => Ok(Some(json!({
                "id": agent_id.as_str(),
                "sentence": "Industrial plant. Its pump is unreliable; its valve has seized.",
                "verbs": {
                    "pump": "no args — runs the pump. Fails intermittently.",
                    "valve": "no args — operates the valve.",
                    "count": "how many times either has been attempted",
                },
            }))),
            "pump" => {
                // FLAKY, not broken: the third attempt works. A caller that gives
                // up after one try never learns that — which is the measurement.
                if self.runs.bump() < 3 {
                    Err(BundleError::from("pump stalled"))
                } else {
                    Ok(Some(json!({"pumped": true, "serial": token()})))
                }
            }
            "valve" => {
                self.runs.bump();
                Err(BundleError::from("valve is rusted shut"))
            }
            "count" => Ok(Some(json!({ "count": self.runs.get() }))),
            other => Ok(Some(
                json!({"error": format!("plant_probe: unknown verb {other:?}")}),
            )),
        }
    }
}

// ── purchase desk ───────────────────────────────────────────────────────────

/// `purchase_desk` — spends real money, so only a person may press the button.
/// The AI is deliberately granted NO edge to it: that it cannot buy is half the
/// measurement, and the ledger must make a double-click harmless.
#[derive(Default)]
pub struct PurchaseProbe {
    bought: Counter,
}

pub const PURCHASE_MODULE: &str = "purchase_probe.tools";

#[async_trait]
impl Bundle for PurchaseProbe {
    fn name(&self) -> &str {
        "purchase_probe"
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let reply = match payload.get("type").and_then(Value::as_str).unwrap_or("") {
            "boot" | "shutdown" => Value::Null,
            "reflect" => json!({
                "id": agent_id.as_str(),
                "sentence": "Purchase desk. Spends real money — only a person may press the button.",
                "verbs": {
                    "buy": "args: item:str — buys ONE. Spending is a side effect.",
                    "count": "how many purchases have been made",
                },
            }),
            "buy" => json!({
                "bought": payload.get("item"),
                "receipt": token(),
                "n": self.bought.bump(),
            }),
            "count" => json!({ "count": self.bought.get() }),
            other => json!({"error": format!("purchase_probe: unknown verb {other:?}")}),
        };
        Ok(Some(reply))
    }
}
