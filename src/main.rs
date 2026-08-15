#![allow(dead_code)]

mod admin;
mod chains;
mod db;
mod http;
mod mcp;
mod oauth;

use std::collections::HashMap;

use anyhow::{Context, Result};
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
    Http(HttpArgs),
    /// Run the HTTP server in the background, detached from this terminal.
    /// Logs are written to the file given by --log; the command returns immediately.
    Daemon {
        #[command(flatten)]
        http: HttpArgs,
        /// Path to the log file (stdout + stderr go here)
        #[arg(long)]
        log: String,
    },
    /// Manage API keys (admin CLI)
    Admin {
        #[command(subcommand)]
        command: AdminCommands,
    },
}

/// Arguments for the HTTP server (shared by `http` and `daemon`)
#[derive(clap::Args, Clone)]
struct HttpArgs {
    /// Listen address (e.g., "0.0.0.0:3000")
    #[arg(long, default_value = "0.0.0.0:3000")]
    addr: String,

    /// Database file path (default: data/apy-mcp.db)
    #[arg(long, default_value = "data/apy-mcp.db")]
    db_path: String,

    /// Admin API token for management endpoints (optional, or set ADMIN_TOKEN env var)
    #[arg(long)]
    admin_token: Option<String>,

    /// Base URL for OAuth redirects (e.g., "https://mcp.example.com", or set BASE_URL env var)
    #[arg(long, env = "BASE_URL")]
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

    /// Comma-separated list of allowed GitHub usernames or numeric UIDs.
    /// When empty (default) all GitHub users can log in. (or set ALLOWED_GITHUB_USERS env var)
    #[arg(long, env = "ALLOWED_GITHUB_USERS")]
    allowed_github_users: Option<String>,

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

    /// Global EVM RPC provider name (alchemy [default], infura, drpc, public)
    /// Applies to all chains unless overridden by --evm-rpc-* or --evm-config
    #[arg(long, env = "EVM_PROVIDER")]
    evm_provider: Option<String>,

    /// API key for the global EVM provider.
    /// Default provider is alchemy, so ALCHEMY_KEY is used when this is unset.
    #[arg(long, env = "EVM_PROVIDER_KEY")]
    evm_provider_key: Option<String>,

    /// Path to JSON config file for per-chain provider assignments
    #[arg(long, env = "EVM_CONFIG")]
    evm_config: Option<String>,
}

/// Append the HTTP args back onto a command line (used by `daemon` to respawn
/// the server with the same configuration)
fn append_http_args(cmd: &mut std::process::Command, args: &HttpArgs) {
    cmd.arg("http");
    cmd.args(["--addr", &args.addr]);
    cmd.args(["--db-path", &args.db_path]);
    cmd.args(["--pool-id", &args.pool_id]);
    if let Some(v) = &args.admin_token {
        cmd.args(["--admin-token", v]);
    }
    if let Some(v) = &args.base_url {
        cmd.args(["--base-url", v]);
    }
    if let Some(v) = &args.github_client_id {
        cmd.args(["--github-client-id", v]);
    }
    if let Some(v) = &args.github_client_secret {
        cmd.args(["--github-client-secret", v]);
    }
    if let Some(v) = &args.allowed_github_users {
        cmd.args(["--allowed-github-users", v]);
    }
    if let Some(v) = &args.custom_oauth_provider {
        cmd.args(["--custom-oauth-provider", v]);
    }
    if let Some(v) = &args.custom_oauth_client_id {
        cmd.args(["--custom-oauth-client-id", v]);
    }
    if let Some(v) = &args.custom_oauth_client_secret {
        cmd.args(["--custom-oauth-client-secret", v]);
    }
    if let Some(v) = &args.custom_oauth_auth_url {
        cmd.args(["--custom-oauth-auth-url", v]);
    }
    if let Some(v) = &args.custom_oauth_token_url {
        cmd.args(["--custom-oauth-token-url", v]);
    }
    if let Some(v) = &args.custom_oauth_user_info_url {
        cmd.args(["--custom-oauth-user-info-url", v]);
    }
    if let Some(v) = &args.custom_oauth_scopes {
        cmd.args(["--custom-oauth-scopes", v]);
    }
    if let Some(v) = &args.evm_rpc_ethereum {
        cmd.args(["--evm-rpc-ethereum", v]);
    }
    if let Some(v) = &args.evm_rpc_polygon {
        cmd.args(["--evm-rpc-polygon", v]);
    }
    if let Some(v) = &args.evm_rpc_arbitrum {
        cmd.args(["--evm-rpc-arbitrum", v]);
    }
    if let Some(v) = &args.evm_rpc_optimism {
        cmd.args(["--evm-rpc-optimism", v]);
    }
    if let Some(v) = &args.evm_rpc_avalanche {
        cmd.args(["--evm-rpc-avalanche", v]);
    }
    if let Some(v) = &args.evm_rpc_base {
        cmd.args(["--evm-rpc-base", v]);
    }
    if let Some(v) = &args.evm_rpc_gnosis {
        cmd.args(["--evm-rpc-gnosis", v]);
    }
    if let Some(v) = &args.evm_rpc_bnb {
        cmd.args(["--evm-rpc-bnb", v]);
    }
    if let Some(v) = &args.evm_rpc_scroll {
        cmd.args(["--evm-rpc-scroll", v]);
    }
    if let Some(v) = &args.evm_rpc_zksync {
        cmd.args(["--evm-rpc-zksync", v]);
    }
    if let Some(v) = &args.evm_rpc_sonic {
        cmd.args(["--evm-rpc-sonic", v]);
    }
    if let Some(v) = &args.evm_provider {
        cmd.args(["--evm-provider", v]);
    }
    if let Some(v) = &args.evm_provider_key {
        cmd.args(["--evm-provider-key", v]);
    }
    if let Some(v) = &args.evm_config {
        cmd.args(["--evm-config", v]);
    }
}

/// Spawn the HTTP server in the background, detached from this terminal.
/// Logs (stdout + stderr) are appended to the given log file.
fn run_daemon(args: HttpArgs, log: &str) -> Result<()> {
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe().context("Failed to locate current executable")?;
    let mut cmd = Command::new(exe);
    append_http_args(&mut cmd, &args);

    // Route stdout + stderr to the log file (append), stdin to /dev/null
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .context(format!("Failed to open log file '{}'", log))?;
    let file2 = file.try_clone().context("Failed to clone log file handle")?;
    cmd.stdout(Stdio::from(file));
    cmd.stderr(Stdio::from(file2));
    cmd.stdin(Stdio::null());

    // Detach from the terminal's process group so it survives shell exit / Ctrl+C
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let child = cmd.spawn().context("Failed to spawn background process")?;
    println!("Started apy-mcp daemon in the background");
    println!("  PID:     {}", child.id());
    println!("  Log:     {}", log);
    println!("  Address: {}", args.addr);
    println!("\nTail the log with: tail -f {}", log);
    Ok(())
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

/// Seed the GitHub allowlist from a comma-separated list of usernames or numeric UIDs.
/// Numeric values are treated as UIDs, everything else as usernames.
async fn seed_github_allowlist(db: &db::Database, config: Option<&str>) -> Result<(), sqlx::Error> {
    let Some(list) = config else {
        return Ok(());
    };

    for value in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let kind = if value.parse::<u64>().is_ok() {
            "uid"
        } else {
            "username"
        };
        db.add_allowlist(value, kind, Some("ALLOWED_GITHUB_USERS"))
            .await?;
    }
    Ok(())
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
        Commands::Http(args) => run_http(args).await?,
        Commands::Daemon { http, log } => run_daemon(http, &log)?,
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

/// Run the HTTP server (used by both the `http` and `daemon` subcommands)
async fn run_http(args: HttpArgs) -> Result<()> {
    tracing::info!("Starting apy-mcp server in HTTP mode");

    let HttpArgs {
        addr,
        db_path,
        admin_token,
        base_url,
        pool_id,
        github_client_id,
        github_client_secret,
        allowed_github_users,
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
    } = args;

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
        github_client_id.as_deref(),
        github_client_secret.as_deref(),
    )
    .await?;

    // Seed GitHub allowlist from CLI/env config (can be managed later via admin API)
    seed_github_allowlist(&db, allowed_github_users.as_deref()).await?;
    let allowlist_len = db.list_allowlist().await?.len();
    if allowlist_len == 0 {
        tracing::warn!(
            "GitHub allowlist is empty - ALL GitHub users can log in. Set ALLOWED_GITHUB_USERS to restrict access."
        );
    } else {
        tracing::info!(entries = allowlist_len, "GitHub allowlist loaded");
    }

    // Set up EVM RPC manager with provider support
    let rpc = crate::chains::evm::rpc::RpcManager::new();

    // 1. Set global default provider (defaults to alchemy, key from ALCHEMY_KEY / EVM_PROVIDER_KEY).
    //    Without a key the provider is skipped and public default URLs are kept as fallback.
    let default_provider = evm_provider.unwrap_or_else(|| "alchemy".to_string());
    let key = evm_provider_key.or_else(|| {
        let env_key = format!("{}_KEY", default_provider.to_uppercase());
        std::env::var(&env_key).ok()
    });
    rpc.set_default_provider(&default_provider, key).await;

    // 2. Load JSON config file for per-chain provider assignments
    if let Some(ref config_path) = evm_config {
        match load_evm_config(config_path) {
            Ok(config) => {
                for (chain, assignment) in &config.chains {
                    rpc.set_chain_provider(
                        chain,
                        &assignment.provider,
                        assignment.api_key.clone(),
                    )
                    .await;
                }
                tracing::info!(chains = config.chains.len(), "Loaded EVM config from file");
            }
            Err(e) => {
                tracing::warn!(error = %e, path = config_path, "Failed to load EVM config file");
            }
        }
    }

    // 3. Apply provider templates to resolve final URLs (defaults + config-file assignments)
    rpc.apply_providers().await;

    // 4. Per-chain direct overrides (highest priority, via --evm-rpc-* or env vars).
    //    Applied after providers so an explicit per-chain URL always wins.
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

    let tools = ApyMcpTools::with_rpc_manager_and_db(&pool_id, rpc, db.clone());
    let addr: std::net::SocketAddr = addr.parse()?;

    // Fall back to environment variable if not provided via CLI
    let admin_token = admin_token.or_else(|| std::env::var("ADMIN_TOKEN").ok());

    // Start HTTP server (OAuth providers are now managed via database)
    http::start_http_server(addr, tools, db, admin_token, base_url).await?;
    Ok(())
}
