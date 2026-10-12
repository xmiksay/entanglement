//! The stdio MCP transport: a JSON-RPC 2.0 session over a server's stdio
//! (#198). One [`StdioClient`] owns a spawned server subprocess (or, in tests,
//! any `AsyncRead`/`AsyncWrite` pair), completes the `initialize` handshake, and
//! then multiplexes `tools/list` / `tools/call` requests over the single
//! stdin/stdout pipe.
//!
//! Framing is newline-delimited JSON, per the MCP stdio transport: every request,
//! response, and notification is one JSON object on its own line. A background
//! reader task demultiplexes responses back to their callers by JSON-RPC `id`;
//! notifications (no `id`) are ignored, and a server-initiated request (`ping`,
//! #727) is answered on stdin. The client keeps the subprocess alive for
//! its whole lifetime (`kill_on_drop`), so the tools it registers into the
//! [`ToolRegistry`][crate::tools::ToolRegistry] stay callable until the process
//! exits.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{oneshot, Mutex as AsyncMutex};

use super::client::{is_response, list_tools_paginated, server_request_reply, McpToolDef};

/// Protocol version we advertise on `initialize`. `2024-11-05` is the broadly
/// supported baseline; the server negotiates its own supported version in the
/// response, which we accept as long as the handshake succeeds.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Per-request ceiling. A hung server must not park a turn forever — the awaiting
/// [`McpTool`][super::tool::McpTool] surfaces a timeout as a tool-failure result
/// the model sees, rather than blocking the executor task indefinitely.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Ceiling on one newline-framed line off a server's stdout (#556): a
/// misbehaving server that never emits a newline must not grow the reader's
/// buffer without bound — mirrors the OAuth loopback reader's
/// `MAX_REQUEST_BYTES` cap (`entanglement-provider/src/oauth/loopback.rs`).
const MAX_LINE_BYTES: usize = 10 * 1024 * 1024;

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<std::result::Result<Value, String>>>>>;
/// The server's stdin, shared by request writers and the reader task's
/// server-request replies (#727). An async mutex so concurrent writers
/// serialize whole JSON lines without interleaving them on the pipe.
type Writer = Arc<AsyncMutex<Box<dyn AsyncWrite + Unpin + Send>>>;

/// A live JSON-RPC session with one stdio MCP server.
pub struct StdioClient {
    /// Server name from config — carried into every error message.
    server: String,
    /// The server's stdin (see [`Writer`]).
    stdin: Writer,
    /// In-flight requests keyed by id, answered by the reader task.
    pending: Pending,
    next_id: AtomicI64,
    /// Kept only to hold the subprocess for the client's lifetime (`kill_on_drop`
    /// group-kills it when the last owning `Arc` drops).
    _child: Option<Child>,
}

/// Assemble the server `Command`: args + per-server env over an inherited
/// environment with the provider API-key vars removed (#164 parity with
/// `bash`/`call`). `env_remove` runs before `.envs(env)`, so a per-server
/// `env:` entry that deliberately names a secret var survives the scrub.
/// Split from [`StdioClient::spawn`] so the env shape is unit-testable
/// without spawning a process.
fn build_command(
    command: &str,
    args: &[String],
    env: &HashMap<String, String>,
    secret_env: &[String],
) -> Command {
    let mut cmd = Command::new(command);
    for var in secret_env {
        cmd.env_remove(var);
    }
    crate::secret_env::scrub(&mut cmd);
    cmd.args(args)
        .envs(env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // The server's own logs go to *its* stderr; inherit so they surface
        // on the head's stderr rather than being swallowed.
        .stderr(Stdio::inherit())
        // A leaked server must die with us, not orphan (#168 spirit).
        .kill_on_drop(true);
    cmd
}

impl StdioClient {
    /// Spawn the configured server subprocess and complete the handshake.
    ///
    /// `secret_env` (the catalog's provider API-key env vars, #164) is scrubbed
    /// from the child: an MCP server is an arbitrary external process and must
    /// not inherit the engine's provider credentials, exactly like `bash`/`call`
    /// children. An explicit per-server `env:` entry still wins — the user
    /// naming a var in the server's own config is deliberate consent.
    pub async fn spawn(
        server: &str,
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
        secret_env: &[String],
    ) -> Result<Self> {
        let mut cmd = build_command(command, args, env, secret_env);
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning MCP server `{server}` (`{command}`)"))?;
        let stdin = child.stdin.take().context("MCP server has no stdin")?;
        let stdout = child.stdout.take().context("MCP server has no stdout")?;
        let client = Self::new(server.to_string(), stdin, stdout, Some(child));
        client
            .handshake()
            .await
            .with_context(|| format!("MCP server `{server}` handshake"))?;
        Ok(client)
    }

    /// Build a client over any reader/writer (subprocess pipes in production, an
    /// in-memory duplex in tests) and start the demultiplexing reader task.
    pub(crate) fn new<R, W>(server: String, writer: W, reader: R, child: Option<Child>) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let stdin: Writer = Arc::new(AsyncMutex::new(Box::new(writer)));
        spawn_reader(server.clone(), reader, pending.clone(), stdin.clone());
        Self {
            server,
            stdin,
            pending,
            next_id: AtomicI64::new(1),
            _child: child,
        }
    }

    /// `initialize` then the `notifications/initialized` ack — the MCP handshake
    /// every server requires before it will answer `tools/*`.
    async fn handshake(&self) -> Result<()> {
        self.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "skutter", "version": env!("CARGO_PKG_VERSION") },
            }),
        )
        .await
        .context("initialize")?;
        self.notify("notifications/initialized", json!({})).await?;
        Ok(())
    }

    /// Discover the server's tools, following `nextCursor` across pages (#727).
    pub async fn list_tools(&self) -> Result<Vec<McpToolDef>> {
        list_tools_paginated(|params| self.request("tools/list", params))
            .await
            .context("tools/list")
    }

    /// Invoke one tool. Returns the raw `tools/call` result object (`content` +
    /// optional `isError`) for the proxy to render.
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value> {
        self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
        .await
        .with_context(|| format!("tools/call `{name}`"))
    }

    /// Send a request and await its correlated response (or a timeout).
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("MCP pending-requests mutex poisoned")
            .insert(id, tx);
        let frame = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if let Err(e) = self.write_line(&frame).await {
            self.pending
                .lock()
                .expect("MCP pending-requests mutex poisoned")
                .remove(&id);
            return Err(e);
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(err))) => bail!("MCP server `{}` returned error: {err}", self.server),
            // Sender dropped without answering — the reader task drains pending
            // with an explanatory error on EOF, so this is the rare torn case.
            Ok(Err(_)) => bail!("MCP server `{}` closed before answering", self.server),
            Err(_) => {
                self.pending
                    .lock()
                    .expect("MCP pending-requests mutex poisoned")
                    .remove(&id);
                bail!("MCP server `{}` timed out on `{method}`", self.server)
            }
        }
    }

    /// Fire-and-forget notification (no `id`, no response expected).
    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let frame = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.write_line(&frame).await
    }

    async fn write_line(&self, frame: &Value) -> Result<()> {
        write_frame(&self.stdin, frame)
            .await
            .with_context(|| format!("writing to MCP server `{}`", self.server))
    }
}

/// Write one newline-framed JSON message, holding the lock for the whole line.
async fn write_frame(stdin: &Writer, frame: &Value) -> Result<()> {
    let mut line = serde_json::to_string(frame).context("encoding MCP message")?;
    line.push('\n');
    let mut stdin = stdin.lock().await;
    stdin.write_all(line.as_bytes()).await?;
    stdin.flush().await.context("flushing MCP server stdin")?;
    Ok(())
}

/// The demultiplexer: read newline-framed JSON, route each response to its
/// waiting caller by `id`, answer server requests (`ping`, #727), ignore
/// notifications. On EOF (server exited), drain every pending request with an
/// error so no caller hangs forever.
fn spawn_reader<R>(server: String, reader: R, pending: Pending, stdin: Writer)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut reader = BufReader::new(reader);
        loop {
            match read_line_capped(&mut reader, MAX_LINE_BYTES).await {
                Ok(Some(line)) if line.trim().is_empty() => continue,
                Ok(Some(line)) => match serde_json::from_str::<Value>(&line) {
                    Ok(msg) => {
                        if let Some(reply) = server_request_reply(&msg) {
                            // Written from its own task: a reader blocked on a
                            // full stdin pipe would stop draining stdout, and a
                            // server blocked writing stdout stops reading stdin.
                            let (stdin, server) = (stdin.clone(), server.clone());
                            tokio::spawn(async move {
                                if let Err(e) = write_frame(&stdin, &reply).await {
                                    tracing::debug!(server = %server, "MCP reply failed: {e:#}");
                                }
                            });
                        } else {
                            route(&msg, &pending);
                        }
                    }
                    Err(e) => tracing::debug!(server = %server, "unparsable MCP line: {e}"),
                },
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!(server = %server, "MCP server read error: {e}");
                    break;
                }
            }
        }
        // EOF/error: fail everything still waiting rather than leak parked turns.
        let drained: Vec<_> = pending
            .lock()
            .expect("MCP pending-requests mutex poisoned")
            .drain()
            .collect();
        for (_, tx) in drained {
            let _ = tx.send(Err(format!("MCP server `{server}` closed the connection")));
        }
    });
}

/// Read one newline-terminated line off `reader`, capped at `max_bytes` total
/// (#556) — unlike `AsyncBufReadExt::lines()`/`next_line`, which buffer
/// without limit until a `\n` shows up. A subprocess stream that never emits
/// one (hung, or spewing binary noise) would otherwise grow the buffer to an
/// OOM. Returns `Ok(None)` only on a clean EOF with nothing buffered.
async fn read_line_capped<R>(reader: &mut R, max_bytes: usize) -> Result<Option<String>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut buf: Vec<u8> = Vec::new();
    loop {
        // Copy out of the fill_buf borrow before calling `consume` — the two
        // can't be held across each other on `&mut reader`.
        let (chunk, found_newline) = {
            let available = reader
                .fill_buf()
                .await
                .context("reading MCP server stdout")?;
            if available.is_empty() {
                return Ok((!buf.is_empty()).then(|| String::from_utf8_lossy(&buf).into_owned()));
            }
            match available.iter().position(|&b| b == b'\n') {
                Some(pos) => (available[..=pos].to_vec(), true),
                None => (available.to_vec(), false),
            }
        };
        reader.consume(chunk.len());
        if found_newline {
            buf.extend_from_slice(&chunk[..chunk.len() - 1]); // drop the trailing `\n`
            return Ok(Some(String::from_utf8_lossy(&buf).into_owned()));
        }
        buf.extend_from_slice(&chunk);
        if buf.len() > max_bytes {
            bail!("MCP server line exceeded the {max_bytes}-byte cap with no newline in sight");
        }
    }
}

/// Route one parsed message: a response (has `id`, no `method`) resolves its
/// pending oneshot; a notification is dropped.
fn route(msg: &Value, pending: &Pending) {
    if !is_response(msg) {
        return; // notification (server requests are answered before this)
    }
    let Some(id) = msg.get("id").and_then(Value::as_i64) else {
        return; // a non-integer id is never one of ours
    };
    let Some(tx) = pending
        .lock()
        .expect("MCP pending-requests mutex poisoned")
        .remove(&id)
    else {
        return; // unknown / already-timed-out id
    };
    let _ = tx.send(super::client::jsonrpc_payload(msg));
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as IoBufReader};

    /// A minimal in-memory MCP server for the handshake + tool round-trip. Speaks
    /// exactly the three methods the client uses.
    async fn fake_server<R, W>(reader: R, mut writer: W)
    where
        R: AsyncRead + Unpin + Send,
        W: AsyncWrite + Unpin + Send,
    {
        let mut lines = IoBufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            let msg: Value = serde_json::from_str(&line).unwrap();
            let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
            let id = msg.get("id").cloned();
            let result = match method {
                "initialize" => Some(
                    json!({ "protocolVersion": PROTOCOL_VERSION, "serverInfo": { "name": "fake" } }),
                ),
                // Two pages (#727): the second only reachable via the cursor.
                "tools/list" => Some(match msg["params"].get("cursor") {
                    None => json!({ "tools": [
                        { "name": "echo", "description": "echoes back", "inputSchema": { "type": "object", "properties": { "text": { "type": "string" } } } }
                    ], "nextCursor": "page2" }),
                    Some(_) => json!({ "tools": [ { "name": "second" } ] }),
                }),
                "tools/call" => {
                    let text = msg
                        .get("params")
                        .and_then(|p| p.get("arguments"))
                        .and_then(|a| a.get("text"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    Some(
                        json!({ "content": [ { "type": "text", "text": format!("echo: {text}") } ] }),
                    )
                }
                // notifications/initialized has no id → no reply
                _ => None,
            };
            if let (Some(id), Some(result)) = (id, result) {
                let frame = json!({ "jsonrpc": "2.0", "id": id, "result": result });
                let mut s = serde_json::to_string(&frame).unwrap();
                s.push('\n');
                writer.write_all(s.as_bytes()).await.unwrap();
                writer.flush().await.unwrap();
            }
        }
    }

    fn wire() -> StdioClient {
        let (client_end, server_end) = tokio::io::duplex(64 * 1024);
        let (creader, cwriter) = tokio::io::split(client_end);
        let (sreader, swriter) = tokio::io::split(server_end);
        tokio::spawn(fake_server(sreader, swriter));
        StdioClient::new("fake".to_string(), cwriter, creader, None)
    }

    #[tokio::test]
    async fn handshake_then_lists_tools() {
        let client = wire();
        client.handshake().await.unwrap();
        let tools = client.list_tools().await.unwrap();
        // Both pages land: the second only via `nextCursor` (#727).
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "echo");
        assert_eq!(tools[0].description, "echoes back");
        assert_eq!(tools[1].name, "second");
    }

    /// A server `ping` mid-call (#727) — reusing the very id our `tools/call`
    /// is waiting on — is answered with an empty result on stdin and does not
    /// resolve the pending call; the real response that follows does.
    #[tokio::test]
    async fn answers_a_server_ping_without_confusing_it_for_our_response() {
        let (client_end, server_end) = tokio::io::duplex(64 * 1024);
        let (creader, cwriter) = tokio::io::split(client_end);
        let (sreader, mut swriter) = tokio::io::split(server_end);
        let server = tokio::spawn(async move {
            let mut lines = IoBufReader::new(sreader).lines();
            let call: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            let id = call["id"].clone();
            let send = |v: Value| format!("{v}\n");
            let ping = send(json!({ "jsonrpc": "2.0", "id": id, "method": "ping" }));
            swriter.write_all(ping.as_bytes()).await.unwrap();
            let reply: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            let result = json!({ "content": [ { "type": "text", "text": "done" } ] });
            let answer = send(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
            swriter.write_all(answer.as_bytes()).await.unwrap();
            reply
        });
        let client = StdioClient::new("pinger".to_string(), cwriter, creader, None);
        let res = client.call_tool("x", json!({})).await.unwrap();
        assert_eq!(res["content"][0]["text"], "done");
        let reply = server.await.unwrap();
        assert_eq!(reply, json!({ "jsonrpc": "2.0", "id": 1, "result": {} }));
    }

    #[tokio::test]
    async fn calls_a_tool_and_gets_content() {
        let client = wire();
        client.handshake().await.unwrap();
        let res = client
            .call_tool("echo", json!({ "text": "hi" }))
            .await
            .unwrap();
        let text = res["content"][0]["text"].as_str().unwrap();
        assert_eq!(text, "echo: hi");
    }

    /// ADR-0227: an `http_auth` secret is hidden from an MCP stdio server too.
    #[test]
    fn build_command_scrubs_hidden_http_auth_vars() {
        crate::secret_env::hide(["ENTANGLEMENT_TEST_STDIO_HIDDEN".to_string()]);
        let cmd = build_command("srv-bin", &[], &HashMap::new(), &[]);
        let removed = cmd
            .as_std()
            .get_envs()
            .any(|(k, v)| k == "ENTANGLEMENT_TEST_STDIO_HIDDEN" && v.is_none());
        assert!(removed);
    }

    #[test]
    fn build_command_scrubs_secret_env_but_explicit_server_env_wins() {
        let mut env = HashMap::new();
        env.insert("SERVER_TOKEN".to_string(), "abc".to_string());
        env.insert("ZAI_API_KEY".to_string(), "explicit".to_string());
        let secret = vec!["ZAI_API_KEY".to_string(), "ANTHROPIC_API_KEY".to_string()];
        let cmd = build_command("srv-bin", &[], &env, &secret);
        let envs: HashMap<_, _> = cmd
            .as_std()
            .get_envs()
            .map(|(k, v)| (k.to_os_string(), v.map(|v| v.to_os_string())))
            .collect();
        // Scrubbed: the inherited key is explicitly removed…
        assert_eq!(
            envs.get("ANTHROPIC_API_KEY".as_ref() as &std::ffi::OsStr),
            Some(&None)
        );
        // …the per-server `env:` entry re-adds its own value (applied after the
        // scrub, so deliberate config wins)…
        assert_eq!(
            envs.get("ZAI_API_KEY".as_ref() as &std::ffi::OsStr),
            Some(&Some("explicit".into()))
        );
        // …and unrelated per-server vars pass through untouched.
        assert_eq!(
            envs.get("SERVER_TOKEN".as_ref() as &std::ffi::OsStr),
            Some(&Some("abc".into()))
        );
    }

    #[tokio::test]
    async fn read_line_capped_reads_a_full_line_and_stops_before_the_next() {
        let data = std::io::Cursor::new(b"hello world\nsecond line\n".to_vec());
        let mut reader = BufReader::new(data);
        let line = read_line_capped(&mut reader, 1024).await.unwrap();
        assert_eq!(line.as_deref(), Some("hello world"));
        let line = read_line_capped(&mut reader, 1024).await.unwrap();
        assert_eq!(line.as_deref(), Some("second line"));
    }

    #[tokio::test]
    async fn read_line_capped_returns_none_at_clean_eof() {
        let data = std::io::Cursor::new(Vec::new());
        let mut reader = BufReader::new(data);
        assert!(read_line_capped(&mut reader, 1024).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn read_line_capped_errors_past_the_cap_with_no_newline() {
        // A newline-free stream (a hung/misbehaving server) must error out
        // rather than growing the buffer forever (#556).
        let data = std::io::Cursor::new(vec![b'a'; 100]);
        let mut reader = BufReader::new(data);
        let err = read_line_capped(&mut reader, 16).await.unwrap_err();
        assert!(err.to_string().contains("cap"), "{err}");
    }

    #[tokio::test]
    async fn request_to_dead_server_errors_not_hangs() {
        // No server task: the write side of the duplex closes immediately when
        // its peer drops, so the reader hits EOF and drains pending with an error.
        let (client_end, _drop_server) = tokio::io::duplex(1024);
        let (creader, cwriter) = tokio::io::split(client_end);
        drop(_drop_server);
        let client = StdioClient::new("dead".to_string(), cwriter, creader, None);
        let err = client.list_tools().await.unwrap_err();
        // `{:#}` walks the full context chain — the server name lives on a lower
        // link than the outer `tools/list` context.
        assert!(format!("{err:#}").contains("dead"), "got: {err:#}");
    }
}
