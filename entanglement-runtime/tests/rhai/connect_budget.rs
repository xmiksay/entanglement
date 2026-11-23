//! #831: a lazy MCP connect a script triggers is bounded by the script's
//! remaining budget — the script gets a catchable error naming the server,
//! while the shared connect keeps going for whoever queued behind it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use entanglement_core::{
    Catalog, Holly, InMsg, LlmResponse, OutEvent, Permission, SessionId, ToolCall,
};
use entanglement_runtime::mcp::{AvailableMcp, McpServerConfig, McpToolCache};
use entanglement_runtime::tool_names::RHAI_TOOL;
use serde_json::{json, Value};

use super::unified_harness::{start, Setup};
use super::TempDir;

/// How long the slow stub sits on `initialize` — well past the script's
/// one-second budget.
const INIT_DELAY: Duration = Duration::from_secs(3);

/// The catching script: the connect error is a thrown, catchable one.
const SCRIPT: &str =
    r#"let r = "not cut"; try { mcp::slow_srv::ping(#{}); } catch (e) { r = "caught: " + e; } r"#;

/// A catch loop: 50 calls, each failure caught; returns `<caught>|<first>`.
const LOOP: &str = r#"let n = 0; let first = ""; for i in 0..50 { try { mcp::slow_srv::ping(#{}); } catch (e) { if n == 0 { first = e; } n += 1; } } "" + n + "|" + first"#;

/// A streamable-HTTP MCP server with one `ping` tool whose `initialize`
/// answers only after `delay` — with a JSON-RPC error when `fail`.
struct Stub {
    inits: AtomicUsize,
    delay: Duration,
    fail: bool,
    /// `tools/call`s answered so far; every call after the first sleeps
    /// [`LATER_CALL_DELAY`] when `slow_later_calls` is set.
    calls: AtomicUsize,
    slow_later_calls: bool,
}

/// Far past any budget a test leaves for a second call, so the bridge
/// clock — not the endpoint pacer's timing — is what cuts it.
const LATER_CALL_DELAY: Duration = Duration::from_secs(3);

async fn slow_stub(State(stub): State<Arc<Stub>>, Json(req): Json<Value>) -> Response {
    let id = req.get("id").cloned();
    match req.get("method").and_then(Value::as_str).unwrap_or("") {
        "initialize" => {
            stub.inits.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(stub.delay).await;
            if stub.fail {
                return Json(json!({"jsonrpc": "2.0", "id": id, "error": {
                    "code": -32603, "message": "backend down"}}))
                .into_response();
            }
            Json(json!({"jsonrpc": "2.0", "id": id, "result": {
                "protocolVersion": "2025-03-26", "serverInfo": {"name": "slow"}}}))
            .into_response()
        }
        "notifications/initialized" => StatusCode::ACCEPTED.into_response(),
        "tools/list" => Json(json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [
            {"name": "ping", "description": "pong", "inputSchema": {"type": "object"}}]}}))
        .into_response(),
        "tools/call" => {
            if stub.calls.fetch_add(1, Ordering::SeqCst) > 0 && stub.slow_later_calls {
                tokio::time::sleep(LATER_CALL_DELAY).await;
            }
            Json(json!({"jsonrpc": "2.0", "id": id, "result": {
                "content": [{"type": "text", "text": "pong"}]}}))
            .into_response()
        }
        _ => (StatusCode::BAD_REQUEST, "unknown").into_response(),
    }
}

/// Serve `stub`, returning its MCP URL.
async fn serve(stub: Arc<Stub>) -> String {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/mcp", post(slow_stub))
        .with_state(stub);
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    url
}

/// A model turn calling `rhai` with `script` under `timeout` seconds.
fn rhai_turn(id: &str, script: &str, timeout: u64) -> LlmResponse {
    LlmResponse {
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: RHAI_TOOL.into(),
            input: json!({ "script": script, "timeout": timeout }).to_string(),
            provider_meta: None,
        }],
        ..Default::default()
    }
}

/// A `slow-srv` server at `url`, unconnected, whose last-seen `ping` makes
/// the script namespace `mcp::slow_srv::ping`.
fn setup_for(url: &str, dir: &TempDir) -> Setup {
    let cache = McpToolCache::at(Some(dir.path.join("mcp-tools.yml")));
    cache
        .record("slow-srv", url, &["ping".to_string()])
        .unwrap();
    let cfg = McpServerConfig {
        command: None,
        args: vec![],
        env: HashMap::new(),
        url: Some(url.to_string()),
        headers: HashMap::new(),
        disabled: false,
        capabilities: HashMap::new(),
        oauth: None,
        description: None,
    };
    let mut setup = Setup::mode(Permission::Allow, &[], 0);
    setup.timeout = 1;
    setup.avail = AvailableMcp::from_config(
        &Catalog { providers: vec![] },
        &HashMap::from([("slow-srv".to_string(), cfg)]),
        vec![],
    )
    .with_tool_cache(cache);
    setup
}

/// Collect `sid`'s events to `Done`, approving every card, each stamped
/// with how long after `t0` it arrived.
async fn collect_timed(
    holly: &Holly,
    mut sub: tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
    t0: Instant,
) -> Vec<(Duration, OutEvent)> {
    let mut out = Vec::new();
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(10), sub.recv()).await {
        if ev.session() != Some(sid) {
            continue;
        }
        if let OutEvent::ToolRequest { request_id, .. } = &ev {
            holly
                .send(InMsg::Approve {
                    session: sid.clone(),
                    request_id: request_id.clone(),
                    scope: Default::default(),
                    mode: None,
                    run_all: false,
                })
                .await
                .unwrap();
        }
        let done = matches!(ev, OutEvent::Done { .. });
        out.push((t0.elapsed(), ev));
        if done {
            break;
        }
    }
    out
}

/// When the `rhai` result arrived, and its text.
fn rhai_at(events: &[(Duration, OutEvent)]) -> (Duration, String) {
    events
        .iter()
        .find_map(|(at, e)| match e {
            OutEvent::ToolOutput { tool, output, .. } if tool == RHAI_TOOL => {
                Some((*at, output.clone()))
            }
            _ => None,
        })
        .expect("a rhai result")
}

fn assert_cut_within_budget(at: Duration, out: &str) {
    assert!(out.contains("caught: "), "{out}");
    assert!(out.contains("`slow-srv`"), "{out}");
    assert!(out.contains("time budget ran out"), "{out}");
    // One-second budget plus slack for the engine and the scripted turn.
    assert!(at < Duration::from_millis(2500), "script overran: {at:?}");
}

/// The script's wait ends at its budget; the connect it started finishes in
/// the background, and the model's own call right after reuses it — the
/// server sees exactly one `initialize`.
#[tokio::test]
async fn a_slow_connect_is_cut_at_the_script_budget_and_the_model_reuses_it() {
    let stub = Arc::new(Stub {
        inits: AtomicUsize::new(0),
        delay: INIT_DELAY,
        fail: false,
        calls: AtomicUsize::new(0),
        slow_later_calls: false,
    });
    let url = serve(stub.clone()).await;
    let dir = TempDir::new("connect-budget-slow");
    let mut setup = setup_for(&url, &dir);
    // The unwrapped name core hands the runtime for an `mcp` envelope call
    // (this harness wires no surface resolver, so core would not unwrap).
    setup.follow_up = vec![LlmResponse {
        tool_calls: vec![ToolCall {
            id: "t2".into(),
            name: "mcp__slow-srv__ping".into(),
            input: "{}".into(),
            provider_meta: None,
        }],
        ..Default::default()
    }];
    let t0 = Instant::now();
    let (holly, sub, sid) = start(SCRIPT, &dir.path, setup).await;
    let events = collect_timed(&holly, sub, &sid, t0).await;
    let (at, out) = rhai_at(&events);
    assert_cut_within_budget(at, &out);
    let model_out = events
        .iter()
        .find_map(|(_, e)| match e {
            OutEvent::ToolOutput {
                request_id,
                output,
                is_error,
                ..
            } if request_id == "t2" => Some((output.clone(), *is_error)),
            _ => None,
        })
        .expect("the model's mcp result");
    assert_eq!(model_out, ("pong".to_string(), false), "{events:?}");
    assert_eq!(stub.inits.load(Ordering::SeqCst), 1, "one shared connect");
}

/// A server that accepts the TCP connection and never answers: the script
/// still gets its catchable error at its budget, not the connect ceiling.
#[tokio::test]
async fn a_blackholed_server_is_cut_at_the_script_budget() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((sock, _)) = listener.accept().await {
            held.push(sock);
        }
    });
    let dir = TempDir::new("connect-budget-blackhole");
    let t0 = Instant::now();
    let (holly, sub, sid) = start(SCRIPT, &dir.path, setup_for(&url, &dir)).await;
    let events = collect_timed(&holly, sub, &sid, t0).await;
    let (at, out) = rhai_at(&events);
    assert_cut_within_budget(at, &out);
}

/// Catch loops across successive scripts against a slow, failing server:
/// one detached connect serves every waiter, a spent budget starts none,
/// and the recorded failure holds through the cooldown — one `initialize`.
#[tokio::test]
async fn catch_loops_against_a_failing_server_connect_once() {
    let stub = Arc::new(Stub {
        inits: AtomicUsize::new(0),
        delay: Duration::from_secs(3),
        fail: true,
        calls: AtomicUsize::new(0),
        slow_later_calls: false,
    });
    let url = serve(stub.clone()).await;
    let dir = TempDir::new("connect-budget-loop");
    let mut setup = setup_for(&url, &dir);
    // t2 starts ~0.7 s in and outlives the 3 s failure by ~2.5 s.
    setup.follow_up = vec![
        rhai_turn("t2", LOOP, 5),
        rhai_turn("t3", LOOP, 1),
        rhai_turn("t4", LOOP, 1),
    ];
    let t0 = Instant::now();
    let (holly, sub, sid) = start(LOOP, &dir.path, setup).await;
    let events = collect_timed(&holly, sub, &sid, t0).await;
    let outs: Vec<String> = events
        .iter()
        .filter_map(|(_, e)| match e {
            OutEvent::ToolOutput { tool, output, .. } if tool == RHAI_TOOL => Some(output.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(outs.len(), 4, "{events:?}");
    for out in &outs {
        assert!(out.contains("50|"), "every call caught: {out}");
    }
    assert!(outs[0].contains("time budget ran out"), "{outs:?}");
    // The next run joined the same connect and saw its real cause.
    assert!(outs[1].contains("backend down"), "{outs:?}");
    // Let any straggler connect show itself before counting.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(stub.inits.load(Ordering::SeqCst), 1, "{outs:?}");
}

/// Spend a `secs`-second run's budget down to 0.25 s before its deadline —
/// under the 50 ms connect floor once the 300 ms grace is taken off. Host
/// `bash` sleeps, never a busy spin: rhai counts every loop iteration
/// against its operation cap, so a spin's length in operations depends on
/// the CPU, and a fast CI runner trips the cap.
fn burn(secs: f64) -> String {
    format!(
        "let t = timestamp(); while t.elapsed < {} {{ bash(\"sleep 0.05\"); }}",
        secs - 0.25
    )
}

/// The first `rhai` result's text.
fn first_rhai(events: &[(Duration, OutEvent)]) -> String {
    rhai_at(events).1
}

/// A spent budget starts no connect at all: the call refuses at once and
/// the server never sees an `initialize`.
#[tokio::test]
async fn a_spent_budget_starts_no_connect() {
    let stub = Arc::new(Stub {
        inits: AtomicUsize::new(0),
        delay: Duration::from_secs(3),
        fail: true,
        calls: AtomicUsize::new(0),
        slow_later_calls: false,
    });
    let url = serve(stub.clone()).await;
    let dir = TempDir::new("connect-budget-floor");
    let script = format!("{} {SCRIPT}", burn(1.0));
    let mut setup = setup_for(&url, &dir);
    setup.bash = true;
    let (holly, sub, sid) = start(&script, &dir.path, setup).await;
    let out = first_rhai(&collect_timed(&holly, sub, &sid, Instant::now()).await);
    assert!(out.contains("no connect was started"), "{out}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(stub.inits.load(Ordering::SeqCst), 0);
}

/// Once connected, a call on a spent budget is never told the server is
/// not connected: it resolves without the floor and meets only the bridge
/// clock (which, with no budget left, cuts the call itself).
#[tokio::test]
async fn a_connected_server_skips_the_floor() {
    let stub = Arc::new(Stub {
        inits: AtomicUsize::new(0),
        delay: Duration::from_millis(50),
        fail: false,
        calls: AtomicUsize::new(0),
        slow_later_calls: true,
    });
    let url = serve(stub.clone()).await;
    let dir = TempDir::new("connect-budget-fast");
    // Five seconds: the test client paces this endpoint ~1.2 s per request,
    // so the connect (three requests) plus the first call take ~3.6 s; host
    // `bash` sleeps (see `burn`) wait out the rest.
    let script = r#"
        let t = timestamp();
        let a = mcp::slow_srv::ping(#{});
        while t.elapsed < 4.75 { bash("sleep 0.05"); }
        let b = "";
        try { b = mcp::slow_srv::ping(#{}); } catch (e) { b = "caught: " + e; }
        a + "|" + b
    "#;
    let mut setup = setup_for(&url, &dir);
    setup.timeout = 5;
    setup.bash = true;
    let (holly, sub, sid) = start(script, &dir.path, setup).await;
    let out = first_rhai(&collect_timed(&holly, sub, &sid, Instant::now()).await);
    assert!(out.contains("pong|"), "the first call connected: {out}");
    for wrong in [
        "is not connected",
        "no connect was started",
        "still connecting",
    ] {
        assert!(!out.contains(wrong), "{out}");
    }
    // It resolved and ran; only the execution clock stopped it.
    assert!(out.contains("cut off at the script's time budget"), "{out}");
    assert_eq!(stub.inits.load(Ordering::SeqCst), 1);
}
