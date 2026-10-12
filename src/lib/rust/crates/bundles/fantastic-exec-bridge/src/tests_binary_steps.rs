//! `_binary_path` is a LIST OF STEPS on the GUEST WIRE (owner-approved,
//! 2026-10-02) — the twins of python
//! `io_bridge/tests/test_binary_path_steps.py` (e).
//!
//! A kind-B frame's header names where its raw body belongs. It used to spell
//! that as a dotted string, so a payload whose bytes sat at the LITERAL key
//! `"a.b"` went out as `"payload.a.b"` and came back as a NESTED `a` → `b` slot.
//! Now the path is a JSON array: a string step is an object key, taken literally;
//! an integer step is a list index. [`CONNECTOR_SRC`] (python's, byte for byte)
//! writes and reads it; the relay decodes and encodes it with the io_bridge codec.
//!
//! A child of `tests`, so it drives the same helpers.

use super::*;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{Blob, Step};
use std::sync::Mutex;

const RAW: &[u8] = b"\x00raw\xff";

/// What the probe was handed: the header, and the body's steps and bytes.
type Got = Arc<Mutex<Vec<Value>>>;

/// python `_echo`: keeps what arrived and answers with bytes at the LITERAL key
/// `x.y` beside a nested `x` → `y`, so the reply crosses the same way back.
struct BinEcho(Got);

#[async_trait::async_trait]
impl Bundle for BinEcho {
    fn name(&self) -> &str {
        "binpath_echo.tools"
    }
    async fn handle(
        &self,
        _id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        Ok(Some(
            json!({"error": format!("text channel: {}", payload["type"])}),
        ))
    }
    async fn handle_binary(
        &self,
        id: &AgentId,
        header: Value,
        body: Option<Blob>,
        kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Option<Blob>), BundleError> {
        // the kernel dispatches EVERY call here — lifecycle verbs are not a take
        if matches!(
            header["type"].as_str(),
            Some("boot" | "shutdown" | "reflect")
        ) {
            return Ok((self.handle(id, &header, kernel).await?, None));
        }
        let mut keys: Vec<String> = header
            .as_object()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        if let Some(Step::Key(k)) = body.as_ref().and_then(|b| b.path.first()) {
            keys.push(k.clone());
        }
        keys.sort();
        self.0.lock().unwrap().push(json!({
            "header": header,
            "path": body.as_ref().map(|b| fantastic_bundle::path::steps_value(&b.path)),
            "bytes": body.as_ref().map(|b| b.bytes.clone()),
        }));
        let back = fantastic_kernel::body_at(body.as_ref(), "a.b")
            .unwrap_or_default()
            .to_vec();
        Ok((
            Some(json!({"keys": keys, "x.y": null, "x": {"y": "kept"}})),
            Some(Blob::key("x.y", back)),
        ))
    }
}

/// A binary frame's header AS IT CROSSED — `_binary_path` still in it.
fn head_of(frame: &[u8]) -> Value {
    let h = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
    serde_json::from_slice(&frame[4..4 + h]).expect("a JSON header")
}

/// One frame off the parent end: `(kind, rest)`.
async fn read_frame(ctrl: &mut tokio::net::UnixStream) -> (u8, Vec<u8>) {
    use tokio::io::AsyncReadExt;
    let mut head = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(10), ctrl.read_exact(&mut head))
        .await
        .expect("no frame within 10s")
        .expect("head");
    let mut body = vec![0u8; u32::from_be_bytes(head) as usize];
    ctrl.read_exact(&mut body).await.expect("body");
    (body[0], body[1..].to_vec())
}

fn spawn_program(py: &Path, code: &str) -> crate::spawn::Spawned {
    let argv = vec![
        py.to_string_lossy().to_string(),
        "-u".to_string(),
        "-c".to_string(),
        format!("{CONNECTOR_SRC}{code}"),
    ];
    crate::spawn::spawn_job(&argv, "owner_agent", &Map::new(), ".").expect("spawn")
}

/// The frame a program writes names the literal key; a kind-B reply placed by
/// the kernel's own codec lands at the literal key in the program.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_guest_wire_carries_a_literal_key_both_ways() {
    use tokio::io::AsyncWriteExt;
    let Some(py) = find_real_python() else {
        eprintln!("skipping: no python3");
        return;
    };
    let mut sp = spawn_program(
        &py,
        "r = kernel.send('echo', {'type': 'take', 'a': {'b': 'kept'}, \
         'a.b': b'\\x00raw\\xff', 'items': [1]})\n\
         print(sorted(r), r['x.y'], r['x'])\n",
    );
    let (kind, frame) = read_frame(&mut sp.ctrl).await;
    assert_eq!(kind, b'B');
    let head = head_of(&frame);
    assert_eq!(head["_binary_path"], json!(["payload", "a.b"]), "{head}");
    assert_eq!(
        head["payload"],
        json!({"type": "take", "a": {"b": "kept"}, "a.b": null, "items": [1]})
    );
    let got = fantastic_io_bridge::codec::decode_binary_frame(&frame).expect("a frame");
    assert_eq!(got.body, RAW);
    assert_eq!(
        got.path,
        Some(vec![Step::from("payload"), Step::from("a.b")])
    );

    let reply = json!({"op": "reply", "rid": head["rid"],
                       "data": {"x.y": null, "x": {"y": "kept"}}});
    let path = [Step::from("data"), Step::from("x.y")];
    let wire = fantastic_io_bridge::codec::encode_binary_frame(&reply, &path, b"back");
    assert_eq!(head_of(&wire)["_binary_path"], json!(["data", "x.y"]));
    let body = [b"B".as_slice(), &wire].concat();
    sp.ctrl
        .write_all(&[&(body.len() as u32).to_be_bytes()[..], &body].concat())
        .await
        .expect("write");
    let out = tokio::time::timeout(Duration::from_secs(10), sp.child.wait_with_output())
        .await
        .expect("the program did not finish")
        .expect("wait");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "['x', 'x.y'] b'back' {'y': 'kept'}",
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A list index is an INTEGER step on the program's frame.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_guest_wire_carries_a_list_index_from_the_program() {
    let Some(py) = find_real_python() else {
        eprintln!("skipping: no python3");
        return;
    };
    let mut sp = spawn_program(
        &py,
        "kernel.emit({'type': 'chunks', 'items': [{'n': 0}, {'data': b'hi'}]})\n",
    );
    let (kind, frame) = read_frame(&mut sp.ctrl).await;
    assert_eq!(kind, b'B');
    assert_eq!(
        head_of(&frame)["_binary_path"],
        json!(["payload", "items", 1, "data"])
    );
    let got = fantastic_io_bridge::codec::decode_binary_frame(&frame).expect("a frame");
    assert_eq!(
        got.head,
        json!({"op": "emit", "payload": {"type": "chunks", "items": [{"n": 0}, {"data": null}]}})
    );
    assert_eq!(
        got.path,
        Some(vec![
            Step::from("payload"),
            Step::from("items"),
            Step::Index(1),
            Step::from("data")
        ])
    );
    assert_eq!(got.body, b"hi");
    let _ = tokio::time::timeout(Duration::from_secs(10), sp.child.wait()).await;
}

/// The whole guest path: program → relay (decode) → echo → relay (encode) →
/// program. The literal key arrives as ONE key, the reply's literal key comes
/// back as one key, and the kernel's mirror of the call names it too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_round_trips_a_literal_key_through_the_kernel() {
    let Some(py) = find_real_python() else {
        eprintln!("skipping: no python3");
        return;
    };
    let tmp = TempDir::new().unwrap();
    let got: Got = Arc::default();
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, ExecBridgeBundle);
    kernel
        .bundles
        .register("binpath_echo.tools", BinEcho(Arc::clone(&got)));
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
    let a = AgentId::from(agent_id_for(&tmp).as_str());
    for rec in [
        json!({"type": "create_agent", "parent_id": "kernel", "id": a.as_str(),
               "handler_module": HANDLER_MODULE,
               "ingress_rule": "allow_all_ingress", "allowed": [".*"],
               "cwd": tmp.path().to_string_lossy(),
               "can_call": {"echo": "*"}}),
        json!({"type": "create_agent", "parent_id": "kernel_state", "id": "echo",
               "handler_module": "binpath_echo.tools",
               "callable_by": {a.as_str(): "*"}}),
    ] {
        let r = kernel
            .send(vec![Item::new(AgentId::from("kernel_state"), rec)])
            .await
            .remove(0)
            .result;
        assert!(r.get("error").is_none(), "{r}");
    }
    let tap = AgentId::from("tap_binpath");
    let mut tap_rx = kernel.ensure_inbox(&tap);
    assert!(kernel
        .watch(&AgentId::from("echo"), tap.clone())
        .await
        .is_none());
    let mut rx = inbox(&kernel, &a);
    let started = kernel
        .send(vec![Item::new(
            a.clone(),
            py_start(
                &py,
                "r = kernel.send('echo', {'type': 'take', 'a': {'b': 'kept'}, \
                 'a.b': b'\\x00raw\\xff'})\n\
                 print('GOT', r['keys'], r['x.y'] == b'\\x00raw\\xff', r['x'])\n",
                true,
            ),
        )])
        .await
        .remove(0)
        .result;
    assert!(started.get("error").is_none(), "{started}");
    let done = until(&mut rx, |v| v["type"] == "job_done", 25)
        .await
        .expect("no job_done");
    assert_eq!(done["exit_code"], 0, "{done}");
    assert!(
        done["stdout"]
            .as_str()
            .unwrap_or("")
            .contains("GOT ['a', 'a.b', 'type'] True {'y': 'kept'}"),
        "{done}"
    );
    assert_eq!(
        *got.lock().unwrap(),
        vec![json!({"header": {"type": "take", "a": {"b": "kept"}},
                    "path": ["a.b"], "bytes": RAW})],
        "the body reached the handler at the literal key"
    );
    let mirrored = tap_rx.try_recv().expect("the call was mirrored");
    assert_eq!(
        serde_json::to_string(&mirrored.payload).unwrap(),
        r#"{"type":"take","a":{"b":"kept"},"a.b":null}"#
    );
    assert_eq!(mirrored.body, Some(Blob::key("a.b", RAW.to_vec())));
}
