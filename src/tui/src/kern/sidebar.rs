//! THE SIDEBAR's agent (`sidebar`): the canvas's left column — the kernels, the agent
//! tree with each chat's state, what waits for the person, and the project's files — as
//! ONE html view its `reflect` returns (compiled in, no record field).
//! THE AGENT IS THE MODEL: from its `boot` it follows the kernel and publishes every
//! change its tree shows on its own inbox ([`follow`]); its view watches it alone. The
//! view's other calls are READ verbs on the agents it draws (the `calls` object). Made
//! under the root by the first-run seed (`blueprint.yaml`).

use std::sync::Arc;

use async_trait::async_trait;
use fantastic_kernel::bundle::{BundleError, Handler, Reply};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};

/// The `handler_module` this bundle answers to.
pub const HANDLER_MODULE: &str = "sidebar.tools";

pub mod follow;
pub mod record;
pub mod verbs;

/// What the bundle is for: its one short man text; how to call it is rendered from its
/// declared verbs (`docs`, `reflect`).
const MAN: &str = "The canvas's left column as one html view its `reflect` returns: the kernels, the agent tree with each chat's state, what waits for the person, and the project's files. It follows the kernel and publishes each change on its own inbox: tree_changed, status_changed {agent, phase}, line_summary_changed {agent, text, ts}, inquiries_changed {open}, files_changed {path, change}.";

const VIEW: &str = include_str!("sidebar_view.html");

/// What the view calls: the root's reflect (its listing folded in), the person's open inquiries, the
/// project's files (a listing, and a watch on each folder it shows, held by the files agent),
/// and its OWN agent: its `reflect` (each chat's last phase and latest line, and the files'
/// root, the project's name) and its events (`watch`) —
/// the one agent it watches. It reads again only on what the agent publishes, never on a
/// timer.
fn calls(id: &str) -> Value {
    json!({
        "kernel": ["reflect"],
        "user": ["inquiries"],
        "files": ["list", "watch_path", "unwatch_path", "open_view"],
        id: ["reflect", "watch"],
    })
}

/// The root the files agent serves (its own `reflect`, asked by the sidebar agent), "" when
/// there is no files agent: the view names the project from it. The reflect is a call (#795):
/// `then` gets the root when it settles.
async fn files_root<F, Fut>(kernel: &Arc<Kernel>, then: F)
where
    F: FnOnce(String) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let files = AgentId::from(follow::FILES);
    let served = kernel.resolve_target(files.as_str()).is_some_and(|a| a.handler_module.is_some());
    if !served {
        return then(String::new()).await;
    }
    kernel
        .call_then(fantastic_kernel::Item::new(files, json!({"type": "reflect"})), move |out| async move {
            then(out.result.get("root").and_then(Value::as_str).unwrap_or_default().to_string()).await;
        })
        .await;
}

/// The bundle.
pub struct SidebarBundle;

#[async_trait]
impl Handler for SidebarBundle {
    type Call = verbs::Call;

    fn name(&self) -> &str {
        HANDLER_MODULE
    }

    fn man(&self) -> &'static str {
        MAN
    }

    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        verbs::VERBS
    }

    fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
        verbs::dynamic_verbs(agent)
    }

    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        &record::RECORD
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        call: Self::Call,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        match call {
            verbs::Call::Boot(_) => {
                follow::start(kernel, agent_id);
                Ok(Some(json!({"following": true})))
            }
            // the files agent's root comes from its own reflect: this one answers
            // when that settles (it defers its settle). The kernel's own roster read of
            // this agent is a bookkeeping run with no settle to defer: it is answered at
            // once, with no root (it takes the sentence only).
            verbs::Call::Reflect(_) => {
                let (k, me) = (Arc::clone(kernel), agent_id.clone());
                let answer = |k: &Arc<Kernel>, me: &AgentId, root: String| {
                    let id = me.as_str();
                    json!({
                        "id": id,
                        "handler_module": HANDLER_MODULE,
                        "sentence": MAN,
                        "phases": follow::phases(k, me),
                        "line_summaries": follow::lines(k, me),
                        "open": follow::open_questions(k),
                        "files_root": root,
                        "view": {"kind": "html", "bytes": VIEW.replace("{{agent}}", id), "calls": calls(id)},
                    })
                };
                let (k2, me2) = (Arc::clone(&k), me.clone());
                let reply = crate::kern::cps::reply_later(kernel, |done| async move {
                    files_root(&k.clone(), move |root| async move { done(answer(&k2, &me2, root)).await }).await;
                })
                .await;
                Ok(Some(if fantastic_kernel::is_refused(&reply) { answer(kernel, agent_id, String::new()) } else { reply }))
            }
        }
    }

    /// Deleted: it follows the kernel no more.
    async fn on_delete(&self, agent_id: &AgentId, kernel: &Arc<Kernel>) -> Result<(), BundleError> {
        follow::stop(kernel, agent_id);
        Ok(())
    }

    /// The kernel stops: so does the tap.
    async fn on_shutdown(&self, agent_id: &AgentId, kernel: &Arc<Kernel>) -> Result<(), BundleError> {
        follow::stop(kernel, agent_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kern::host::Host;

    /// A kernel whose agent `sidebar` runs this bundle: its `reflect` defers its settle (#795),
    /// so it is asked through a dispatch, as the operator, and its settle is the answer.
    fn world() -> (Arc<Kernel>, Host) {
        let mut kernel = Kernel::new();
        kernel.bundles.register(HANDLER_MODULE, SidebarBundle);
        let kernel = Arc::new(kernel);
        // a root above it: an agent with no parent is the kernel's own, and its reflect is the kernel's
        let root = fantastic_kernel::Agent::new(AgentId::from("root"), None, None, Default::default(), std::path::PathBuf::new(), false);
        fantastic_kernel::test_support::register(&kernel, Arc::clone(&root));
        kernel.set_root(root);
        let host = Host { kernel: Arc::clone(&kernel), cfg: crate::env::ClientCfg::here(), root: "root".to_string() };
        (kernel, host)
    }

    async fn reflect(host: &Host) -> Value {
        crate::kern::cps::op(host, "sidebar", json!({"type": "reflect"})).await.result
    }

    #[tokio::test]
    async fn the_sidebar_answers_reflect_and_refuses_the_rest_by_declaration() {
        let (kernel, host) = world();
        fantastic_kernel::test_support::register(&kernel, fantastic_kernel::Agent::new(
            AgentId::from("sidebar"),
            Some(HANDLER_MODULE.to_string()),
            Some(AgentId::from("root")),
            Default::default(),
            std::path::PathBuf::new(),
            false,
        ));
        let ask = |payload: Value| {
            let kernel = Arc::clone(&kernel);
            async move {
                fantastic_kernel::Bundle::handle_binary(&SidebarBundle, &AgentId::from("sidebar"), payload, None, &kernel)
                    .await
                    .unwrap()
                    .0
            }
        };
        let r = ask(json!({"type": "launch"})).await.unwrap();
        assert_eq!(r["reason"], "unhandled", "{r}");
        assert!(r["error"].as_str().unwrap().contains("reflect"), "{r}");
        assert!(r["error"].as_str().unwrap().contains("boot"), "{r}");
        for v in ["reflect", "boot"] {
            let r = ask(json!({"type": v, "zzz_unknown": 1})).await.unwrap();
            assert_eq!((r["reason"].as_str(), r["arg"].as_str()), (Some("bad_arg"), Some("zzz_unknown")), "{v}: {r}");
        }
        let r = reflect(&host).await;
        assert_eq!(r["view"]["kind"], "html", "{r}");
    }

    /// The kernel's own roster read of the sidebar (a bookkeeping run, nothing to defer) still
    /// gets its sentence.
    #[tokio::test]
    async fn the_roster_of_the_tree_still_lists_the_sidebars_sentence() {
        let mut kernel = Kernel::new();
        kernel.bundles.register(HANDLER_MODULE, SidebarBundle);
        kernel.bundles.register(fantastic_core::HANDLER_MODULE, fantastic_core::KernelStateBundle::default());
        let kernel = Arc::new(kernel);
        let root = fantastic_kernel::Agent::new(
            AgentId::from("kernel"),
            Some(fantastic_core::HANDLER_MODULE.to_string()),
            None,
            Default::default(),
            std::path::PathBuf::new(),
            false,
        );
        fantastic_kernel::test_support::register(&kernel, Arc::clone(&root));
        kernel.set_root(root);
        let host = Host { kernel: Arc::clone(&kernel), cfg: crate::env::ClientCfg::here(), root: "kernel".to_string() };
        let made = crate::kern::cps::op(&host, "kernel", json!({"type": "create_agent", "parent_id": "kernel", "id": "sidebar", "handler_module": HANDLER_MODULE})).await.result;
        assert!(made.get("error").is_none(), "{made}");
        let r = crate::kern::cps::op(&host, "kernel", json!({"type": "reflect", "tree": "all"})).await.result;
        let found = r["tree"]["children"].as_array().and_then(|c| c.iter().find(|n| n["id"] == "sidebar")).cloned();
        assert_eq!(found.and_then(|n| n["sentence"].as_str().map(str::to_string)).as_deref(), Some(MAN), "{r}");
    }

    #[tokio::test]
    async fn reflect_carries_the_html_view_and_exactly_its_calls() {
        let (kernel, host) = world();
        fantastic_kernel::test_support::register(&kernel, fantastic_kernel::Agent::new(
            AgentId::from("sidebar"),
            Some(HANDLER_MODULE.to_string()),
            Some(AgentId::from("root")),
            Default::default(),
            std::path::PathBuf::new(),
            false,
        ));
        let r = reflect(&host).await;
        let view = &r["view"];
        assert_eq!(view["kind"], "html");
        assert!(
            view["bytes"]
                .as_str()
                .is_some_and(|b| b.contains("fantastic.send")),
            "the body is there"
        );
        assert_eq!(
            view["calls"],
            json!({
                "kernel": ["reflect"],
                "user": ["inquiries"],
                "files": ["list", "watch_path", "unwatch_path", "open_view"],
                "sidebar": ["reflect", "watch"],
            })
        );
        assert_eq!(r["phases"], json!({}), "no chat has moved");
        assert_eq!(r["line_summaries"], json!({}), "no chat has written a line");
        assert_eq!(r["open"], json!(0));
        assert_eq!(r["files_root"], "", "no files agent: no root");
    }

    fn agent(kernel: &Arc<Kernel>, id: &str) {
        let handler = (id == "sidebar").then(|| HANDLER_MODULE.to_string());
        fantastic_kernel::test_support::register(kernel, fantastic_kernel::Agent::new(AgentId::from(id), handler, Some(AgentId::from("root")), Default::default(), std::path::PathBuf::new(), false));
    }

    /// The next event the sidebar published on its own inbox: `(type, the whole event)`.
    /// The `reflect` calls the test makes arrive on that inbox as messages too: they are not events.
    async fn next(rx: &mut fantastic_kernel::inbox::InboxReceiver) -> Value {
        loop {
            let p = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .expect("the sidebar published")
                .expect("its inbox is open")
                .payload;
            if p["type"] != "reflect" {
                return p;
            }
        }
    }

    /// Nothing but the test's own `reflect` calls is waiting on the inbox.
    fn quiet(rx: &mut fantastic_kernel::inbox::InboxReceiver) -> bool {
        while let Some(m) = rx.try_recv() {
            if m.payload["type"] != "reflect" {
                return false;
            }
        }
        true
    }

    #[tokio::test]
    async fn from_its_boot_the_sidebar_publishes_what_its_tree_shows_on_its_own_inbox() {
        let (kernel, host) = world();
        for id in ["sidebar", "c1", "files", "user", "web"] {
            agent(&kernel, id);
        }
        let me = AgentId::from("sidebar");
        let mut rx = kernel.ensure_inbox(&me);
        let ask = |payload: Value| {
            let kernel = Arc::clone(&kernel);
            async move {
                fantastic_kernel::Bundle::handle_binary(&SidebarBundle, &AgentId::from("sidebar"), payload, None, &kernel)
                    .await
                    .unwrap()
                    .0
                    .unwrap()
            }
        };
        assert_eq!(ask(json!({"type": "boot"})).await, json!({"following": true}));
        assert_eq!(ask(json!({"type": "boot"})).await, json!({"following": true}), "a second boot: still one tap");
        let emit = |at: &str, p: Value| {
            let kernel = Arc::clone(&kernel);
            let at = AgentId::from(at);
            async move { fantastic_kernel::send::as_operator(kernel.emit(&at, p)).await }
        };

        // a chat's call moved: its phase, kept for the view's first read
        emit("c1", json!({"type": "status", "source": "c1", "phase": "thinking"})).await;
        assert_eq!(next(&mut rx).await, json!({"type": "status_changed", "agent": "c1", "phase": "thinking"}));
        assert_eq!(reflect(&host).await["phases"], json!({"c1": "thinking"}));

        // a chat wrote a line: passed on with the tree state, kept for the view's first read
        emit("c1", json!({"type": "line_summary_changed", "source": "c1", "text": "reading the spec", "ts": 12.5})).await;
        assert_eq!(
            next(&mut rx).await,
            json!({"type": "line_summary_changed", "agent": "c1", "text": "reading the spec", "ts": 12.5})
        );
        assert_eq!(
            reflect(&host).await["line_summaries"],
            json!({"c1": {"text": "reading the spec", "ts": 12.5}})
        );

        // an agent came (the kernel's lifecycle), a kernel connected (a leg's event)
        kernel.publish_state(&json!({"agent_id": "c2", "kind": "added", "parent_id": "kernel"}));
        assert_eq!(next(&mut rx).await, json!({"type": "tree_changed"}));
        emit("web", json!({"type": "peer_connected", "guid": "g2"})).await;
        assert_eq!(next(&mut rx).await, json!({"type": "tree_changed"}));

        // a question to the person
        emit("user", json!({"type": "inquiry_opened", "re": 4, "asker": "c1", "holder": "user"})).await;
        assert_eq!(next(&mut rx).await, json!({"type": "inquiries_changed", "open": 0}));

        // the files: a log written in place is nothing; an entry that came or went is
        emit("files", json!({"type": "changed", "path": "team.log", "kind": "modified"})).await;
        emit("files", json!({"type": "changed", "path": "src/a.rs", "kind": "added"})).await;
        assert_eq!(next(&mut rx).await, json!({"type": "files_changed", "path": "src/a.rs", "change": "added"}), "the write in place published nothing");
        emit("files", json!({"type": "changed", "path": "src/a.rs", "kind": "deleted"})).await;
        assert_eq!(next(&mut rx).await, json!({"type": "files_changed", "path": "src/a.rs", "change": "removed"}));

        // the call ended: its end is the chat's last phase, kept until the next call
        emit("c1", json!({"type": "status", "source": "c1", "phase": "failed"})).await;
        assert_eq!(next(&mut rx).await, json!({"type": "status_changed", "agent": "c1", "phase": "failed"}));
        assert_eq!(reflect(&host).await["phases"], json!({"c1": "failed"}));
        emit("c1", json!({"type": "status", "source": "c1", "phase": "paused"})).await;
        assert_eq!(next(&mut rx).await, json!({"type": "status_changed", "agent": "c1", "phase": "paused"}));
        assert_eq!(reflect(&host).await["phases"], json!({"c1": "paused"}));
        // resumed with nothing held: the chat is at rest
        emit("c1", json!({"type": "status", "source": "c1", "phase": "idle"})).await;
        assert_eq!(next(&mut rx).await, json!({"type": "status_changed", "agent": "c1", "phase": "idle"}));
        assert_eq!(reflect(&host).await["phases"], json!({}));
        assert!(quiet(&mut rx), "nothing else was published");

        // deleted: it follows no more
        fantastic_kernel::Bundle::on_delete(&SidebarBundle, &me, &kernel).await.unwrap();
        emit("c1", json!({"type": "status", "source": "c1", "phase": "thinking"})).await;
        kernel.publish_state(&json!({"agent_id": "c3", "kind": "added", "parent_id": "kernel"}));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(quiet(&mut rx), "a deleted sidebar publishes nothing");
    }
}
