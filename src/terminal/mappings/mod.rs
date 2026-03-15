pub mod colors;
pub mod keys;
pub mod mouse;

use std::collections::HashMap;
use std::sync::OnceLock;

/// macOS system paths configuration files
#[cfg(target_os = "macos")]
const PATHS_FILE: &str = "/etc/paths";
#[cfg(target_os = "macos")]
const PATHS_D_DIR: &str = "/etc/paths.d";

/// Common Homebrew paths for Apple Silicon and Intel Macs
#[cfg(target_os = "macos")]
const HOMEBREW_PATHS: &[&str] = &[
    "/opt/homebrew/bin",
    "/opt/homebrew/sbin",
    "/usr/local/bin",
    "/usr/local/sbin",
];

/// Parse paths from file content, adding valid paths to the collection
fn parse_paths_content(content: &str, paths: &mut Vec<String>) {
    for line in content.lines() {
        let line = line.trim();
        if !line.is_empty() && !line.starts_with('#') {
            paths.push(line.to_string());
        }
    }
}

/// Get the system PATH by reading /etc/paths and /etc/paths.d/*
/// This ensures shell has access to standard system commands even when
/// launched from Finder (which doesn't load shell profiles).
///
/// This function is cached using OnceLock to avoid file I/O on every terminal creation.
#[cfg(target_os = "macos")]
fn get_macos_system_path() -> &'static str {
    static SYSTEM_PATH: OnceLock<String> = OnceLock::new();

    SYSTEM_PATH.get_or_init(|| {
        let mut paths = Vec::with_capacity(16);

        // Add current PATH if available
        if let Ok(current_path) = std::env::var("PATH") {
            paths.push(current_path);
        }

        // Read /etc/paths
        if let Ok(content) = std::fs::read_to_string(PATHS_FILE) {
            parse_paths_content(&content, &mut paths);
        }

        // Read /etc/paths.d/*
        if let Ok(entries) = std::fs::read_dir(PATHS_D_DIR) {
            for entry in entries.flatten() {
                if let Ok(content) = std::fs::read_to_string(entry.path()) {
                    parse_paths_content(&content, &mut paths);
                }
            }
        }

        // Add Homebrew paths
        for path in HOMEBREW_PATHS {
            // Only add if not already present to avoid duplicates
            if !paths.iter().any(|p| p.contains(path)) {
                paths.push(path.to_string());
            }
        }

        paths.join(":")
    })
}

/// Get system PATH for non-macOS platforms (just returns current PATH)
#[cfg(not(target_os = "macos"))]
fn get_macos_system_path() -> &'static str {
    static SYSTEM_PATH: OnceLock<String> = OnceLock::new();

    SYSTEM_PATH.get_or_init(|| {
        std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin:/usr/sbin:/sbin".to_string())
    })
}

/// Creates the standard terminal environment variables.
/// Used by both local terminals and SSH PTY sessions.
pub fn default_terminal_env() -> HashMap<String, String> {
    let mut env = HashMap::new();
    env.insert("DEEPTERM".to_string(), "true".to_string());
    env.insert("TERM_PROGRAM".to_string(), "deepterm".to_string());
    env.insert(
        "TERM_PROGRAM_VERSION".to_string(),
        env!("CARGO_PKG_VERSION").to_string(),
    );
    env.insert("TERM".to_string(), "xterm-256color".to_string());
    env.insert("COLORTERM".to_string(), "truecolor".to_string());

    // Set a comprehensive PATH that includes system and Homebrew paths
    // This ensures shell initialization works correctly when launched from Finder
    let path = get_macos_system_path().to_string();
    env.insert("PATH".to_string(), path);

    // Set UTF-8 locale to ensure proper Unicode/Chinese character display
    // This fixes garbled text when the terminal is launched from Finder
    env.insert("LANG".to_string(), "en_US.UTF-8".to_string());
    env.insert("LC_ALL".to_string(), "en_US.UTF-8".to_string());

    env
}
