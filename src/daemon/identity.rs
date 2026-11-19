//! core 在握手中报告的身份。
//!
//! `generation` 每次 core 启动都会变化，供 app 区分“core 重启了”与“同一条
//! core 上又开了一条连接”；`core_id` 则持久化在同一运行时目录里，供未来的
//! 机器列表把远端 core 当作稳定实体。两者都不是鉴权凭据。

use std::io::Write as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

/// systemd 在都没有调用方时也会无限保留用户管理器。
const SYSTEMD_INFINITY: &str = "18446744073709551615";
const LINGER_REFRESH: Duration = Duration::from_secs(60);
const LOGINCTL_TIMEOUT: Duration = Duration::from_secs(30);

/// linger 状态。`None` 表示无法证实“登出后一定会停”，不是“一定会停”。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct LingerReport {
    keeps_running: Option<bool>,
    user: Option<String>,
}

/// 一次 core 进程的握手身份。
#[derive(Debug)]
pub struct ServerIdentity {
    pub core_id: String,
    pub generation: String,
    pub hostname: String,
    linger: Mutex<LingerReport>,
}

/// 当前可执行文件的内容指纹。
///
/// core 与 holder 是**同一个二进制**的两个角色，但可以来自不同构建：升级
/// 后新 core 必须能认出旧 holder（它手里还有用户的终端），同时不能让新终端
/// 继续落到旧 holder 上。版本号做不到这一点——同一个 `0.1.0` 可以从不同源码
/// 构建出来。herdr 用链接器写入的 build id 区分，这里对可执行文件内容做
/// 一次 SHA-256；结果只算一次，之后是常量。
///
/// 注意这个指纹只在**同一台机器**内比较：GUI 与远端 core 是不同平台的
/// 二进制，跨机器不能用它判断版本。
pub fn build_id() -> &'static str {
    static BUILD: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    BUILD.get_or_init(|| {
        let Ok(exe) = std::env::current_exe() else {
            return format!("version:{}", env!("CARGO_PKG_VERSION"));
        };
        let Ok(mut file) = std::fs::File::open(&exe) else {
            return format!("version:{}", env!("CARGO_PKG_VERSION"));
        };
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            match std::io::Read::read(&mut file, &mut buffer) {
                Ok(0) => break,
                Ok(read) => hasher.update(&buffer[..read]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return format!("version:{}", env!("CARGO_PKG_VERSION")),
            }
        }
        let digest = hasher.finalize();
        let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        format!("sha256:{}", &hex[..32])
    })
}

impl ServerIdentity {
    /// 发现本机信息。调用方应保证运行时目录已经创建。
    pub fn detect() -> Self {
        let core_id = load_or_create_core_id().unwrap_or_else(|e| {
            log::warn!("无法持久化 core_id，本次进程使用临时值: {e}");
            uuid::Uuid::new_v4().to_string()
        });
        let linger = initial_linger_report(current_user());
        Self {
            core_id,
            generation: uuid::Uuid::new_v4().to_string(),
            hostname: hostname(),
            linger: Mutex::new(linger),
        }
    }

    pub fn keeps_running_after_logout(&self) -> Option<bool> {
        self.lock_linger().keeps_running
    }

    pub fn logout_linger_user(&self) -> Option<String> {
        self.lock_linger().user.clone()
    }

    #[cfg(test)]
    pub(crate) fn for_tests(
        core_id: &str,
        generation: &str,
        hostname: &str,
        keeps_running: bool,
        user: Option<&str>,
    ) -> Self {
        Self {
            core_id: core_id.to_string(),
            generation: generation.to_string(),
            hostname: hostname.to_string(),
            linger: Mutex::new(LingerReport {
                keeps_running: Some(keeps_running),
                user: user.map(str::to_owned),
            }),
        }
    }

    /// 启动后台 linger 探测。失败或权限不足只影响上报，不阻塞 core 启动。
    ///
    /// herdr 的取舍同样如此：polkit 可能让 `loginctl` 卡很久，所以
    /// `enable-linger` 必须放到独立线程里；状态每 60 秒重算一次。
    pub fn start_linger_refresh(self: &Arc<Self>) {
        let identity = self.clone();
        if std::thread::Builder::new()
            .name("linger-refresh".into())
            .spawn(move || identity.refresh_linger())
            .is_err()
        {
            log::warn!("无法启动 linger 探测线程");
        }
    }

    fn refresh_linger(&self) {
        let mut enable_attempted = false;
        loop {
            let user = self.lock_linger().user.clone();
            let mut report = probe_linger(user.as_deref());
            if !enable_attempted && report.keeps_running != Some(true) {
                enable_attempted = true;
                if let Some(user) = user.as_deref() {
                    try_enable_linger(user);
                    report = probe_linger(Some(user));
                }
            }
            *self.lock_linger() = report;
            std::thread::sleep(LINGER_REFRESH);
        }
    }

    fn lock_linger(&self) -> std::sync::MutexGuard<'_, LingerReport> {
        self.linger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn load_or_create_core_id() -> std::io::Result<String> {
    let path = super::runtime_dir().join("core-id");
    if let Ok(id) = read_valid_id(&path) {
        return Ok(id);
    }

    let generated = uuid::Uuid::new_v4().to_string();
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            file.write_all(generated.as_bytes())?;
            file.sync_all()?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
            Ok(generated)
        }
        // 另一个 core 抢先创建了：读它的值，保证同一台机器只有一个 core_id。
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => read_valid_id(&path),
        Err(e) => Err(e),
    }
}

fn read_valid_id(path: &std::path::Path) -> std::io::Result<String> {
    let text = std::fs::read_to_string(path)?;
    let id = text.trim();
    if uuid::Uuid::parse_str(id).is_ok() {
        Ok(id.to_string())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("core-id 内容无效: {}", path.display()),
        ))
    }
}

fn current_user() -> Option<String> {
    ["USER", "LOGNAME", "USERNAME"]
        .into_iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
}

/// 启动路径上只做一次尽量短的探测；拿不到结论时保持 `None`，由后台线程
/// 在 `loginctl enable-linger` 之后更新。
fn initial_linger_report(user: Option<String>) -> LingerReport {
    let keeps_running = match user.as_deref() {
        Some(user) if linger_marker_exists(user) => Some(true),
        Some(user) => probe_loginctl(user, Duration::from_secs(2)),
        None => None,
    };
    LingerReport {
        keeps_running,
        user,
    }
}

fn linger_marker_exists(user: &str) -> bool {
    std::path::Path::new("/var/lib/systemd/linger")
        .join(user)
        .exists()
}

fn probe_linger(user: Option<&str>) -> LingerReport {
    let Some(user) = user else {
        return LingerReport::default();
    };
    let keeps_running = if linger_marker_exists(user) {
        Some(true)
    } else {
        probe_loginctl(user, LOGINCTL_TIMEOUT)
    };
    LingerReport {
        keeps_running,
        user: Some(user.to_string()),
    }
}

/// 解析 `loginctl show-user` 的两行输出。
///
/// 只有能同时证实 Linger 关闭、且 UserStopDelayUSec 是有限值时才敢断言
/// `Some(false)`；无穷大表示 systemd 不会因为登出回收管理器，不能报警。
fn linger_from_loginctl(marker_exists: bool, output: &str) -> Option<bool> {
    if marker_exists {
        return Some(true);
    }
    let mut linger = None;
    let mut stop_delay = None;
    for line in output.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("Linger=") {
            linger = Some(value.trim().eq_ignore_ascii_case("yes"));
        } else if let Some(value) = line.strip_prefix("UserStopDelayUSec=") {
            stop_delay = Some(value.trim().to_string());
        }
    }
    match (linger, stop_delay.as_deref()) {
        (Some(true), _) => Some(true),
        (Some(false), Some(value))
            if value != SYSTEMD_INFINITY && !value.eq_ignore_ascii_case("infinity") =>
        {
            Some(false)
        }
        _ => None,
    }
}

fn probe_loginctl(user: &str, timeout: Duration) -> Option<bool> {
    let output = run_with_timeout(
        "loginctl",
        &[
            "show-user",
            user,
            "-p",
            "Linger",
            "-p",
            "UserStopDelayUSec",
            "--no-pager",
        ],
        timeout,
    )?;
    linger_from_loginctl(linger_marker_exists(user), &output)
}

fn try_enable_linger(user: &str) {
    let Some(output) = run_with_timeout(
        "loginctl",
        &["--no-ask-password", "enable-linger", "--", user],
        LOGINCTL_TIMEOUT,
    ) else {
        return;
    };
    if !output.trim().is_empty() {
        log::debug!("loginctl enable-linger {user}: {}", output.trim());
    }
}

fn run_with_timeout(program: &str, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                let output = child.wait_with_output().ok()?;
                let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
                text.push_str(&String::from_utf8_lossy(&output.stderr));
                return Some(text);
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                log::warn!("{program} 超过 {:?} 未返回，已终止", timeout);
                return None;
            }
            Err(_) => return None,
        }
    }
}

#[cfg(unix)]
fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: `gethostname` 只写调用方提供的缓冲区；`buf` 是本地数组，指针与长度都有效。
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if ok != 0 {
        return "unknown".to_string();
    }
    let end = buf.iter().position(|byte| *byte == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

#[cfg(not(unix))]
fn hostname() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostname_is_never_empty() {
        assert!(!hostname().is_empty());
    }

    #[test]
    fn invalid_core_id_files_are_rejected() {
        let path = std::env::temp_dir().join(format!(
            "deepterm-core-id-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&path, b"not-a-uuid").expect("write");
        assert!(read_valid_id(&path).is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn linger_requires_both_a_disabled_marker_and_a_finite_stop_delay() {
        assert_eq!(linger_from_loginctl(true, "Linger=no"), Some(true));
        assert_eq!(
            linger_from_loginctl(false, "Linger=yes\nUserStopDelayUSec=1000000"),
            Some(true)
        );
        assert_eq!(
            linger_from_loginctl(false, "Linger=no\nUserStopDelayUSec=1000000"),
            Some(false)
        );
        assert_eq!(
            linger_from_loginctl(false, "Linger=no\nUserStopDelayUSec=18446744073709551615"),
            None
        );
        assert_eq!(linger_from_loginctl(false, "Linger=no"), None);
        assert_eq!(linger_from_loginctl(false, ""), None);
    }
}
