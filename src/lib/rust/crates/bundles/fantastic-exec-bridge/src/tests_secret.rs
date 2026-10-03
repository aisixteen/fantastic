//! Secret marks on the GUEST WIRE — the twins of python
//! `exec_bridge/tests/test_exec_bridge.py`'s "secret marks on the guest wire".
//!
//! A program calls `kernel.send(target, payload, secret=[…])`; the connector puts
//! `secret` on the frame only when it is given and non-empty, and the relay copies
//! it onto the dispatched item as sent. The kernel validates the marks (a bad one
//! is its refusal, in the reply) and records `<secret>` in every record of the
//! call; the program gets the real values back.
//!
//! A child of `tests`, so it drives the same helpers.

use super::*;

/// The frames a program writes for each `kernel.send` (kind T, JSON) — read off
/// the parent end, each answered with an empty reply so the program goes on.
async fn read_t_frame(ctrl: &mut tokio::net::UnixStream) -> Value {
    use tokio::io::AsyncReadExt;
    let mut head = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(10), ctrl.read_exact(&mut head))
        .await
        .expect("no frame within 10s")
        .expect("head");
    let mut body = vec![0u8; u32::from_be_bytes(head) as usize];
    ctrl.read_exact(&mut body).await.expect("body");
    assert_eq!(
        body.first(),
        Some(&b'T'),
        "{:?}",
        &body[..body.len().min(8)]
    );
    serde_json::from_slice(&body[1..]).expect("json")
}

/// `kernel.send(target, payload=None, secret=None)`: the frame carries
/// `"secret": [...]` exactly as given, and no `secret` key at all when the program
/// marks nothing (None or an empty list).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_connector_puts_secret_on_the_frame_only_when_given() {
    let Some(py) = find_real_python() else {
        eprintln!("skipping: no python3");
        return;
    };
    let program = format!(
        "{CONNECTOR_SRC}\
         kernel.send('x', {{'type': 'y', 'k': 'v'}}, secret=['payload.k', 'result'])\n\
         kernel.send('x', {{'type': 'y'}})\n\
         kernel.send('x', {{'type': 'y'}}, secret=[])\n\
         kernel.send('x', {{'type': 'y', 'a.b': 'v'}}, secret=[['payload', 'a.b'], \
         'result', ['result', 'items', 0]])\n\
         print('DONE')\n"
    );
    let argv = vec![
        py.to_string_lossy().to_string(),
        "-u".to_string(),
        "-c".to_string(),
        program,
    ];
    let mut sp = crate::spawn::spawn_job(&argv, "owner_agent", &Map::new(), ".").expect("spawn");
    let mut frames = Vec::new();
    for _ in 0..4 {
        let m = read_t_frame(&mut sp.ctrl).await;
        wire_write(
            &mut sp.ctrl,
            &json!({"op": "reply", "rid": m["rid"], "data": {}}),
        )
        .await;
        frames.push(m);
    }
    let out = tokio::time::timeout(Duration::from_secs(10), sp.child.wait_with_output())
        .await
        .expect("the program did not finish within 10s")
        .expect("wait");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("DONE"),
        "{text} / {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        frames,
        vec![
            json!({"op": "send", "rid": "1", "target": "x",
                   "payload": {"type": "y", "k": "v"},
                   "secret": ["payload.k", "result"]}),
            json!({"op": "send", "rid": "2", "target": "x", "payload": {"type": "y"}}),
            json!({"op": "send", "rid": "3", "target": "x", "payload": {"type": "y"}}),
            json!({"op": "send", "rid": "4", "target": "x",
                   "payload": {"type": "y", "a.b": "v"},
                   "secret": [["payload", "a.b"], "result", ["result", "items", 0]]}),
        ],
    );
}

/// The relay copies `secret` onto the item as sent: the kernel checks it (a mark
/// naming nothing is ITS refusal, in the reply) and records the call masked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_relay_puts_the_frames_secret_on_the_dispatched_item() {
    let tmp = TempDir::new().unwrap();
    let probe = format!("{}_probe", agent_id_for(&tmp));
    let mut extra = Map::new();
    extra.insert("ingress_rule".to_string(), json!("allow_all_ingress"));
    extra.insert("allowed".to_string(), json!(["x"]));
    extra.insert("can_call".to_string(), json!({probe.as_str(): "*"}));
    let (kernel, a) = mk_kernel_with(&tmp, extra).await;
    let made = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "create_agent", "parent_id": "kernel", "id": probe,
                   "handler_module": HANDLER_MODULE, "ingress_rule": "allow_all_ingress",
                   "cwd": tmp.path().to_string_lossy(), "allowed": ["x"],
                   "callable_by": {a.as_str(): "*"}}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(made["id"], json!(probe), "{made}");
    let (mut child, stop, relay) = wire(&kernel, &a, 9007);
    wire_write(
        &mut child,
        &json!({"op": "send", "rid": "1", "target": probe,
                "payload": {"type": "status", "note": "hush-0451"},
                "secret": ["payload.note"]}),
    )
    .await;
    let rep = wire_read(&mut child).await;
    assert!(rep["op"] == "reply" && rep["rid"] == "1", "{rep}");
    assert!(rep["data"].get("jobs").is_some(), "{rep}");
    let rows: Vec<Value> = kernel
        .rows
        .all()
        .into_iter()
        .map(Value::Object)
        .filter(|r| {
            r["kind"] == "call" && r["agent"] == probe.as_str() && r["caller"] == a.as_str()
        })
        .collect();
    assert_eq!(
        rows.iter().map(|r| r["secret"].clone()).collect::<Vec<_>>(),
        vec![json!(["payload.note"])],
        "{rows:?}"
    );
    assert_eq!(rows[0]["payload"]["note"], "<secret>", "{rows:?}");
    let all = Value::Array(kernel.rows.all().into_iter().map(Value::Object).collect());
    assert!(!all.to_string().contains("hush-0451"), "{all}");
    wire_write(
        &mut child,
        &json!({"op": "send", "rid": "2", "target": probe,
                "payload": {"type": "status"}, "secret": ["payload.nope"]}),
    )
    .await;
    let bad = wire_read(&mut child).await;
    assert_eq!(bad["rid"], "2", "{bad}");
    assert_eq!(
        (&bad["data"]["reason"], &bad["data"]["arg"]),
        (&json!("bad_arg"), &json!("secret")),
        "{bad}"
    );
    // an ARRAY mark rides as given: the literal key "a.b" is masked
    wire_write(
        &mut child,
        &json!({"op": "send", "rid": "3", "target": probe,
                "payload": {"type": "status", "a.b": "hush-dotted-0452"},
                "secret": [["payload", "a.b"]]}),
    )
    .await;
    let rep = wire_read(&mut child).await;
    assert!(
        rep["rid"] == "3" && rep["data"].get("jobs").is_some(),
        "{rep}"
    );
    let dotted: Vec<Value> = kernel
        .rows
        .all()
        .into_iter()
        .map(Value::Object)
        .filter(|r| r["kind"] == "call" && r["secret"] == json!([["payload", "a.b"]]))
        .collect();
    assert_eq!(dotted.len(), 1, "{dotted:?}");
    assert_eq!(dotted[0]["payload"]["a.b"], "<secret>", "{dotted:?}");
    let all = Value::Array(kernel.rows.all().into_iter().map(Value::Object).collect());
    assert!(!all.to_string().contains("hush-dotted-0452"), "{all}");
    stop.notify_one();
    tokio::time::timeout(Duration::from_secs(5), relay)
        .await
        .expect("relay did not stop")
        .expect("relay");
}

const CREDS_CHILD: &str = "import json, secrets\n\
key = 'sk-guest-' + secrets.token_hex(16)\n\
s = kernel.send('creds', {'type': 'set', 'ref': 'guest', \
'fields': {'api_key': key}}, \
secret=['payload.fields'])\n\
g = kernel.send('creds', {'type': 'get', 'ref': 'guest'}, secret=['result'])\n\
u = kernel.send('creds', {'type': 'get', 'ref': 'guest'})\n\
print(json.dumps({'stored': s.get('stored'), \
'match': (g.get('fields') or {}).get('api_key') == key, \
'unmarked': [u.get('reason'), u.get('arg')]}), flush=True)\n";

/// A spawned program stores and reads a credential over the guest wire with the
/// same marks any caller uses: it gets the real value back, every record of the
/// calls shows `<secret>`, and an unmarked `get` is refused. The key is minted
/// INSIDE the program, so the only copies are the store's file and the program.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_program_marks_a_credential_store_set_and_get() {
    let Some(py) = find_real_python() else {
        eprintln!("skipping: no python3");
        return;
    };
    let tmp = TempDir::new().unwrap();
    let runner = agent_id_for(&tmp);
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, ExecBridgeBundle);
    kernel.bundles.register(
        fantastic_credential_store::HANDLER_MODULE,
        fantastic_credential_store::test_support::bundle(),
    );
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let op = |p: Value| {
        let k = Arc::clone(&kernel);
        async move {
            k.send(vec![Item::new(AgentId::from("kernel"), p)])
                .await
                .remove(0)
                .result
        }
    };
    let file = tmp.path().join("creds.yaml");
    let made = op(json!({"type": "create_agent", "parent_id": "kernel",
                         "handler_module": fantastic_credential_store::HANDLER_MODULE,
                         "id": "creds", "file": file.to_string_lossy(),
                         "refs": {"guest": {"host": "api.example.com",
                                            "readers": [runner.as_str()],
                                            "writers": [runner.as_str()]}},
                         "callable_by": {runner.as_str(): "*"}}))
    .await;
    assert_eq!(made["id"], json!("creds"), "{made}");
    let made = op(json!({"type": "create_agent", "parent_id": "kernel",
                         "handler_module": HANDLER_MODULE, "id": runner,
                         "ingress_rule": "allow_all_ingress",
                         "cwd": tmp.path().to_string_lossy(), "inherit_env": ["PATH"],
                         "allowed": [format!("{} -c .*", py.display())],
                         "can_call": {"creds": "*"}}))
    .await;
    assert_eq!(made["id"], json!(runner), "{made}");
    let seen: Arc<std::sync::Mutex<Vec<Value>>> = Arc::default();
    let tap = Arc::clone(&seen);
    let _sub = kernel.add_state_subscriber(Arc::new(move |ev: &Value, _body| {
        tap.lock().unwrap().push(ev.clone());
    }));
    let a = AgentId::from(runner.as_str());
    let started = kernel
        .send(vec![Item::new(
            a.clone(),
            json!({"type": "start", "command": py.to_string_lossy(),
                   "args": ["-c", format!("{CONNECTOR_SRC}{CREDS_CHILD}")]}),
        )])
        .await
        .remove(0)
        .result;
    let job_id = started["job_id"].clone();
    assert!(job_id.is_i64(), "{started}");
    // python `_wait_done`: poll status until the job leaves `running`
    let mut row = Value::Null;
    for _ in 0..300 {
        let st = kernel
            .send(vec![Item::new(
                a.clone(),
                json!({"type": "status", "job_id": job_id}),
            )])
            .await
            .remove(0)
            .result;
        row = st["jobs"][0].clone();
        if row["status"] != "running" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_ne!(row["status"], "running", "the job never finished: {row}");
    assert_eq!(row["exit_code"], 0, "{row}");
    let last: Value =
        serde_json::from_str(row["last_line"].as_str().unwrap_or("")).expect("a JSON last line");
    assert_eq!(
        last,
        json!({"stored": true, "match": true, "unmarked": ["bad_arg", "secret"]}),
        "{row}"
    );
    let doc = fantastic_kernel::yaml_io::load(&std::fs::read_to_string(&file).unwrap())
        .expect("the store's file");
    // the value at rest carries the ref's declared host (audit S3)
    assert_eq!(
        doc["refs"]["guest"]["host"],
        json!("api.example.com"),
        "{doc}"
    );
    let key = doc["refs"]["guest"]["fields"]["api_key"]
        .as_str()
        .expect("the stored key")
        .to_string();
    assert!(key.starts_with("sk-guest-"), "{key}");
    let calls: std::collections::HashMap<String, Value> = kernel
        .rows
        .all()
        .into_iter()
        .map(Value::Object)
        .filter(|r| {
            r["kind"] == "call"
                && r["agent"] == "creds"
                && r.get("secret").is_some_and(|s| !s.is_null())
        })
        .map(|r| (r["verb"].as_str().unwrap_or("").to_string(), r))
        .collect();
    assert_eq!(
        calls["set"]["secret"],
        json!(["payload.fields"]),
        "{calls:?}"
    );
    assert_eq!(calls["get"]["secret"], json!(["result"]), "{calls:?}");
    assert_eq!(
        calls["set"]["payload"]["fields"], "<secret>",
        "{:?}",
        calls["set"]
    );
    let ledger = op(json!({"type": "ledger"})).await;
    let records = Value::Array(
        kernel
            .agents
            .iter()
            .map(|e| Value::Object(e.meta.read().unwrap().clone()))
            .collect(),
    );
    let stream = Value::Array(seen.lock().unwrap().clone());
    let rows = Value::Array(kernel.rows.all().into_iter().map(Value::Object).collect());
    for (where_, what) in [
        ("rows", &rows),
        ("ledger", &ledger),
        ("state stream", &stream),
        ("records", &records),
        ("job row", &row),
    ] {
        assert!(
            !what.to_string().contains(&key),
            "the key reached the {where_}"
        );
    }
}
