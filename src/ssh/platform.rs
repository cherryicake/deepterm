//! Platform/OS detection and icon mapping
//!
//! Provides types for identifying remote server operating systems
//! and mapping them to appropriate icons.

use crate::assets::Icon;

/// Detected remote operating system
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RemoteOS {
    Macos,
    Ubuntu,
    Debian,
    Centos,
    Arch,
    Windows,
    LinuxMint,
    Fedora,
    Rhel,
    Opensuse,
    Manjaro,
    /// Generic Linux (fallback when distro is unknown)
    Linux,
    /// Could not detect
    Unknown,
}

impl RemoteOS {
    /// Parse OS ID from /etc/os-release ID field or uname output
    pub fn from_id(id: &str) -> Self {
        let id = id.trim();

        // Try full string first, then first word (for "Arch Linux" -> "arch")
        if let Some(os) = Self::match_distro(id) {
            return os;
        }
        if let Some(first_word) = id.split_whitespace().next() {
            if let Some(os) = Self::match_distro(first_word) {
                return os;
            }
        }
        RemoteOS::Unknown
    }

    fn match_distro(name: &str) -> Option<RemoteOS> {
        // 精确匹配（不分配内存）
        if name.eq_ignore_ascii_case("macos")
            || name.eq_ignore_ascii_case("darwin")
            || name.eq_ignore_ascii_case("osx")
        {
            return Some(RemoteOS::Macos);
        }
        if name.eq_ignore_ascii_case("windows")
            || name.eq_ignore_ascii_case("win32")
            || name.eq_ignore_ascii_case("win64")
        {
            return Some(RemoteOS::Windows);
        }
        if name.eq_ignore_ascii_case("linux") {
            return Some(RemoteOS::Linux);
        }

        // 前缀匹配：提取第一个词，避免重复调用 split_once
        let first_token = name.split_once('-').map(|(prefix, _)| prefix).unwrap_or(name);

        if first_token.eq_ignore_ascii_case("ubuntu") {
            return Some(RemoteOS::Ubuntu);
        }
        if first_token.eq_ignore_ascii_case("debian") {
            return Some(RemoteOS::Debian);
        }
        if first_token.eq_ignore_ascii_case("centos") {
            return Some(RemoteOS::Centos);
        }
        if first_token.eq_ignore_ascii_case("arch") || first_token.eq_ignore_ascii_case("archlinux") {
            return Some(RemoteOS::Arch);
        }
        if first_token.eq_ignore_ascii_case("linuxmint") || first_token.eq_ignore_ascii_case("mint") {
            return Some(RemoteOS::LinuxMint);
        }
        if first_token.eq_ignore_ascii_case("fedora") {
            return Some(RemoteOS::Fedora);
        }
        if first_token.eq_ignore_ascii_case("rhel")
            || first_token.eq_ignore_ascii_case("redhat")
            || first_token.eq_ignore_ascii_case("red hat enterprise linux")
        {
            return Some(RemoteOS::Rhel);
        }
        if first_token.eq_ignore_ascii_case("opensuse") || first_token.eq_ignore_ascii_case("suse") {
            return Some(RemoteOS::Opensuse);
        }
        if first_token.eq_ignore_ascii_case("manjaro") {
            return Some(RemoteOS::Manjaro);
        }
        if first_token.eq_ignore_ascii_case("kali") || first_token.eq_ignore_ascii_case("alpine") {
            return Some(RemoteOS::Linux);
        }

        None
    }

    /// Map to the corresponding icon
    pub fn icon(self) -> Icon {
        match self {
            RemoteOS::Macos => Icon::Macos,
            RemoteOS::Ubuntu => Icon::Ubuntu,
            RemoteOS::Debian => Icon::Debian,
            RemoteOS::Centos => Icon::Centos,
            RemoteOS::Arch => Icon::Arch,
            RemoteOS::Windows => Icon::Windows,
            RemoteOS::LinuxMint => Icon::LinuxMint,
            RemoteOS::Fedora => Icon::Fedora,
            RemoteOS::Rhel => Icon::Rhel,
            RemoteOS::Opensuse => Icon::Opensuse,
            RemoteOS::Manjaro => Icon::Manjaro,
            RemoteOS::Linux => Icon::Linux,
            RemoteOS::Unknown => Icon::Server,
        }
    }

    /// Get a display name for the OS
    pub fn display_name(self) -> &'static str {
        match self {
            RemoteOS::Macos => "macOS",
            RemoteOS::Ubuntu => "Ubuntu",
            RemoteOS::Debian => "Debian",
            RemoteOS::Centos => "CentOS",
            RemoteOS::Arch => "Arch Linux",
            RemoteOS::Windows => "Windows",
            RemoteOS::LinuxMint => "Linux Mint",
            RemoteOS::Fedora => "Fedora",
            RemoteOS::Rhel => "RHEL",
            RemoteOS::Opensuse => "openSUSE",
            RemoteOS::Manjaro => "Manjaro",
            RemoteOS::Linux => "Linux",
            RemoteOS::Unknown => "Unknown",
        }
    }

    /// Serialize to OS ID string for config storage
    pub fn to_id(self) -> &'static str {
        match self {
            RemoteOS::Macos => "macos",
            RemoteOS::Ubuntu => "ubuntu",
            RemoteOS::Debian => "debian",
            RemoteOS::Centos => "centos",
            RemoteOS::Arch => "arch",
            RemoteOS::Windows => "windows",
            RemoteOS::LinuxMint => "linuxmint",
            RemoteOS::Fedora => "fedora",
            RemoteOS::Rhel => "rhel",
            RemoteOS::Opensuse => "opensuse",
            RemoteOS::Manjaro => "manjaro",
            RemoteOS::Linux => "linux",
            RemoteOS::Unknown => "unknown",
        }
    }
}

/// Detect the local operating system
pub fn detect_local_os() -> RemoteOS {
    #[cfg(target_os = "macos")]
    {
        return RemoteOS::Macos;
    }
    #[cfg(target_os = "windows")]
    {
        RemoteOS::Windows
    }
    #[cfg(target_os = "linux")]
    {
        // Try to read /etc/os-release for the specific distro
        if let Ok(content) = std::fs::read_to_string("/etc/os-release") {
            if let Some(id_line) = content.lines().find(|l| l.starts_with("ID=")) {
                let id = id_line.trim_start_matches("ID=").trim_matches('"');
                let os = RemoteOS::from_id(id);
                if os != RemoteOS::Unknown {
                    return os;
                }
            }
        }
        return RemoteOS::Linux;
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        RemoteOS::Unknown
    }
}

/// Detect WSL distribution from command and args
///
/// When creating a WSL terminal on Windows, this function inspects the
/// command line to determine which Linux distro is being launched.
pub fn detect_wsl_os(command: &str, args: &[String]) -> RemoteOS {
    // Check if command is wsl.exe (avoid matching unrelated commands like wscript)
    let cmd_lower = command.to_lowercase();
    let is_wsl = cmd_lower.ends_with("wsl") || cmd_lower.ends_with("wsl.exe");
    if !is_wsl {
        return detect_local_os();
    }

    // Parse -d or --distribution argument
    let mut distro_name: Option<&str> = None;
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        if arg == "-d" || arg == "--distribution" {
            if let Some(name) = iter.next() {
                distro_name = Some(name.as_str());
                break;
            }
        }
        // Handle combined form like "-dUbuntu" but not "--distribution"
        if let Some(name) = arg.strip_prefix("-d") {
            if !name.is_empty() && arg.len() > 2 {
                distro_name = Some(name);
                break;
            }
        }
    }

    // Map distribution name to RemoteOS
    if let Some(name) = distro_name {
        let os = RemoteOS::from_id(name);
        if os != RemoteOS::Unknown {
            return os;
        }
    }

    // No -d arg: try to detect default WSL distro
    #[cfg(windows)]
    {
        // Use `wsl --list --verbose` to find the default distro (marked with *)
        if let Ok(output) = std::process::Command::new("wsl.exe")
            .args(["--list", "--verbose"])
            .output()
        {
            if output.status.success() {
                let text = String::from_utf8_lossy(&output.stdout);
                for line in text.lines() {
                    let trimmed = line.trim();
                    // Default distro line starts with "*"
                    if trimmed.starts_with('*') {
                        // Format: "* Ubuntu    Running    2"
                        let parts: Vec<&str> = trimmed.split_whitespace().collect();
                        if parts.len() >= 2 {
                            let name = parts[1];
                            let os = RemoteOS::from_id(name);
                            if os != RemoteOS::Unknown {
                                return os;
                            }
                        }
                        break;
                    }
                }
            }
        }
    }

    // Fallback to generic Linux
    RemoteOS::Linux
}

/// Guess OS from host name or nickname text (fallback before actual detection)
pub fn guess_os_from_text(text: &str) -> RemoteOS {
    let t = text.to_lowercase();
    if t.contains("ubuntu") {
        RemoteOS::Ubuntu
    } else if t.contains("debian") {
        RemoteOS::Debian
    } else if t.contains("centos") {
        RemoteOS::Centos
    } else if t.contains("fedora") {
        RemoteOS::Fedora
    } else if t.contains("arch") {
        RemoteOS::Arch
    } else if t.contains("manjaro") {
        RemoteOS::Manjaro
    } else if t.contains("mint") || t.contains("linuxmint") {
        RemoteOS::LinuxMint
    } else if t.contains("opensuse") || t.contains("suse") {
        RemoteOS::Opensuse
    } else if t.contains("rhel") || t.contains("redhat") {
        RemoteOS::Rhel
    } else if t.contains("windows") || t.contains("win") {
        RemoteOS::Windows
    } else if t.contains("mac") || t.contains("darwin") || t.contains("apple") {
        RemoteOS::Macos
    } else if t.contains("linux") {
        RemoteOS::Linux
    } else {
        RemoteOS::Unknown
    }
}
