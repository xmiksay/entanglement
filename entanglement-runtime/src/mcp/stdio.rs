//! The stdio MCP transport: a JSON-RPC 2.0 session over a server's stdio
//! (#198). One [`StdioClient`] owns a spawned server subprocess (or, in tests,
//! any `AsyncRead`/`AsyncWrite` pair), completes the `initialize` handshake, and
//! then multiplexes `tools/list` / `tools/call` requests over the single
//! stdin/stdout pipe.
//!
//! Framing is newline-delimited JSON, per the MCP stdio transport: every request,
//! response, and notification is one JSON object on its own line. A background
//! reader task demultiplexes responses back to their callers by JSON-RPC `id`;
//! notifications (no `id`) are ignored. The client keeps the subprocess alive for
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

use super::client::{parse_tool_def, McpToolDef};

/// Protocol version we advertise on `initialize`. `2024-11-05` is the broadly
/// supported baseline; the server negotiates its own supported version in the
/// response, which we accept as long as the handshake succeeds.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Per-request ceiling. A hung server must not park a turn forever — the awaiting
/// [`McpTool`][super::tool::McpTool] surfaces a timeout as a tool-failure result
/// the model sees, rather than blocking the executor task indefinitely.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<std::result::Result<Value, String>>>>>;

/// A live JSON-RPC session with one stdio MCP server.
pub struct StdioClient {
    /// Server name from config — carried into every error message.
    server: String,
    /// The server's stdin. Behind an async mutex so concurrent `tools/call`s
    /// serialize their writes without interleaving JSON lines on the pipe.
    stdin: AsyncMutex<Box<dyn AsyncWrite + Unpin + Send>>,
    /// In-flight requests keyed by id, answered by the reader task.
    pending: Pending,
    next_id: AtomicI64,
    /// Kept only to hold the subprocess for the client's lifetime (`kill_on_drop`
    /// group-kills it when the last owning `Arc` drops).
    _child: Option<Child>,
}

impl StdioClient {
    /// Spawn the configured server subprocess and complete the handshake.
    pub async fn spawn(
        server: &str,
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
    ) -> Result<Self> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // The server's own logs go to *its* stderr; inherit so they surface
            // on the head's stderr rather than being swallowed.
            .stderr(Stdio::inherit())
            // A leaked server must die with us, not orphan (#168 spirit).
            .kill_on_drop(true);
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
        spawn_reader(server.clone(), reader, pending.clone());
        Self {
            server,
            stdin: AsyncMutex::new(Box::new(writer)),
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

    /// Discover the server's tools.
    pub async fn list_tools(&self) -> Result<Vec<McpToolDef>> {
        let res = self
            .request("tools/list", json!({}))
            .await
            .context("tools/list")?;
        let tools = res
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(tools.iter().filter_map(parse_tool_def).collect())
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
        self.pending.lock().unwrap().insert(id, tx);
        let frame = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if let Err(e) = self.write_line(&frame).await {
            self.pending.lock().unwrap().remove(&id);
            return Err(e);
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(err))) => bail!("MCP server `{}` returned error: {err}", self.server),
            // Sender dropped without answering — the reader task drains pending
            // with an explanatory error on EOF, so this is the rare torn case.
            Ok(Err(_)) => bail!("MCP server `{}` closed before answering", self.server),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
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
        let mut line = serde_json::to_string(frame).context("encoding MCP request")?;
        line.push('\n');
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(line.as_bytes())
            .await
            .with_context(|| format!("writing to MCP server `{}`", self.server))?;
        stdin.flush().await.context("flushing MCP server stdin")?;
        Ok(())
    }
}

/// The demultiplexer: read newline-framed JSON, route each response to its
/// waiting caller by `id`, ignore notifications. On EOF (server exited), drain
/// every pending request with an error so no caller hangs forever.
fn spawn_reader<R>(server: String, reader: R, pending: Pending)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) if line.trim().is_empty() => continue,
                Ok(Some(line)) => match serde_json::from_str::<Value>(&line) {
                    Ok(msg) => route(&msg, &pending),
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
        let drained: Vec<_> = pending.lock().unwrap().drain().collect();
        for (_, tx) in drained {
            let _ = tx.send(Err(format!("MCP server `{server}` closed the connection")));
        }
    });
}

/// Route one parsed message: a response (has `id`) resolves its pending oneshot;
/// a notification is dropped.
fn route(msg: &Value, pending: &Pending) {
    let Some(id) = msg.get("id").and_then(Value::as_i64) else {
        return; // notification
    };
    let Some(tx) = pending.lock().unwrap().remove(&id) else {
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
                "tools/list" => Some(json!({ "tools": [
                    { "name": "echo", "description": "echoes back", "inputSchema": { "type": "object", "properties": { "text": { "type": "string" } } } }
                ] })),
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
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        assert_eq!(tools[0].description, "echoes back");
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
