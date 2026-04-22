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
        if id.eq_ignore_ascii_case("macos")
            || id.eq_ignore_ascii_case("darwin")
            || id.eq_ignore_ascii_case("osx")
        {
            return RemoteOS::Macos;
        }
        if id.eq_ignore_ascii_case("ubuntu") {
            return RemoteOS::Ubuntu;
        }
        if id.eq_ignore_ascii_case("debian") {
            return RemoteOS::Debian;
        }
        if id.eq_ignore_ascii_case("centos") {
            return RemoteOS::Centos;
        }
        if id.eq_ignore_ascii_case("arch") {
            return RemoteOS::Arch;
        }
        if id.eq_ignore_ascii_case("windows")
            || id.eq_ignore_ascii_case("win32")
            || id.eq_ignore_ascii_case("win64")
        {
            return RemoteOS::Windows;
        }
        if id.eq_ignore_ascii_case("linuxmint") || id.eq_ignore_ascii_case("mint") {
            return RemoteOS::LinuxMint;
        }
        if id.eq_ignore_ascii_case("fedora") {
            return RemoteOS::Fedora;
        }
        if id.eq_ignore_ascii_case("rhel")
            || id.eq_ignore_ascii_case("redhat")
            || id.eq_ignore_ascii_case("red hat enterprise linux")
        {
            return RemoteOS::Rhel;
        }
        if id.eq_ignore_ascii_case("opensuse")
            || id.eq_ignore_ascii_case("opensuse-leap")
            || id.eq_ignore_ascii_case("opensuse-tumbleweed")
            || id.eq_ignore_ascii_case("suse")
        {
            return RemoteOS::Opensuse;
        }
        if id.eq_ignore_ascii_case("manjaro") {
            return RemoteOS::Manjaro;
        }
        if id.eq_ignore_ascii_case("linux") {
            return RemoteOS::Linux;
        }
        RemoteOS::Unknown
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
        return RemoteOS::Windows;
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
