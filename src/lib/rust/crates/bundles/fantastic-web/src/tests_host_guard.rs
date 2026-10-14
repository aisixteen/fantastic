//! The Host guard on a live server (python
//! `web/host/tests/test_origin_guard.py`, the Host half): a DNS-rebinding page's
//! same-origin GET carries no Origin, only its own domain in `Host` — refused on
//! every route, the file proxy and the websocket upgrade included.

use super::*;

/// python `test_rebound_host_is_refused_on_every_method_with_no_origin` — the
/// file proxy answers a rebound Host with the coded 403.
#[tokio::test]
async fn a_rebound_host_cannot_read_the_file_proxy() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let kernel = stage_host(&tmp, port, "web_hg", "rest_hg", "fb_hg").await;
    let c = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}/fb_hg/file/hello.txt");
    let r = c.get(&url).send().await.unwrap();
    assert_eq!(r.status(), 200, "own authority is served");
    assert_eq!(r.text().await.unwrap(), "hello world\n");
    for host in [
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
        format!("127.0.0.1:{port}"),
    ] {
        let r = c.get(&url).header("Host", &host).send().await.unwrap();
        assert_eq!(r.status(), 200, "{host}");
    }
    let rebound = format!("rebind.invalid:{port}");
    let r = c.get(&url).header("Host", &rebound).send().await.unwrap();
    assert_eq!(r.status(), 403);
    assert_eq!(r.headers()["content-type"], "application/json");
    assert_eq!(
        r.text().await.unwrap(),
        format!(
            r#"{{"error": "host 'rebind.invalid:{port}' is not this server's own authority", "reason": "unauthorized", "hint": "address the host by its bound host:port (a loopback bind also answers localhost, 127.0.0.1 and [::1] on that port)"}}"#
        )
    );
    for method in [
        reqwest::Method::HEAD,
        reqwest::Method::OPTIONS,
        reqwest::Method::POST,
    ] {
        let r = c
            .request(
                method.clone(),
                format!("http://127.0.0.1:{port}/rest_hg/kernel_state"),
            )
            .header("Host", &rebound)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "{method}");
    }
    let r = c
        .get(&url)
        .header("Host", format!("127.0.0.1:{}", port.wrapping_add(1)))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403, "another port is another authority");
    stop(&kernel, "web_hg").await;
}

/// python `test_rebound_websocket_upgrade_is_refused`.
#[tokio::test]
async fn a_rebound_websocket_upgrade_is_refused() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::Error as WsError;
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let kernel = stage_web_with_ws(&tmp, port, "web_hw").await;
    let url = format!("ws://127.0.0.1:{port}/web_hw/ws");
    let mut req = url.as_str().into_client_request().unwrap();
    req.headers_mut()
        .insert("host", format!("rebind.invalid:{port}").parse().unwrap());
    match tokio_tungstenite::connect_async(req).await {
        Err(WsError::Http(resp)) => assert_eq!(resp.status(), 403),
        other => panic!("expected a refused upgrade, got {:?}", other.map(|_| ())),
    }
    tokio_tungstenite::connect_async(url.as_str())
        .await
        .expect("own authority upgrades");
    stop(&kernel, "web_hw").await;
}
