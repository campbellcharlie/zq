mod forwarder;
mod hub;
mod install;
mod pf;
mod procinfo;
mod proxy;
mod status;
mod tui_server;

use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tokio::signal;
use tracing::{info, warn};
use zq_proto::config::Config;

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "zq-daemon",
    about = "ZQ network monitor daemon",
    version,
    arg_required_else_help = false
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Load pf redirect rules into anchor 'zq'
    Setup,
    /// Remove pf redirect rules from anchor 'zq'
    Teardown,
    /// Show daemon status, pf anchor state, and proxy reachability
    Status,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Setup) => return install::run_setup(),
        Some(Commands::Teardown) => return install::run_teardown(),
        Some(Commands::Status) => return status::run(),
        None => {} // Fall through to run the daemon.
    }

    // --- Daemon mode (no subcommand) ---

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "zq_daemon=info".into()),
        )
        .init();

    let config = Config::load();
    info!(
        proxy_addr = %config.proxy_addr,
        proxy_port = config.proxy_port,
        socket_path = %config.socket_path,
        "zq-daemon starting"
    );

    // Open /dev/pf for DIOCNATLOOK.
    let pf_device = Arc::new(
        pf::PfDevice::open().expect("failed to open /dev/pf — is the daemon running as root?"),
    );

    // Auto-load pf rules on daemon startup.
    let uid = unsafe { libc::getuid() };
    if let Err(e) = pf::load_rules(config.proxy_port, uid) {
        warn!("failed to load pf rules: {e}");
        warn!("traffic interception may not work — run `zq-daemon setup` manually");
    }

    let hub = hub::Hub::new();

    // Start the transparent proxy.
    let proxy_handle = {
        let hub = hub.clone();
        let pf = pf_device.clone();
        let proxy_port = config.proxy_port;
        let proxy_addr = config.proxy_addr.clone();
        tokio::spawn(async move {
            if let Err(e) = proxy::run(hub, pf, proxy_port, proxy_addr).await {
                warn!("proxy exited with error: {e}");
            }
        })
    };

    // Start the TUI-facing server.
    let tui_handle = {
        let hub = hub.clone();
        let socket_path = config.socket_path.clone();
        tokio::spawn(async move {
            if let Err(e) = tui_server::run(hub, &socket_path).await {
                warn!("tui_server exited with error: {e}");
            }
        })
    };

    // Start proxy health check loop.
    let health_handle = {
        let hub = hub.clone();
        let proxy_addr = config.proxy_addr.clone();
        tokio::spawn(async move {
            forwarder::health_check_loop(hub, proxy_addr).await;
        })
    };

    // Wait for shutdown signal (Ctrl-C or TUI shutdown command).
    tokio::select! {
        _ = signal::ctrl_c() => {
            info!("shutdown signal received (ctrl-c)");
        }
        _ = hub.wait_shutdown() => {
            info!("shutdown signal received (TUI command)");
        }
    }

    info!("cleaning up");

    // Unload pf rules.
    if let Err(e) = pf::unload_rules() {
        warn!("failed to unload pf rules on shutdown: {e}");
    }

    proxy_handle.abort();
    tui_handle.abort();
    health_handle.abort();

    // Clean up socket files.
    let _ = std::fs::remove_file(&config.socket_path);

    info!("zq-daemon stopped");
    Ok(())
}
