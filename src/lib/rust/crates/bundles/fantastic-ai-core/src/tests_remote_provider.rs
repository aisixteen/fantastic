//! The split: a chat completes through a provider agent it knows only by id.
use super::*;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};

/// A provider agent: `complete` streams the words of its record's `say`.
struct Echo;

struct Words(Vec<String>);

#[async_trait]
impl Provider for Words {
    async fn chat(&self, _m: &[Value], _x: Option<i64>) -> Result<ProviderStream, ProviderError> {
        let toks: Vec<Result<ProviderEvent, ProviderError>> =
            self.0.iter().map(|w| Ok(ProviderEvent::Token(w.clone()))).collect();
        Ok(Box::pin(futures_util::stream::iter(toks)))
    }
    fn model(&self) -> String {
        "echo-1".into()
    }
}

#[async_trait]
impl Bundle for Echo {
    fn name(&self) -> &str {
        "echo"
    }
    async fn handle(&self, id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Result<Reply, BundleError> {
        let build = |_m: Map<String, Value>| -> Arc<dyn Provider> { Arc::new(Words(vec!["hel".into(), "lo".into()])) };
        Ok(provider_verbs(id, payload, kernel, build, NO_LIVE).await)
    }
}

fn agent(k: &Arc<Kernel>, root: &Arc<fantastic_kernel::Agent>, id: &str, meta: Value, dir: &std::path::Path) {
    let a = fantastic_kernel::Agent::new(
        AgentId::from(id),
        Some("echo.tools".into()),
        Some(root.id.clone()),
        meta.as_object().cloned().unwrap_or_default(),
        dir.join(id),
        false,
    );
    k.register(a);
}

#[tokio::test]
async fn a_chat_completes_through_its_provider_agent_by_id() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut k = Kernel::new();
    k.bundles.register("echo.tools", Echo);
    let k = Arc::new(k);
    let root = fantastic_kernel::Agent::new(AgentId::from("kernel_state"), None, None, Map::new(), tmp.path().join(".fantastic"), false);
    k.register(Arc::clone(&root));
    k.set_root(Arc::clone(&root));
    agent(&k, &root, "p", json!({"callable_by": {"chat": "*"}}), tmp.path());
    agent(&k, &root, "chat", json!({"can_call": {"p": ["complete", "watch"]}}), tmp.path());
    let rp = RemoteProvider::new(&k, &AgentId::from("chat"), "p");
    let text: Vec<String> = fantastic_kernel::send::as_agent(&AgentId::from("chat"), async {
        let s = rp.chat(&[json!({"role": "user", "content": "hi"})], None).await.unwrap();
        s.map(|e| match e.unwrap() {
            ProviderEvent::Token(t) => t,
        })
        .collect()
        .await
    })
    .await;
    assert_eq!(text.concat(), "hello", "the provider's tokens, in order");
    assert_eq!(rp.model(), "echo-1", "the model the provider reported");
}

#[tokio::test]
async fn a_chat_without_the_grant_is_refused_by_name() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut k = Kernel::new();
    k.bundles.register("echo.tools", Echo);
    let k = Arc::new(k);
    let root = fantastic_kernel::Agent::new(AgentId::from("kernel_state"), None, None, Map::new(), tmp.path().join(".fantastic"), false);
    k.register(Arc::clone(&root));
    k.set_root(Arc::clone(&root));
    agent(&k, &root, "p", json!({}), tmp.path());
    agent(&k, &root, "chat", json!({}), tmp.path());
    let rp = RemoteProvider::new(&k, &AgentId::from("chat"), "p");
    let r = fantastic_kernel::send::as_agent(&AgentId::from("chat"), async {
        match rp.chat(&[json!({"role": "user", "content": "hi"})], None).await {
            Err(e) => e.message,
            Ok(_) => String::new(),
        }
    })
    .await;
    assert!(r.contains("cannot watch provider `p`"), "{r}");
}
