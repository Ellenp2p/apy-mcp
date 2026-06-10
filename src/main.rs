#![allow(dead_code)]

mod chains;
mod mcp;

use anyhow::Result;
use rmcp::transport::stdio;
use rmcp::ServiceExt;
use tracing_subscriber::EnvFilter;

use mcp::tools::ApyMcpTools;

/// Default Blend Capital pool on Stellar mainnet
const DEFAULT_BLEND_POOL: &str = "CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD";

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging (writes to stderr so it doesn't interfere with MCP stdio)
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    tracing::info!("Starting apy-mcp server");

    // Create the MCP tools handler
    let tools = ApyMcpTools::new(DEFAULT_BLEND_POOL);

    // Start the MCP server over stdio
    let server = tools.serve(stdio()).await?;

    tracing::info!("MCP server running on stdio");

    // Wait for shutdown
    let reason = server.waiting().await?;
    tracing::info!(?reason, "MCP server shut down");

    Ok(())
}
