//! Setup and teardown of pf redirect rules.
//!
//! `run_setup()` loads the `zq` pf anchor with redirect rules.
//! `run_teardown()` flushes the anchor.

use anyhow::Result;

use crate::pf;
use zq_proto::config::Config;

/// Load pf redirect rules for the transparent proxy.
pub fn run_setup() -> Result<()> {
    let config = Config::load();
    let uid = unsafe { libc::getuid() };
    println!(
        "Loading pf rules (proxy port {}, interface {}, excluded uid {uid})...",
        config.proxy_port, config.interface
    );
    pf::load_rules(config.proxy_port, uid, &config.interface)?;
    println!("Done. PF anchor 'zq' is active.");
    println!();
    println!("Verify with: sudo pfctl -a zq -sr");
    Ok(())
}

/// Remove pf redirect rules.
pub fn run_teardown() -> Result<()> {
    println!("Removing pf anchor 'zq'...");
    pf::unload_rules()?;
    println!("Done. PF anchor 'zq' has been flushed.");
    Ok(())
}
