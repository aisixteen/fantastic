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

/// What the view calls: the root's reflect and listing, the person's open inquiries, the
/// project's files (a listing, and a watch on each folder it shows, held by the files agent),
/// and its OWN agent: its `reflect` (each chat's last phase and latest line, and the files'
/// root, the project's name) and its events (`watch`) —
/// the one agent it watches. It reads again only on what the agent publishes, never on a
/// timer.
fn calls(id: &str) -> Value {
    json!({
        "kernel": ["reflect", "list_agents"],
        "user": ["inquiries"],
        "files": ["list", "watch_path", "unwatch_path", "open_view"],
        id: ["reflect", "watch"],
    })
}

/// The root the files agent serves (its own `reflect`, asked by the sidebar agent), "" when
/// there is no files agent: the view names the project from it.
async fn files_root(kernel: &Arc<Kernel>) -> String {
    let files = AgentId::from(follow::FILES);
    let served = kernel.agents.get(&files).is_some_and(|a| a.handler_module.is_some());
    if !served {
        return String::new();
    }
    let r = kernel
        .send(vec![fantastic_kernel::Item::new(files, json!({"type": "reflect"}))])
        .await
        .remove(0)
        .result;
    r.get("root").and_then(Value::as_str).unwrap_or_default().to_string()
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
            verbs::Call::Reflect(_) => {
                let id = agent_id.as_str();
                Ok(Some(json!({
                    "id": id,
                    "handler_module": HANDLER_MODULE,
                    "sentence": MAN,
                    "phases": follow::phases(kernel, agent_id),
                    "line_summaries": follow::lines(kernel, agent_id),
                    "open": follow::open_questions(kernel),
                    "files_root": files_root(kernel).await,
                    "view": {"kind": "html", "bytes": VIEW.replace("{{agent}}", id), "calls": calls(id)},
                })))
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

    #[tokio::test]
    async fn the_sidebar_answers_reflect_and_refuses_the_rest_by_declaration() {
        let kernel = Arc::new(Kernel::new());
        kernel.register(fantastic_kernel::Agent::new(
            AgentId::from("sidebar"),
            None,
            None,
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
        let r = ask(json!({"type": "reflect"})).await.unwrap();
        assert_eq!(r["view"]["kind"], "html", "{r}");
    }

    #[tokio::test]
    async fn reflect_carries_the_html_view_and_exactly_its_calls() {
        let kernel = Arc::new(Kernel::new());
        let r = SidebarBundle
            .handle(
                &AgentId::from("sidebar"),
                verbs::Call::Reflect(fantastic_kernel::declared::common::Reflect {
                    detail: None,
                    tree: None,
                    show_subtrees: None,
                    ignore_subtrees: None,
                    handler_modules: None,
                    bundles: None,
                    bundle_names: None,
                    limit: None,
                    after: None,
                }),
                &kernel,
            )
            .await
            .unwrap()
            .unwrap();
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
                "kernel": ["reflect", "list_agents"],
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
        kernel.register(fantastic_kernel::Agent::new(AgentId::from(id), None, None, Default::default(), std::path::PathBuf::new(), false));
    }

    /// The next event the sidebar published on its own inbox: `(type, the whole event)`.
    async fn next(rx: &mut fantastic_kernel::inbox::InboxReceiver) -> Value {
        tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("the sidebar published")
            .expect("its inbox is open")
            .payload
    }

    #[tokio::test]
    async fn from_its_boot_the_sidebar_publishes_what_its_tree_shows_on_its_own_inbox() {
        let kernel = Arc::new(Kernel::new());
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
            async move { kernel.emit(&at, p).await }
        };

        // a chat's call moved: its phase, kept for the view's first read
        emit("c1", json!({"type": "status", "source": "c1", "phase": "thinking"})).await;
        assert_eq!(next(&mut rx).await, json!({"type": "status_changed", "agent": "c1", "phase": "thinking"}));
        assert_eq!(ask(json!({"type": "reflect"})).await["phases"], json!({"c1": "thinking"}));

        // a chat wrote a line: passed on with the tree state, kept for the view's first read
        emit("c1", json!({"type": "line_summary_changed", "source": "c1", "text": "reading the spec", "ts": 12.5})).await;
        assert_eq!(
            next(&mut rx).await,
            json!({"type": "line_summary_changed", "agent": "c1", "text": "reading the spec", "ts": 12.5})
        );
        assert_eq!(
            ask(json!({"type": "reflect"})).await["line_summaries"],
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
        assert_eq!(ask(json!({"type": "reflect"})).await["phases"], json!({"c1": "failed"}));
        emit("c1", json!({"type": "status", "source": "c1", "phase": "paused"})).await;
        assert_eq!(next(&mut rx).await, json!({"type": "status_changed", "agent": "c1", "phase": "paused"}));
        assert_eq!(ask(json!({"type": "reflect"})).await["phases"], json!({"c1": "paused"}));
        // resumed with nothing held: the chat is at rest
        emit("c1", json!({"type": "status", "source": "c1", "phase": "idle"})).await;
        assert_eq!(next(&mut rx).await, json!({"type": "status_changed", "agent": "c1", "phase": "idle"}));
        assert_eq!(ask(json!({"type": "reflect"})).await["phases"], json!({}));
        assert!(rx.try_recv().is_none(), "nothing else was published");

        // deleted: it follows no more
        fantastic_kernel::Bundle::on_delete(&SidebarBundle, &me, &kernel).await.unwrap();
        emit("c1", json!({"type": "status", "source": "c1", "phase": "thinking"})).await;
        kernel.publish_state(&json!({"agent_id": "c3", "kind": "added", "parent_id": "kernel"}));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(rx.try_recv().is_none(), "a deleted sidebar publishes nothing");
    }
}
