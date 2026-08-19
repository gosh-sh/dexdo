// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
//! Wire-level coverage for `GraphqlClient::fetch_events`: what actually reaches
//! the gateway, and what an operator is told when the gateway refuses.
//!
//! The unit tests beside the client cover `after_or_earliest` and the response
//! shapes in isolation, which leaves the one line that matters untested — the
//! `"after"` variable in the request payload. Passing `after` there verbatim
//! instead of the resolved value is what wedged mainnet for a week, and it type-
//! checks, so only a test that reads the request body can pin it.
//!
//! The mock is a bare `TcpListener` loop, matching `tests/account_boc.rs` — no
//! extra dev-dependency, and the request body is captured for assertions.

use std::io::Read;
use std::io::Write;
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use dodex_infrastructure::graphql::GraphqlClient;

struct MockGateway {
    port: u16,
    /// Body of every request served, in arrival order.
    bodies: Arc<Mutex<Vec<String>>>,
}

/// Answers every request with `status` and `body`, recording what was posted.
fn spawn_mock(status: &'static str, body: &'static str) -> MockGateway {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
    let port = listener.local_addr().unwrap().port();
    let bodies: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let seen = bodies.clone();

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 16384];
            let n = stream.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let posted = req.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default();
            seen.lock().unwrap().push(posted);

            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len(),
            );
        }
    });

    MockGateway { port, bodies }
}

fn client_for(mock: &MockGateway) -> GraphqlClient {
    GraphqlClient::new(format!("http://127.0.0.1:{}/graphql", mock.port), Duration::from_secs(5))
        .expect("client")
}

const EMPTY_PAGE: &str = r#"{"data":{"blockchain":{"events":{"edges":[],"pageInfo":{"endCursor":null,"hasNextPage":false}}}}}"#;

/// The `after` variable the client actually put on the wire.
fn after_sent(mock: &MockGateway) -> String {
    let bodies = mock.bodies.lock().unwrap();
    let body = bodies.last().expect("a request was served");
    let v: serde_json::Value = serde_json::from_str(body).expect("request body is json");
    v["variables"]["after"].to_string()
}

#[tokio::test]
async fn cold_start_puts_the_sentinel_on_the_wire_not_null() {
    let mock = spawn_mock("200 OK", EMPTY_PAGE);
    client_for(&mock).fetch_events(100, None).await.expect("fetch");

    // The whole point of the fix: `after: null` is a query this gateway family
    // cannot answer on a large chain, so it must never leave the client.
    assert_eq!(after_sent(&mock), "\"0\"");
}

#[tokio::test]
async fn a_stored_empty_cursor_is_also_replaced_on_the_wire() {
    let mock = spawn_mock("200 OK", EMPTY_PAGE);
    client_for(&mock).fetch_events(100, Some("")).await.expect("fetch");

    assert_eq!(after_sent(&mock), "\"0\"");
}

#[tokio::test]
async fn a_real_cursor_is_passed_through_untouched() {
    let mock = spawn_mock("200 OK", EMPTY_PAGE);
    let cursor = "76a83e6cf00670000000000000000000000000000000000000000000000000000";
    client_for(&mock).fetch_events(100, Some(cursor)).await.expect("fetch");

    assert_eq!(after_sent(&mock), format!("\"{cursor}\""));
}

#[tokio::test]
async fn an_http_error_carries_the_gateway_reason_not_just_the_status() {
    // The shape a restricted gateway returns: the reason is in the body, and
    // `error_for_status` would have thrown it away.
    let mock = spawn_mock(
        "403 Forbidden",
        r#"{"error":"GraphQL field is outside the Dexdo read surface"}"#,
    );

    let err = client_for(&mock).fetch_events(100, None).await.expect_err("403 must fail");
    let rendered = format!("{err:#}");

    assert!(rendered.contains("403"), "status is reported: {rendered}");
    assert!(
        rendered.contains("outside the Dexdo read surface"),
        "the gateway's own reason reaches the operator: {rendered}"
    );
}

#[tokio::test]
async fn a_resolver_error_is_reported_by_its_message() {
    // HTTP 200 with `blockchain: null` and the reason in `errors` — the exact
    // mainnet timeout response that used to surface as "not valid json".
    let mock = spawn_mock(
        "200 OK",
        r#"{"data":{"blockchain":null},"errors":[{"message":"Request timeout","path":["blockchain","events"]}]}"#,
    );

    let err = client_for(&mock).fetch_events(100, None).await.expect_err("resolver error");
    let rendered = format!("{err:#}");

    assert!(rendered.contains("Request timeout"), "gateway reason reaches the log: {rendered}");
    assert!(
        !rendered.contains("not valid json"),
        "the deserialization error must not mask it: {rendered}"
    );
}

#[tokio::test]
async fn a_non_json_body_is_echoed_so_the_operator_can_see_what_arrived() {
    let mock = spawn_mock("200 OK", "<html><body>502 upstream</body></html>");

    let err = client_for(&mock).fetch_events(100, None).await.expect_err("html is not json");
    let rendered = format!("{err:#}");

    assert!(rendered.contains("not valid json"), "{rendered}");
    assert!(rendered.contains("502 upstream"), "the body itself is shown: {rendered}");
}
