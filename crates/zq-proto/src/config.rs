//! Configuration file support for zq.
//!
//! Reads `~/.zq/config` (simple `key = value` format, `#` comments).
//! Creates the file with defaults on first run.

use std::path::PathBuf;

/// Runtime configuration for all zq components.
#[derive(Debug, Clone)]
pub struct Config {
    /// Address of the upstream HTTP proxy (e.g. Burp Suite, mitmproxy).
    pub proxy_addr: String,
    /// Network interface for pf route-to rules.
    pub interface: String,
    /// Local port the transparent proxy listens on.
    pub proxy_port: u16,
    /// Unix socket path for daemon ↔ TUI communication.
    pub socket_path: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            proxy_addr: "127.0.0.1:8080".to_string(),
            interface: "en0".to_string(),
            proxy_port: 8888,
            socket_path: "/tmp/zq-tui.sock".to_string(),
        }
    }
}

impl Config {
    /// Load configuration from `~/.zq/config`.
    ///
    /// If the file does not exist, creates it with default values.
    /// If `$HOME` is not set, returns defaults without creating a file.
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };

        if !path.exists() {
            // Create config directory and default file.
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&path, DEFAULT_CONFIG);
            return Self::default();
        }

        match std::fs::read_to_string(&path) {
            Ok(contents) => Self::parse(&contents),
            Err(_) => Self::default(),
        }
    }

    /// Parse a config string (key = value lines, # comments).
    pub fn parse(input: &str) -> Self {
        let mut config = Self::default();

        for line in input.lines() {
            let line = line.trim();

            // Skip empty lines and comments.
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let Some((key, value)) = line.split_once('=') else {
                continue;
            };

            let key = key.trim();
            let value = value.trim();

            match key {
                "proxy_addr" => config.proxy_addr = value.to_string(),
                "interface" => config.interface = value.to_string(),
                "proxy_port" => {
                    if let Ok(port) = value.parse::<u16>() {
                        config.proxy_port = port;
                    }
                }
                "socket_path" => config.socket_path = value.to_string(),
                _ => {} // Unknown keys are silently ignored.
            }
        }

        config
    }
}

/// Returns the path to `~/.zq/config`, or `None` if `$HOME` is unset.
fn config_path() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".zq").join("config"))
}

const DEFAULT_CONFIG: &str = "\
# zq configuration
proxy_addr = 127.0.0.1:8080
interface = en0
proxy_port = 8888
# socket_path = /tmp/zq-tui.sock
";

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_defaults() {
        let config = Config::default();
        assert_eq!(config.proxy_addr, "127.0.0.1:8080");
        assert_eq!(config.interface, "en0");
        assert_eq!(config.proxy_port, 8888);
        assert_eq!(config.socket_path, "/tmp/zq-tui.sock");
    }

    #[test]
    fn test_parse_basic() {
        let input = "proxy_addr = 10.0.0.1:9090\ninterface = en1\nproxy_port = 9999\n";
        let config = Config::parse(input);
        assert_eq!(config.proxy_addr, "10.0.0.1:9090");
        assert_eq!(config.interface, "en1");
        assert_eq!(config.proxy_port, 9999);
        // socket_path should remain default
        assert_eq!(config.socket_path, "/tmp/zq-tui.sock");
    }

    #[test]
    fn test_parse_comments_and_blanks() {
        let input = "\
# This is a comment
proxy_addr = 1.2.3.4:5555

# Another comment
interface = en2
";
        let config = Config::parse(input);
        assert_eq!(config.proxy_addr, "1.2.3.4:5555");
        assert_eq!(config.interface, "en2");
        assert_eq!(config.proxy_port, 8888); // default
    }

    #[test]
    fn test_parse_invalid_port_uses_default() {
        let input = "proxy_port = notanumber\n";
        let config = Config::parse(input);
        assert_eq!(config.proxy_port, 8888);
    }

    #[test]
    fn test_parse_unknown_keys_ignored() {
        let input = "unknown_key = some_value\nproxy_addr = 5.5.5.5:1234\n";
        let config = Config::parse(input);
        assert_eq!(config.proxy_addr, "5.5.5.5:1234");
    }

    #[test]
    fn test_parse_socket_path() {
        let input = "socket_path = /var/run/zq.sock\n";
        let config = Config::parse(input);
        assert_eq!(config.socket_path, "/var/run/zq.sock");
    }

    #[test]
    fn test_parse_empty_input() {
        let config = Config::parse("");
        assert_eq!(config.proxy_addr, "127.0.0.1:8080");
        assert_eq!(config.interface, "en0");
        assert_eq!(config.proxy_port, 8888);
    }
}
