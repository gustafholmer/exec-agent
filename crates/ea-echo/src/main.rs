//! `ea-echo` -- a minimal MCP server spoken over stdio.
//!
//! This binary exists only so the daemon's connector registry can be tested
//! against a real MCP peer rather than a mock: it exercises the genuine
//! handshake, the real `tools/list` and `tools/call` round trips, and the two
//! distinct failure shapes the protocol has (a tool that reports failure in
//! its result versus a request that never answers).
//!
//! Tools:
//! - `echo`       -- returns its `text` argument unchanged
//! - `explode`    -- always fails, as a `CallToolResult` with `is_error: true`
//! - `hang`       -- never returns, so callers must rely on their own timeout
//! - `watch_poll` -- returns an empty JSON array, standing in for the
//!   change-polling tool every real connector will expose

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerConfig};
use rmcp::transport::stdio;
use rmcp::{schemars, tool, tool_handler, tool_router, ServerHandler, ServiceExt};

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct EchoArgs {
    /// The text to return unchanged.
    pub text: String,
}

#[derive(Clone)]
pub struct EchoServer {
    #[expect(
        dead_code,
        reason = "read by the code the #[tool_handler] macro generates"
    )]
    tool_router: ToolRouter<Self>,
}

impl EchoServer {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }
}

impl Default for EchoServer {
    fn default() -> Self {
        Self::new()
    }
}

#[tool_router]
impl EchoServer {
    #[tool(description = "Return the `text` argument unchanged")]
    async fn echo(&self, Parameters(EchoArgs { text }): Parameters<EchoArgs>) -> String {
        text
    }

    /// Returning `Err(String)` makes `rmcp` answer with a successful JSON-RPC
    /// response whose `CallToolResult` carries `is_error: true` -- the MCP way
    /// of saying "the tool ran and failed", as opposed to a protocol error.
    #[tool(description = "Always fail, reporting the failure in the tool result")]
    async fn explode(&self) -> Result<String, String> {
        Err("explode: boom".to_string())
    }

    #[tool(description = "Never return; the caller must time itself out")]
    async fn hang(&self) -> String {
        std::future::pending::<String>().await
    }

    #[tool(description = "Poll for changes; this fixture never has any")]
    async fn watch_poll(&self) -> String {
        "[]".to_string()
    }
}

#[tool_handler]
impl ServerHandler for EchoServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Fixture MCP server used by exec-agent's connector registry tests")
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let service = EchoServer::new().serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
