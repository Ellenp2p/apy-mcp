#![allow(dead_code)]

mod admin;
mod chains;
mod db;
mod http;
mod mcp;
mod oauth;

use std::collections::HashMap;

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

        /// Base URL for OAuth redirects (e.g., "http://localhost:3000" or "https://mcp.example.com")
        #[arg(long)]
        base_url: Option<String>,

        /// Default Blend pool ID
        #[arg(long, default_value = DEFAULT_BLEND_POOL)]
        pool_id: String,

        /// GitHub OAuth Client ID (optional, or set GITHUB_CLIENT_ID env var)
        #[arg(long)]
        github_client_id: Option<String>,

        /// GitHub OAuth Client Secret (optional, or set GITHUB_CLIENT_SECRET env var)
        #[arg(long)]
        github_client_secret: Option<String>,

        /// Google OAuth Client ID (optional, or set GOOGLE_CLIENT_ID env var)
        #[arg(long)]
        google_client_id: Option<String>,

        /// Google OAuth Client Secret (optional, or set GOOGLE_CLIENT_SECRET env var)
        #[arg(long)]
        google_client_secret: Option<String>,

        /// Custom OAuth Provider Name (optional, or set CUSTOM_OAUTH_PROVIDER env var)
        #[arg(long)]
        custom_oauth_provider: Option<String>,

        /// Custom OAuth Client ID (optional, or set CUSTOM_OAUTH_CLIENT_ID env var)
        #[arg(long)]
        custom_oauth_client_id: Option<String>,

        /// Custom OAuth Client Secret (optional, or set CUSTOM_OAUTH_CLIENT_SECRET env var)
        #[arg(long)]
        custom_oauth_client_secret: Option<String>,

        /// Custom OAuth Auth URL (optional, or set CUSTOM_OAUTH_AUTH_URL env var)
        #[arg(long)]
        custom_oauth_auth_url: Option<String>,

        /// Custom OAuth Token URL (optional, or set CUSTOM_OAUTH_TOKEN_URL env var)
        #[arg(long)]
        custom_oauth_token_url: Option<String>,

        /// Custom OAuth User Info URL (optional, or set CUSTOM_OAUTH_USER_INFO_URL env var)
        #[arg(long)]
        custom_oauth_user_info_url: Option<String>,

        /// Custom OAuth Scopes (optional, or set CUSTOM_OAUTH_SCOPES env var, comma-separated)
        #[arg(long)]
        custom_oauth_scopes: Option<String>,

        /// EVM RPC URL for Ethereum (optional, or set EVM_RPC_ETHEREUM env var)
        #[arg(long)]
        evm_rpc_ethereum: Option<String>,

        /// EVM RPC URL for Polygon (optional, or set EVM_RPC_POLYGON env var)
        #[arg(long)]
        evm_rpc_polygon: Option<String>,

        /// EVM RPC URL for Arbitrum (optional, or set EVM_RPC_ARBITRUM env var)
        #[arg(long)]
        evm_rpc_arbitrum: Option<String>,

        /// EVM RPC URL for Optimism (optional, or set EVM_RPC_OPTIMISM env var)
        #[arg(long)]
        evm_rpc_optimism: Option<String>,

        /// EVM RPC URL for Avalanche (optional, or set EVM_RPC_AVALANCHE env var)
        #[arg(long)]
        evm_rpc_avalanche: Option<String>,

        /// EVM RPC URL for Base (optional, or set EVM_RPC_BASE env var)
        #[arg(long)]
        evm_rpc_base: Option<String>,

        /// EVM RPC URL for Gnosis (optional, or set EVM_RPC_GNOSIS env var)
        #[arg(long)]
        evm_rpc_gnosis: Option<String>,

        /// EVM RPC URL for BNB Chain (optional, or set EVM_RPC_BNB env var)
        #[arg(long)]
        evm_rpc_bnb: Option<String>,

        /// EVM RPC URL for Scroll (optional, or set EVM_RPC_SCROLL env var)
        #[arg(long)]
        evm_rpc_scroll: Option<String>,

        /// EVM RPC URL for zkSync (optional, or set EVM_RPC_ZKSYNC env var)
        #[arg(long)]
        evm_rpc_zksync: Option<String>,

        /// EVM RPC URL for Sonic (optional, or set EVM_RPC_SONIC env var)
        #[arg(long)]
        evm_rpc_sonic: Option<String>,

        /// Global EVM RPC provider name (alchemy, infura, drpc, public)
        /// Applies to all chains unless overridden by --evm-rpc-* or --evm-config
        #[arg(long, env = "EVM_PROVIDER")]
        evm_provider: Option<String>,

        /// API key for the global EVM provider
        #[arg(long, env = "EVM_PROVIDER_KEY")]
        evm_provider_key: Option<String>,

        /// Path to JSON config file for per-chain provider assignments
        #[arg(long, env = "EVM_CONFIG")]
        evm_config: Option<String>,
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

/// Build OAuth config from CLI args and environment variables
fn build_oauth_config(
    addr: &std::net::SocketAddr,
    github_client_id: Option<String>,
    github_client_secret: Option<String>,
    google_client_id: Option<String>,
    google_client_secret: Option<String>,
    custom_oauth_provider: Option<String>,
    custom_oauth_client_id: Option<String>,
    custom_oauth_client_secret: Option<String>,
    custom_oauth_auth_url: Option<String>,
    custom_oauth_token_url: Option<String>,
    custom_oauth_user_info_url: Option<String>,
    custom_oauth_scopes: Option<String>,
) -> Option<oauth::OAuthConfig> {
    let mut providers = HashMap::new();
    let base_url = format!("http://localhost:{}", addr.port());

    // GitHub OAuth
    let github_id = github_client_id.or_else(|| std::env::var("GITHUB_CLIENT_ID").ok());
    let github_secret = github_client_secret.or_else(|| std::env::var("GITHUB_CLIENT_SECRET").ok());

    if let (Some(id), Some(secret)) = (github_id, github_secret) {
        tracing::info!(client_id = %id, "GitHub OAuth configured");
        providers.insert(
            "github".to_string(),
            oauth::OAuthProviderConfig {
                name: "github".to_string(),
                client_id: id,
                client_secret: secret,
                auth_url: "https://github.com/login/oauth/authorize".to_string(),
                token_url: "https://github.com/login/oauth/access_token".to_string(),
                user_info_url: "https://api.github.com/user".to_string(),
                scopes: vec!["read:user".to_string(), "user:email".to_string()],
            },
        );
    }

    // Google OAuth
    let google_id = google_client_id.or_else(|| std::env::var("GOOGLE_CLIENT_ID").ok());
    let google_secret = google_client_secret.or_else(|| std::env::var("GOOGLE_CLIENT_SECRET").ok());

    if let (Some(id), Some(secret)) = (google_id, google_secret) {
        tracing::info!(client_id = %id, "Google OAuth configured");
        providers.insert(
            "google".to_string(),
            oauth::OAuthProviderConfig {
                name: "google".to_string(),
                client_id: id,
                client_secret: secret,
                auth_url: "https://accounts.google.com/o/oauth2/v2/auth".to_string(),
                token_url: "https://oauth2.googleapis.com/token".to_string(),
                user_info_url: "https://www.googleapis.com/oauth2/v3/userinfo".to_string(),
                scopes: vec![
                    "openid".to_string(),
                    "email".to_string(),
                    "profile".to_string(),
                ],
            },
        );
    }

    // Custom OAuth Provider
    let custom_id = custom_oauth_client_id.or_else(|| std::env::var("CUSTOM_OAUTH_CLIENT_ID").ok());
    let custom_secret =
        custom_oauth_client_secret.or_else(|| std::env::var("CUSTOM_OAUTH_CLIENT_SECRET").ok());
    let custom_name = custom_oauth_provider.or_else(|| std::env::var("CUSTOM_OAUTH_PROVIDER").ok());
    let custom_auth = custom_oauth_auth_url.or_else(|| std::env::var("CUSTOM_OAUTH_AUTH_URL").ok());
    let custom_token =
        custom_oauth_token_url.or_else(|| std::env::var("CUSTOM_OAUTH_TOKEN_URL").ok());
    let custom_user =
        custom_oauth_user_info_url.or_else(|| std::env::var("CUSTOM_OAUTH_USER_INFO_URL").ok());
    let custom_scopes = custom_oauth_scopes.or_else(|| std::env::var("CUSTOM_OAUTH_SCOPES").ok());

    if let (Some(name), Some(id), Some(secret), Some(auth), Some(token), Some(user)) = (
        custom_name,
        custom_id,
        custom_secret,
        custom_auth,
        custom_token,
        custom_user,
    ) {
        let scopes = custom_scopes
            .map(|s| s.split(',').map(|s| s.trim().to_string()).collect())
            .unwrap_or_default();

        tracing::info!(provider = %name, client_id = %id, "Custom OAuth configured");
        providers.insert(
            name.clone(),
            oauth::OAuthProviderConfig {
                name,
                client_id: id,
                client_secret: secret,
                auth_url: auth,
                token_url: token,
                user_info_url: user,
                scopes,
            },
        );
    }

    if providers.is_empty() {
        tracing::info!("No OAuth providers configured (set GITHUB_CLIENT_ID, GOOGLE_CLIENT_ID, or CUSTOM_OAUTH_* to enable)");
        None
    } else {
        Some(oauth::OAuthConfig {
            providers,
            base_url,
        })
    }
}

/// EVM provider configuration loaded from JSON
#[derive(serde::Deserialize)]
struct EvmConfig {
    #[serde(default)]
    chains: HashMap<String, crate::chains::evm::rpc::ChainProviderAssignment>,
}

/// Load EVM provider configuration from a JSON file
fn load_evm_config(path: &str) -> Result<EvmConfig> {
    let content = std::fs::read_to_string(path)?;
    let config: EvmConfig = serde_json::from_str(&content)?;
    Ok(config)
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
            base_url,
            pool_id,
            github_client_id,
            github_client_secret,
            google_client_id,
            google_client_secret,
            custom_oauth_provider: _,
            custom_oauth_client_id: _,
            custom_oauth_client_secret: _,
            custom_oauth_auth_url: _,
            custom_oauth_token_url: _,
            custom_oauth_user_info_url: _,
            custom_oauth_scopes: _,
            evm_rpc_ethereum,
            evm_rpc_polygon,
            evm_rpc_arbitrum,
            evm_rpc_optimism,
            evm_rpc_avalanche,
            evm_rpc_base,
            evm_rpc_gnosis,
            evm_rpc_bnb,
            evm_rpc_scroll,
            evm_rpc_zksync,
            evm_rpc_sonic,
            evm_provider,
            evm_provider_key,
            evm_config,
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

            // Initialize OAuth tables
            oauth::init_oauth_db(&db.pool).await?;
            tracing::info!("OAuth tables initialized");

            // Determine base URL for OAuth redirects
            let base_url = base_url.unwrap_or_else(|| {
                let port = addr.split(':').last().unwrap_or("3000");
                format!("http://localhost:{}", port)
            });
            tracing::info!("OAuth base URL: {}", base_url);

            // Initialize default OAuth providers (CLI args > env vars)
            oauth::init_default_providers(
                &db.pool,
                &base_url,
                github_client_id.as_deref(),
                github_client_secret.as_deref(),
                google_client_id.as_deref(),
                google_client_secret.as_deref(),
            )
            .await?;

            // Set up EVM RPC manager with provider support
            let rpc = crate::chains::evm::rpc::RpcManager::new();

            // 1. Set global default provider (if specified)
            if let Some(ref provider_name) = evm_provider {
                let key = evm_provider_key
                    .or_else(|| {
                        let env_key = format!("{}_KEY", provider_name.to_uppercase());
                        std::env::var(&env_key).ok()
                    });
                rpc.set_default_provider(provider_name, key).await;
            }

            // 2. Load JSON config file for per-chain provider assignments
            if let Some(ref config_path) = evm_config {
                match load_evm_config(config_path) {
                    Ok(config) => {
                        for (chain, assignment) in &config.chains {
                            rpc.set_chain_provider(chain, &assignment.provider, assignment.api_key.clone()).await;
                        }
                        tracing::info!(chains = config.chains.len(), "Loaded EVM config from file");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, path = config_path, "Failed to load EVM config file");
                    }
                }
            }

            // 3. Per-chain direct overrides (highest priority, via --evm-rpc-* or env vars)
            macro_rules! set_rpc_url {
                ($chain:expr, $cli_val:expr, $env_var:expr) => {
                    if let Some(url) = $cli_val {
                        rpc.set_rpc_url($chain, &url).await;
                    } else if let Ok(url) = std::env::var($env_var) {
                        rpc.set_rpc_url($chain, &url).await;
                    }
                };
            }

            set_rpc_url!("ethereum", evm_rpc_ethereum, "EVM_RPC_ETHEREUM");
            set_rpc_url!("polygon", evm_rpc_polygon, "EVM_RPC_POLYGON");
            set_rpc_url!("arbitrum", evm_rpc_arbitrum, "EVM_RPC_ARBITRUM");
            set_rpc_url!("optimism", evm_rpc_optimism, "EVM_RPC_OPTIMISM");
            set_rpc_url!("avalanche", evm_rpc_avalanche, "EVM_RPC_AVALANCHE");
            set_rpc_url!("base", evm_rpc_base, "EVM_RPC_BASE");
            set_rpc_url!("gnosis", evm_rpc_gnosis, "EVM_RPC_GNOSIS");
            set_rpc_url!("bnb", evm_rpc_bnb, "EVM_RPC_BNB");
            set_rpc_url!("scroll", evm_rpc_scroll, "EVM_RPC_SCROLL");
            set_rpc_url!("zksync", evm_rpc_zksync, "EVM_RPC_ZKSYNC");
            set_rpc_url!("sonic", evm_rpc_sonic, "EVM_RPC_SONIC");

            // 4. Apply provider templates to resolve final URLs
            rpc.apply_providers().await;

            let tools = ApyMcpTools::with_rpc_manager_and_db(&pool_id, rpc, db.clone());
            let addr: std::net::SocketAddr = addr.parse()?;

            // Fall back to environment variable if not provided via CLI
            let admin_token = admin_token.or_else(|| std::env::var("ADMIN_TOKEN").ok());

            // Start HTTP server (OAuth providers are now managed via database)
            http::start_http_server(addr, tools, db, admin_token, base_url).await?;
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
