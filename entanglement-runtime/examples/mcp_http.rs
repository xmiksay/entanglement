//! Programmatic `mcp::HttpClient` assembly (issue #364, follow-up to #312).
//!
//! `docs/embedding.md` documents this seam — an embedder building a per-tenant
//! remote MCP client directly, bypassing the YAML `mcp:` config path — but it
//! can never appear in `examples/embedded.rs`: that example builds
//! `--no-default-features`, which compiles the `mcp-http` feature (and
//! `HttpClient`) out entirely. This sibling is gated `required-features =
//! ["mcp-http"]` (Cargo.toml), so it only builds under the feature — but that
//! feature is on by default, so `make lint`'s default-features `clippy
//! --all-targets` pass compiles it on every change, guarding the type/method
//! signatures below against silent drift even with no live server to connect
//! to in CI.
//!
//! Set `MCP_HTTP_URL` (and optionally `MCP_HTTP_TOKEN`) to actually connect and
//! list a real server's tools; with neither set, the example still exercises
//! every type in the assembly path and exits cleanly — compilation is the
//! guard, not a live connection.
//!
//! Run with `MCP_HTTP_URL=https://example.com/mcp cargo run -p
//! entanglement-runtime --example mcp_http --features mcp-http`.

use std::collections::HashMap;
use std::sync::Arc;

use entanglement_runtime::mcp::{HttpClient, McpClient, McpTool};
use entanglement_runtime::ToolRegistry;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let Ok(url) = std::env::var("MCP_HTTP_URL") else {
        println!(
            "MCP_HTTP_URL not set — skipping the live connect. Set it (and \
             optionally MCP_HTTP_TOKEN) to list a real server's tools through \
             this assembly path."
        );
        return Ok(());
    };

    // Per-tenant auth: a real embedder reads each user's token from its own
    // store rather than the environment, building one client per tenant. This
    // is exactly what the YAML `mcp:` config path can't do — it has one static
    // `headers` map per server, not one per caller.
    let mut headers = HashMap::new();
    if let Ok(token) = std::env::var("MCP_HTTP_TOKEN") {
        headers.insert("Authorization".to_string(), format!("Bearer {token}"));
    }

    let http = HttpClient::connect("example", &url, &headers).await?;
    let client: Arc<McpClient> = Arc::new(McpClient::Http(http));

    let mut registry = ToolRegistry::new();
    for def in client.list_tools().await? {
        println!("registering mcp tool `{}`", def.name);
        registry.register(McpTool::new(client.clone(), "example", def));
    }

    let names: Vec<String> = registry.specs().into_iter().map(|s| s.name).collect();
    println!("advertised tool specs: {names:?}");
    Ok(())
}
