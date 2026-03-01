//! Process name and bundle ID resolution via system commands.
//!
//! These functions are synchronous and should be called from a blocking
//! context (e.g., `tokio::task::spawn_blocking`).

use std::process::Command;

/// Resolve a PID to its process name using `ps`.
///
/// Returns `"unknown"` if the process cannot be found or the command fails.
pub fn resolve_process_name(pid: u32) -> String {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if name.is_empty() {
                "unknown".to_string()
            } else {
                // ps returns the full path; take just the binary name.
                name.rsplit('/').next().unwrap_or("unknown").to_string()
            }
        }
        _ => "unknown".to_string(),
    }
}

/// Attempt to resolve a bundle-like identifier for the given PID.
///
/// Strategy: run `lsof -p <pid>` and scan for a `.app/` path component,
/// then extract a reverse-DNS-style identifier from the app bundle path.
/// Falls back to the process name if no `.app/` path is found.
pub fn resolve_bundle_id(pid: u32) -> String {
    let output = Command::new("lsof")
        .args(["-p", &pid.to_string(), "-Fn"])
        .stderr(std::process::Stdio::null())
        .output();

    if let Ok(out) = output {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            // lsof -Fn prefixes file name lines with 'n'.
            let path = line.strip_prefix('n').unwrap_or(line);
            if let Some(app_idx) = path.find(".app/") {
                // Extract the .app bundle name, e.g. "/Applications/Safari.app/..."
                // Walk backwards from .app to find the last '/' before it.
                let bundle_path = &path[..app_idx + 4]; // include ".app"
                let app_name = bundle_path
                    .rsplit('/')
                    .next()
                    .unwrap_or(bundle_path)
                    .strip_suffix(".app")
                    .unwrap_or(bundle_path);

                // Produce a bundle-id-like string. Real bundle IDs come from
                // Info.plist, but for our purposes a synthetic one works fine.
                if !app_name.is_empty() {
                    return format!("app.{}", app_name.to_lowercase());
                }
            }
        }
    }

    // Fallback: use process name as bundle identifier.
    let name = resolve_process_name(pid);
    format!("proc.{name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_process_name_for_init() {
        // PID 1 (launchd) should always exist on macOS.
        let name = resolve_process_name(1);
        assert!(!name.is_empty());
        assert_ne!(name, "unknown");
    }

    #[test]
    fn test_resolve_process_name_nonexistent() {
        // An absurdly high PID that almost certainly does not exist.
        let name = resolve_process_name(4_000_000_000);
        assert_eq!(name, "unknown");
    }

    #[test]
    fn test_resolve_bundle_id_fallback() {
        // For PID 1 (launchd), there is no .app bundle, so it should
        // fall back to proc.<name>.
        let bid = resolve_bundle_id(1);
        assert!(bid.starts_with("proc."));
    }
}
