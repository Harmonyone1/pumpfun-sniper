//! Pump.fun Sniper Bot - High-performance token sniper using Jito ShredStream
//!
//! # WARNING
//! - This bot trades with real money. Only use funds you can afford to lose.
//! - Most pump.fun tokens go to zero (rug pulls, abandonment).
//! - MEV competition means other bots may outbid you.
//! - Testnet success does NOT equal mainnet success.

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::{error, info, warn};

// Use the library crate
use pumpfun_sniper::cli::commands;
use pumpfun_sniper::config::Config;

/// Pump.fun Sniper Bot - High-performance token sniper
#[derive(Parser)]
#[command(name = "snipe")]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Path to config file
    #[arg(short, long, default_value = "config.toml")]
    config: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the sniper bot
    Start {
        /// Run in dry-run mode (no real trades)
        #[arg(long)]
        dry_run: bool,
    },

    /// Manually sell a token position
    Sell {
        /// Token mint address
        token: String,

        /// Amount to sell (default: all). Use percentage like "50%" or absolute amount
        #[arg(default_value = "100%")]
        amount: String,

        /// Skip confirmation prompt
        #[arg(long)]
        force: bool,

        /// Simulate only, don't execute
        #[arg(long)]
        dry_run: bool,
    },

    /// Show current positions and P&L
    Status,

    /// Show current configuration (secrets masked)
    Config,

    /// Check system health (RPC, ShredStream, Jito)
    Health,

    /// Wallet management commands
    Wallet {
        #[command(subcommand)]
        action: WalletAction,
    },
}

#[derive(Subcommand)]
enum WalletAction {
    /// Show wallet status (all wallets, balances)
    Status,

    /// List all configured wallets
    List,

    /// Add a new wallet
    Add {
        /// Wallet name (lowercase, no spaces)
        name: String,

        /// Human-readable alias
        #[arg(long)]
        alias: String,

        /// Wallet type: hot, vault, external, auth
        #[arg(long, value_name = "TYPE")]
        wallet_type: String,

        /// Address (required for external wallets)
        #[arg(long)]
        address: Option<String>,

        /// Generate new keypair (for hot/vault types)
        #[arg(long)]
        generate: bool,
    },

    /// Extract SOL to vault
    Extract {
        /// Amount in SOL
        amount: f64,

        /// Skip confirmation prompt
        #[arg(long)]
        force: bool,

        /// Simulate only, don't execute
        #[arg(long)]
        dry_run: bool,
    },

    /// View transfer history
    History {
        /// Number of records to show
        #[arg(short, long, default_value = "20")]
        limit: usize,
    },

    /// View/manage AI proposals
    Proposals {
        /// Approve proposal by ID
        #[arg(long)]
        approve: Option<String>,

        /// Reject proposal by ID
        #[arg(long)]
        reject: Option<String>,
    },

    /// Emergency actions
    Emergency {
        /// Shutdown all trading
        #[arg(long)]
        shutdown: bool,

        /// Resume operations
        #[arg(long)]
        resume: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load environment variables from .env file
    dotenvy::dotenv().ok();

    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("pumpfun_sniper=info".parse().unwrap()),
        )
        .with_target(true)
        .with_thread_ids(true)
        .with_file(true)
        .with_line_number(true)
        .init();

    // Parse CLI arguments
    let cli = Cli::parse();

    // Load configuration
    let config = match Config::load(&cli.config) {
        Ok(cfg) => cfg,
        Err(e) => {
            error!("Failed to load configuration: {}", e);
            std::process::exit(1);
        }
    };

    // Perform startup checks
    if let Err(e) = startup_checks(&config).await {
        error!("Startup checks failed: {}", e);
        std::process::exit(1);
    }

    // Execute command
    let result = match cli.command {
        Commands::Start { dry_run } => commands::start(&config, dry_run).await,
        Commands::Sell {
            token,
            amount,
            force,
            dry_run,
        } => commands::sell(&config, &token, &amount, force, dry_run).await,
        Commands::Status => commands::status(&config).await,
        Commands::Config => commands::show_config(&config),
        Commands::Health => commands::health(&config).await,
        Commands::Wallet { action } => match action {
            WalletAction::Status => commands::wallet_status(&config).await,
            WalletAction::List => commands::wallet_list(&config).await,
            WalletAction::Add {
                name,
                alias,
                wallet_type,
                address,
                generate,
            } => commands::wallet_add(&config, &name, &alias, &wallet_type, address, generate).await,
            WalletAction::Extract { amount, force, dry_run } => {
                commands::wallet_extract(&config, amount, force, dry_run).await
            }
            WalletAction::History { limit } => commands::wallet_history(&config, limit).await,
            WalletAction::Proposals { approve, reject } => {
                commands::wallet_proposals(&config, approve, reject).await
            }
            WalletAction::Emergency { shutdown, resume } => {
                commands::wallet_emergency(&config, shutdown, resume).await
            }
        },
    };

    if let Err(e) = result {
        error!("Command failed: {}", e);
        std::process::exit(1);
    }

    Ok(())
}

/// Perform startup safety checks
async fn startup_checks(config: &Config) -> Result<()> {
    info!("Performing startup checks...");

    // Check keypair permissions (Unix only)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let keypair_path = std::env::var("KEYPAIR_PATH")
            .map_err(|_| anyhow::anyhow!("KEYPAIR_PATH environment variable not set"))?;

        let metadata = std::fs::metadata(&keypair_path)
            .map_err(|e| anyhow::anyhow!("Cannot read keypair file {}: {}", keypair_path, e))?;

        let permissions = metadata.permissions();
        let mode = permissions.mode();

        // Check if file is readable by group or others (not 600)
        if mode & 0o077 != 0 {
            return Err(anyhow::anyhow!(
                "Keypair file {} has insecure permissions {:o}. \
                 Run 'chmod 600 {}' to fix. \
                 This bot refuses to run with world-readable keypairs.",
                keypair_path,
                mode & 0o777,
                keypair_path
            ));
        }

        info!("Keypair permissions OK");
    }

    // Check keypair balance warning
    let keypair_path = std::env::var("KEYPAIR_PATH").ok();
    if let Some(path) = keypair_path {
        if std::path::Path::new(&path).exists() {
            // We'll check balance when we have RPC client initialized
            info!("Keypair file found: {}", path);
        } else {
            return Err(anyhow::anyhow!("Keypair file not found: {}", path));
        }
    } else {
        return Err(anyhow::anyhow!("KEYPAIR_PATH environment variable not set"));
    }

    // Warn about safety limits
    warn!(
        "Safety limits active: max_position={}SOL, daily_loss_limit={}SOL",
        config.safety.max_position_sol, config.safety.daily_loss_limit_sol
    );

    info!("Startup checks passed");
    Ok(())
}
