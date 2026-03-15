pub mod colors;
pub mod keys;
pub mod mouse;

use std::collections::HashMap;

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
    env
}
