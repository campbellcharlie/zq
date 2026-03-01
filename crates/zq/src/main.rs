mod app;
mod client;
mod event;
mod ui;

use anyhow::Result;
use tracing::info;
use zq_proto::config::Config;

#[tokio::main]
async fn main() -> Result<()> {
    // Log to file so it doesn't interfere with the TUI.
    let log_file = std::fs::File::create("/tmp/zq.log")?;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "zq=debug".into()),
        )
        .with_writer(log_file)
        .init();

    let config = Config::load();
    info!("zq starting");

    // Pre-flight: check if daemon is reachable.
    let daemon_reachable =
        std::os::unix::net::UnixStream::connect(&config.socket_path).is_ok();

    if !daemon_reachable {
        let is_root = unsafe { libc::geteuid() } == 0;

        if !is_root {
            eprintln!("zq: daemon is not running.");
            eprintln!();
            eprintln!("The daemon requires root privileges to intercept network traffic");
            eprintln!("(it needs access to /dev/pf and pfctl for packet redirection).");
            eprintln!();
            eprintln!("Run with: sudo zq");
            std::process::exit(1);
        }

        // We are root and daemon is not running — spawn it.
        info!("daemon not running, spawning zq-daemon");
        let daemon_exe = std::env::current_exe()?
            .parent()
            .expect("exe has parent dir")
            .join("zq-daemon");

        if !daemon_exe.exists() {
            eprintln!(
                "zq: cannot find daemon binary at {}",
                daemon_exe.display()
            );
            eprintln!("Build the daemon first: cargo build -p zq-daemon");
            std::process::exit(1);
        }

        std::process::Command::new(&daemon_exe)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to spawn zq-daemon: {e}"))?;

        // Poll for the socket to appear (up to 2 seconds).
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_secs(2) {
            if std::os::unix::net::UnixStream::connect(&config.socket_path).is_ok() {
                info!("daemon is ready");
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    let result = app::run(config).await;

    info!("zq stopped");
    result
}
