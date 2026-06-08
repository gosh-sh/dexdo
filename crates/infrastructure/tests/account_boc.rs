// Coverage for the REST-backed `GraphqlClient::fetch_account_boc`.
//
// Account state is read through `tvm_client::account::get_account`
// (REST `GET /v2/account`), not GraphQL: the >= 1.0.0 gateway's
// `account(){info{boc}}` sub-resolver hangs server-side. The client
// picks the wire form from the server version it resolves via
// `GET /graphql` — these tests pin both forms and the not-found
// mapping against a local mock so the suite runs without a chain.
//
// The mock is a bare `TcpListener` loop (no extra dev-deps): every
// request gets recorded, `/graphql` answers the endpoint-resolution
// info query, `/v2/account` answers whatever the test configured.

use std::io::Read;
use std::io::Write;
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use dodex_infrastructure::graphql::GraphqlClient;

struct MockGateway {
    port: u16,
    /// First request line of every request served, in arrival order.
    requests: Arc<Mutex<Vec<String>>>,
}

/// Serves `/graphql` with the given `info.version` and `/v2/account`
/// with the given (status line, body) until the process exits. The
/// listener thread is detached — tests are short-lived and the OS
/// reclaims the socket.
fn spawn_mock(
    version: &'static str,
    account_status: &'static str,
    account_body: &'static str,
) -> MockGateway {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
    let port = listener.local_addr().unwrap().port();
    let requests: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 8192];
            let n = stream.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let first_line = req.lines().next().unwrap_or_default().to_string();
            seen.lock().unwrap().push(first_line.clone());

            let (status, body) = if first_line.contains("/v2/account") {
                (account_status, account_body.to_string())
            } else {
                // Endpoint resolution: any /graphql request gets the
                // info payload `Endpoint::resolve` expects.
                (
                    "200 OK",
                    format!(
                        r#"{{"data":{{"info":{{"version":"{version}","time":1700000000,"latency":1,"rempEnabled":false}}}}}}"#,
                    ),
                )
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len(),
            );
        }
    });

    MockGateway { port, requests }
}

fn client_for(mock: &MockGateway) -> GraphqlClient {
    GraphqlClient::new(format!("http://127.0.0.1:{}/graphql", mock.port), Duration::from_secs(5))
        .expect("client")
}

const ADDR: &str = "0:1010101010101010101010101010101010101010101010101010101010101010";
const ZERO_DAPP: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[tokio::test]
async fn v3_gateway_fetches_boc_with_account_id_and_zero_dapp() {
    let mock = spawn_mock("1.0.0", "200 OK", r#"{"boc":"dGVzdA=="}"#);
    let client = client_for(&mock);

    let boc = client.fetch_account_boc(ADDR).await.expect("fetch");
    assert_eq!(boc.as_deref(), Some("dGVzdA=="));

    let requests = mock.requests.lock().unwrap();
    let account_req = requests
        .iter()
        .find(|r| r.contains("/v2/account"))
        .expect("account request reached the gateway");
    // v3 wire form: bare 64-hex account_id (no `0:`), System dApp.
    assert!(
        account_req.contains(&format!("account_id={}", ADDR.trim_start_matches("0:"))),
        "account_id must be the address without its workchain prefix: {account_req}",
    );
    assert!(
        !account_req.contains("account_id=0:"),
        "workchain prefix must be stripped: {account_req}"
    );
    assert!(
        account_req.contains(&format!("dapp_id={ZERO_DAPP}")),
        "DEX contracts live under the System dApp: {account_req}",
    );
}

#[tokio::test]
async fn legacy_gateway_falls_back_to_address_form() {
    let mock = spawn_mock("0.9.0", "200 OK", r#"{"boc":"bGVnYWN5"}"#);
    let client = client_for(&mock);

    let boc = client.fetch_account_boc(ADDR).await.expect("fetch");
    assert_eq!(boc.as_deref(), Some("bGVnYWN5"));

    let requests = mock.requests.lock().unwrap();
    let account_req = requests
        .iter()
        .find(|r| r.contains("/v2/account"))
        .expect("account request reached the gateway");
    // Pre-1.0.0 servers key on the raw address and take no dapp_id.
    assert!(
        account_req.contains(&format!("address={ADDR}")),
        "legacy wire form must keep the full address: {account_req}",
    );
    assert!(!account_req.contains("dapp_id="), "legacy form sends no dapp_id: {account_req}");
}

#[tokio::test]
async fn missing_account_maps_to_none() {
    // Shape observed live on shellnet 1.0.0: a 404 whose body the client
    // folds into "Not found: Resource not found: http://…/v2/account?…".
    let mock = spawn_mock("1.0.0", "404 Not Found", "Resource not found");
    let client = client_for(&mock);

    let boc = client.fetch_account_boc(ADDR).await.expect("not-found is Ok(None), not Err");
    assert!(boc.is_none());
}

#[tokio::test]
async fn server_error_propagates_as_err_not_none() {
    // A 5xx is an outage, not a verdict on deployment — it must surface
    // as `Err` so the read path answers 503, not a false "not deployed".
    let mock = spawn_mock("1.0.0", "503 Service Unavailable", "upstream is having a bad day");
    let client = client_for(&mock);

    assert!(
        client.fetch_account_boc(ADDR).await.is_err(),
        "a 5xx must propagate as Err, not collapse to Ok(None)",
    );
}

#[tokio::test]
async fn server_error_whose_body_says_not_found_still_propagates() {
    // The exact trap a message-substring match falls into: a proxy/CDN
    // 5xx whose body contains "not found". Classification is on the HTTP
    // code (404 → NotFound), so this stays an error, not a phantom None.
    let mock = spawn_mock("1.0.0", "502 Bad Gateway", "<html><body>404 Not Found</body></html>");
    let client = client_for(&mock);

    assert!(
        client.fetch_account_boc(ADDR).await.is_err(),
        "a 5xx body containing 'not found' must NOT collapse to Ok(None)",
    );
}

/// Live smoke for the whole path against shellnet: RootPN must resolve
/// to a real BOC under the System dApp; a ghost account must map to
/// `None`. This is the read-path leg of the chain smoke ladder — run it
/// whenever the stand is redeployed:
///   cargo test -p dodex-infrastructure --test account_boc -- --ignored
#[tokio::test]
#[ignore = "requires shellnet"]
async fn live_root_pn_resolves_and_ghost_is_none() {
    let client =
        GraphqlClient::new("https://shellnet.ackinacki.org/graphql", Duration::from_secs(15))
            .expect("client");

    let boc = client.fetch_account_boc(ADDR).await.expect("fetch RootPN");
    assert!(boc.is_some(), "RootPN must exist on the stand");

    let ghost = "0:00000000000000000000000000000000000000000000000000000000000000aa";
    let none = client.fetch_account_boc(ghost).await.expect("fetch ghost");
    assert!(none.is_none(), "ghost account must map to None");
}
