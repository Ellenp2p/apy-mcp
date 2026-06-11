#![allow(dead_code)]

mod admin;
mod chains;
mod db;
mod http;
mod mcp;

use anyhow::Result;
use clap::{Parser, Subcommand};
use rmcp::transport::stdio;
use rmcp::ServiceExt;
use tracing_subscriber::EnvFilter;

use mcp::tools::ApyMcpTools;

/// Default Blend Capital pool on Stellar mainnet
const DEFAULT_BLEND_POOL: &str = "CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD";

/// APY MCP Server - DeFi lending rate aggregation
#[derive(Parser)]
#[command(name = "apy-mcp")]
#[command(about = "MCP server for DeFi lending rate aggregation across multiple chains")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run as stdio MCP server (for local use with Claude Desktop, etc.)
    Stdio {
        /// Default Blend pool ID
        #[arg(long, default_value = DEFAULT_BLEND_POOL)]
        pool_id: String,
    },
    /// Run as HTTP server (for public deployment)
    Http {
        /// Listen address (e.g., "0.0.0.0:3000")
        #[arg(long, default_value = "0.0.0.0:3000")]
        addr: String,

        /// Database file path (default: data/apy-mcp.db)
        #[arg(long, default_value = "data/apy-mcp.db")]
        db_path: String,

        /// Admin API token for management endpoints (optional, or set ADMIN_TOKEN env var)
        #[arg(long)]
        admin_token: Option<String>,

        /// Default Blend pool ID
        #[arg(long, default_value = DEFAULT_BLEND_POOL)]
        pool_id: String,
    },
    /// Manage API keys (admin CLI)
    Admin {
        #[command(subcommand)]
        command: AdminCommands,
    },
}

#[derive(Subcommand)]
enum AdminCommands {
    /// Create a new API key
    CreateKey {
        /// Human-readable name for this key
        #[arg(long)]
        name: String,

        /// Optional user identifier
        #[arg(long)]
        user_id: Option<String>,

        /// Rate limit (requests per minute, default: 100)
        #[arg(long, default_value = "100")]
        rate_limit: i32,

        /// Database file path
        #[arg(long, default_value = "data/apy-mcp.db")]
        db_path: String,
    },
    /// List all API keys
    ListKeys {
        /// Database file path
        #[arg(long, default_value = "data/apy-mcp.db")]
        db_path: String,
    },
    /// Deactivate an API key
    Deactivate {
        /// API key ID
        #[arg(long)]
        key_id: String,

        /// Database file path
        #[arg(long, default_value = "data/apy-mcp.db")]
        db_path: String,
    },
    /// Delete an API key permanently
    Delete {
        /// API key ID
        #[arg(long)]
        key_id: String,

        /// Database file path
        #[arg(long, default_value = "data/apy-mcp.db")]
        db_path: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging (writes to stderr so it doesn't interfere with MCP stdio)
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Stdio { pool_id } => {
            tracing::info!("Starting apy-mcp server in stdio mode");
            let tools = ApyMcpTools::new(&pool_id);
            let server = tools.serve(stdio()).await?;
            tracing::info!("MCP server running on stdio");
            let reason = server.waiting().await?;
            tracing::info!(?reason, "MCP server shut down");
        }
        Commands::Http {
            addr,
            db_path,
            admin_token,
            pool_id,
        } => {
            tracing::info!("Starting apy-mcp server in HTTP mode");

            // Ensure database directory exists
            if let Some(parent) = std::path::Path::new(&db_path).parent() {
                std::fs::create_dir_all(parent)?;
            }

            // Initialize database
            let db_url = format!("sqlite:{}?mode=rwc", db_path);
            let db = db::Database::new(&db_url).await?;
            tracing::info!("Database initialized at {}", db_path);

            let tools = ApyMcpTools::new(&pool_id);
            let addr: std::net::SocketAddr = addr.parse()?;

            // Fall back to environment variable if not provided via CLI
            let admin_token = admin_token.or_else(|| std::env::var("ADMIN_TOKEN").ok());

            http::start_http_server(addr, tools, db, admin_token).await?;
        }
        Commands::Admin { command } => match command {
            AdminCommands::CreateKey {
                name,
                user_id,
                rate_limit,
                db_path,
            } => {
                let db_url = format!("sqlite:{}?mode=rwc", db_path);
                let db = db::Database::new(&db_url).await?;

                let (api_key, raw_key) = db
                    .create_key(&name, user_id.as_deref(), Some(rate_limit))
                    .await?;

                println!("✅ API Key created successfully!");
                println!();
                println!("  ID:        {}", api_key.id);
                println!("  Name:      {}", api_key.name);
                println!("  User ID:   {}", api_key.user_id.unwrap_or_default());
                println!("  Rate Limit: {} req/min", api_key.rate_limit);
                println!("  Created:   {}", api_key.created_at);
                println!();
                println!("🔑 API Key (save this - it won't be shown again):");
                println!("  {}", raw_key);
                println!();
                println!("Use in requests: Authorization: Bearer {}", raw_key);
            }
            AdminCommands::ListKeys { db_path } => {
                let db_url = format!("sqlite:{}?mode=rwc", db_path);
                let db = db::Database::new(&db_url).await?;

                let keys = db.list_keys().await?;

                if keys.is_empty() {
                    println!("No API keys found.");
                } else {
                    println!("📋 API Keys ({} total):", keys.len());
                    println!();
                    for key in &keys {
                        let status = if key.is_active { "✅" } else { "❌" };
                        println!(
                            "{} {} | ID: {} | User: {} | Limit: {} req/min | Calls: {}",
                            status,
                            key.name,
                            key.id,
                            key.user_id.as_deref().unwrap_or("-"),
                            key.rate_limit,
                            key.total_calls
                        );
                    }
                }
            }
            AdminCommands::Deactivate { key_id, db_path } => {
                let db_url = format!("sqlite:{}?mode=rwc", db_path);
                let db = db::Database::new(&db_url).await?;

                if db.deactivate_key(&key_id).await? {
                    println!("✅ API Key {} deactivated", key_id);
                } else {
                    println!("❌ API Key {} not found", key_id);
                }
            }
            AdminCommands::Delete { key_id, db_path } => {
                let db_url = format!("sqlite:{}?mode=rwc", db_path);
                let db = db::Database::new(&db_url).await?;

                if db.delete_key(&key_id).await? {
                    println!("✅ API Key {} deleted", key_id);
                } else {
                    println!("❌ API Key {} not found", key_id);
                }
            }
        },
    }

    Ok(())
}
