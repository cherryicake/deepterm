//! core 在握手中报告的身份。
//!
//! `generation` 每次 core 启动都会变化，供 app 区分“core 重启了”与“同一条
//! core 上又开了一条连接”；`core_id` 则是**跨重启稳定**的实体标识，供机器
//! 列表把远端 core 当作同一个对象。两者都不是鉴权凭据。
//!
//! # `core_id` 为什么不放在运行时目录
//!
//! 运行时目录（`$XDG_RUNTIME_DIR/deepterm`）在 Linux 上是 **tmpfs**：重启、
//! 登出、或者 systemd 的 `RemoveIPC` 都会把它清空。身份文件放在那里，同一
//! 台机器的 core 每次重启都会换一个新 id，于是远端记着的 `remote_core_id`
//! 永远对不上，机器列表会一直卡在「同一目标上出现了另一个 core」。所以身份
//! 文件走 [`state_dir`]（`$XDG_STATE_HOME`，在 tmpfs 之外），只有 socket 留在
//! 运行时目录——与 herdr 的 `paths.rs` / `server.rs::load_identity` 同构。

use std::io::Write as _;
use std::sync::{Arc, Mutex};
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

/// systemd 在都没有调用方时也会无限保留用户管理器。
#[cfg(target_os = "linux")]
const SYSTEMD_INFINITY: &str = "18446744073709551615";
#[cfg(target_os = "linux")]
const LINGER_REFRESH: Duration = Duration::from_secs(60);
#[cfg(target_os = "linux")]
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

/// 当前可执行文件的构建指纹。
///
/// core 与 holder 是**同一个构建**的两个角色，但可以来自不同构建：升级后
/// 新 core 必须能认出旧 holder（它手里还有用户的终端），同时不能让新终端
/// 继续落到旧 holder 上。版本号做不到这一点——同一个 `0.1.0` 可以从不同源码
/// 构建出来。
///
/// # 为什么 macOS 上必须用 `LC_UUID` 而不是文件哈希
///
/// 分发时 `deepcore` 与 `deepholder` 是**两份分别签名的副本**（各自独立的
/// app bundle，活动监视器里才会分别显示 `DeepTerm Core` / `DeepHolder`）。
/// `codesign` 会往文件尾部写签名 blob，两份副本因此字节不同、SHA-256 也不同
/// ——而 `holder_is_current` 正是按这个指纹决定“新终端交给谁”。用文件哈希会
/// 让刚拉起来的 holder 永远被判成旧构建并立刻 drain。
///
/// `LC_UUID` 由链接器写入 Mach-O 头部，签名不会改动它（实测签名前后完全
/// 一致），因此它才是“同一份构建”的判据——这正是 herdr 用链接器 build id
/// 的原因。其余平台仍然用文件哈希。
///
/// 注意这个指纹只在**同一台机器**内比较：GUI 与远端 core 是不同平台的
/// 二进制，跨机器不能用它判断版本。
pub fn build_id() -> &'static str {
    static BUILD: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    BUILD.get_or_init(|| {
        let Ok(exe) = std::env::current_exe() else {
            return version_fallback();
        };
        #[cfg(target_os = "macos")]
        {
            match macho_build_id(&exe) {
                Some(id) => return id,
                // 解析失败只能退回文件哈希。这样两份分别签名的副本又会得到
                // 不同指纹，所以必须留下线索，否则现场表现只是“新 holder
                // 一启动就 drain”，没有任何日志能解释。
                None => log::warn!(
                    "无法从 {} 读取 LC_UUID，退回文件哈希；核心与 holder 若分别签名会被判成不同构建",
                    exe.display()
                ),
            }
        }
        file_digest(&exe).unwrap_or_else(version_fallback)
    })
}

fn version_fallback() -> String {
    format!("version:{}", env!("CARGO_PKG_VERSION"))
}

/// 对文件内容做一次 SHA-256（非 macOS 的构建指纹）。
fn file_digest(path: &std::path::Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        match std::io::Read::read(&mut file, &mut buffer) {
            Ok(0) => break,
            Ok(read) => hasher.update(&buffer[..read]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
    }
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    Some(format!("sha256:{}", &hex[..32]))
}

/// Mach-O 里 `LC_UUID` 的载入命令号。
#[cfg(target_os = "macos")]
const LC_UUID: u32 = 0x1b;
/// 64 位 Mach-O 头部长度（load command 从这里开始）。
#[cfg(target_os = "macos")]
const MACHO_HEADER_64: usize = 32;
#[cfg(target_os = "macos")]
const MH_MAGIC_64: u32 = 0xfeed_facf;
#[cfg(target_os = "macos")]
const FAT_MAGIC_32: u32 = 0xcafe_babe;
#[cfg(target_os = "macos")]
const FAT_MAGIC_64: u32 = 0xcafe_babf;

/// 读当前可执行文件的 `LC_UUID`，格式化成 `macho:<UUID>`。
#[cfg(target_os = "macos")]
fn macho_build_id(path: &std::path::Path) -> Option<String> {
    use std::io::Read as _;

    let mut file = std::fs::File::open(path).ok()?;
    // 头部 + load command 都在文件最前面；64 KiB 足够覆盖任何正常构建。
    let mut header = vec![0u8; 64 * 1024];
    let mut filled = 0;
    while filled < header.len() {
        match file.read(&mut header[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
    }
    header.truncate(filled);
    macho_uuid(&header).map(|uuid| format!("macho:{uuid}"))
}

/// 从 Mach-O 字节里取 `LC_UUID`；fat 二进制取第一个架构切片。
#[cfg(target_os = "macos")]
fn macho_uuid(bytes: &[u8]) -> Option<String> {
    let start = match read_be32(bytes, 0)? {
        FAT_MAGIC_32 => {
            if read_be32(bytes, 4)? == 0 {
                return None;
            }
            read_be32(bytes, 16)? as usize
        }
        FAT_MAGIC_64 => {
            if read_be32(bytes, 4)? == 0 {
                return None;
            }
            let offset: [u8; 8] = bytes.get(16..24)?.try_into().ok()?;
            usize::try_from(u64::from_be_bytes(offset)).ok()?
        }
        _ => 0,
    };

    if read_le32(bytes, start)? != MH_MAGIC_64 {
        // 本项目的 macOS 构建都是 64 位，32 位 Mach-O 不认。
        return None;
    }
    let ncmds = read_le32(bytes, start + 16)?;
    let mut at = start + MACHO_HEADER_64;
    for _ in 0..ncmds {
        let command = read_le32(bytes, at)?;
        let size = read_le32(bytes, at + 4)? as usize;
        if size < 8 {
            return None;
        }
        if command == LC_UUID {
            return Some(format_uuid(bytes.get(at + 8..at + 24)?));
        }
        at += size;
    }
    None
}

#[cfg(target_os = "macos")]
fn read_be32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

#[cfg(target_os = "macos")]
fn read_le32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// 16 字节 UUID → 标准 `XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX` 大写形式。
#[cfg(target_os = "macos")]
fn format_uuid(raw: &[u8]) -> String {
    let hex: String = raw.iter().map(|byte| format!("{byte:02X}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
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
    ///
    /// **只在 Linux 上跑。** `loginctl` 是 systemd 的命令，macOS / Windows
    /// 上根本不存在：那条线程会每 60 秒 `spawn` 一个必然失败的子进程，
    /// 直到进程退出，且永远不会被 join。
    pub fn start_linger_refresh(self: &Arc<Self>) {
        #[cfg(target_os = "linux")]
        {
            let identity = self.clone();
            if std::thread::Builder::new()
                .name("linger-refresh".into())
                .spawn(move || identity.refresh_linger())
                .is_err()
            {
                log::warn!("无法启动 linger 探测线程");
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = self;
        }
    }

    #[cfg(target_os = "linux")]
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
    let path = super::state_dir().join("core-id");
    load_or_create_core_id_at(&path, &super::runtime_dir().join("core-id"))
}

/// [`load_or_create_core_id`] 的可注入版本。
///
/// `legacy` 是早期版本写身份的位置（运行时目录）。它只在**新位置还没有身份**
/// 时被读一次，读到就搬到新位置：升级不该让一台已经在用的机器换 id，否则
/// 远端记着的 `remote_core_id` 立刻失配。
fn load_or_create_core_id_at(
    path: &std::path::Path,
    legacy: &std::path::Path,
) -> std::io::Result<String> {
    if let Ok(id) = read_valid_id(path) {
        return Ok(id);
    }

    // 迁移：旧的运行时目录文件还在的话，沿用它的值。
    let inherited = read_valid_id(legacy).ok();

    let generated = inherited
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    if let Some(parent) = path.parent() {
        // 首次运行时状态目录还没有；0700 与运行时目录一致。
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
        }
        #[cfg(not(unix))]
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => {
            file.write_all(generated.as_bytes())?;
            file.sync_all()?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
            }
            if inherited.is_some() {
                log::info!(
                    "core_id 已迁移到持久目录 {}（沿用 {} 的身份）",
                    path.display(),
                    legacy.display()
                );
            }
            Ok(generated)
        }
        // 另一个 core 抢先创建了：读它的值，保证同一台机器只有一个 core_id。
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => read_valid_id(path),
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
///
/// 非 Linux 上没有 systemd，`Linger` 概念不成立：直接保持 `None`
/// （= "无法证实登出后一定会停"），而不是启动一个必然失败的子进程。
fn initial_linger_report(user: Option<String>) -> LingerReport {
    #[cfg(target_os = "linux")]
    let keeps_running = match user.as_deref() {
        Some(user) if linger_marker_exists(user) => Some(true),
        Some(user) => probe_loginctl(user, Duration::from_secs(2)),
        None => None,
    };
    #[cfg(not(target_os = "linux"))]
    let keeps_running = None;
    LingerReport {
        keeps_running,
        user,
    }
}

#[cfg(target_os = "linux")]
fn linger_marker_exists(user: &str) -> bool {
    std::path::Path::new("/var/lib/systemd/linger")
        .join(user)
        .exists()
}

#[cfg(target_os = "linux")]
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
#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
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

    /// **签名后的两份副本必须得到同一个指纹。**
    ///
    /// 回归价值：`codesign` 会往文件尾部写签名 blob，而打包时 core 与 holder
    /// 是两次独立签名。若指纹按文件内容算，两份字节不同 → 新 holder 一启动
    /// 就被 `holder_is_current` 判成旧构建并立刻 drain，现场表现是“新终端
    /// 打不开”且没有任何日志。
    #[test]
    #[cfg(target_os = "macos")]
    fn the_uuid_survives_codesign_rewrites() {
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        let original = std::fs::read(&exe).expect("读测试二进制");
        let uuid = macho_uuid(&original).expect("测试二进制应有 LC_UUID");

        // 模拟 codesign：改尾部字节（签名 blob 所在区），并改掉文件长度。
        let mut signed = original.clone();
        let tail = signed.len().saturating_sub(4096);
        for byte in &mut signed[tail..] {
            *byte ^= 0xff;
        }
        signed.extend_from_slice(b"fake-signature");
        assert_eq!(
            macho_uuid(&signed).as_deref(),
            Some(uuid.as_str()),
            "签名改写尾部后 LC_UUID 必须不变"
        );
        assert_ne!(
            file_digest(&exe).expect("文件哈希"),
            {
                let digest = Sha256::digest(&signed);
                let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
                format!("sha256:{}", &hex[..32])
            },
            "这条测试的前提是文件哈希确实会变，否则测不到回归"
        );
    }

    /// fat 二进制取第一个架构切片的 UUID。
    #[test]
    #[cfg(target_os = "macos")]
    fn fat_binaries_use_the_first_slice() {
        // 构造一个最小的 64 位 Mach-O：ncmds=1，唯一一条 LC_UUID。
        let slice = |uuid: [u8; 16]| {
            let mut bytes = vec![0u8; MACHO_HEADER_64];
            bytes[0..4].copy_from_slice(&MH_MAGIC_64.to_le_bytes());
            bytes[16..20].copy_from_slice(&1u32.to_le_bytes());
            bytes.extend_from_slice(&LC_UUID.to_le_bytes());
            bytes.extend_from_slice(&24u32.to_le_bytes());
            bytes.extend_from_slice(&uuid);
            bytes
        };
        let arm = slice([0xab; 16]);
        let offset = 4096usize;
        let mut fat = vec![0u8; offset];
        fat[0..4].copy_from_slice(&FAT_MAGIC_64.to_be_bytes());
        fat[4..8].copy_from_slice(&1u32.to_be_bytes());
        fat[16..24].copy_from_slice(&(offset as u64).to_be_bytes());
        fat.extend_from_slice(&arm);

        assert_eq!(
            macho_uuid(&fat),
            Some("ABABABAB-ABAB-ABAB-ABAB-ABABABABABAB".to_string())
        );
    }

    /// 非 Mach-O 输入返回 `None`，由调用方退回文件哈希。
    #[test]
    #[cfg(target_os = "macos")]
    fn garbage_is_not_mistaken_for_a_macho() {
        assert_eq!(macho_uuid(b"not a mach-o file"), None);
        assert_eq!(macho_uuid(&[]), None);
    }

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

    /// 升级后第一次启动：身份要从运行时目录搬进持久目录，且**值不变**。
    ///
    /// 变值的后果不是本机可见的：远端机器表里记着的 `remote_core_id` 立刻
    /// 失配，那台机器永久停在「同一目标上出现了另一个 core」。
    #[test]
    fn a_core_id_in_the_runtime_dir_moves_to_the_state_dir_unchanged() {
        let root = std::env::temp_dir().join(format!("deepterm-id-move-{}", uuid::Uuid::new_v4()));
        let legacy = root.join("run/core-id");
        let path = root.join("state/core-id");
        std::fs::create_dir_all(legacy.parent().unwrap()).expect("runtime dir");
        let existing = uuid::Uuid::new_v4().to_string();
        std::fs::write(&legacy, &existing).expect("old id");

        assert_eq!(load_or_create_core_id_at(&path, &legacy).expect("load"), existing);
        assert_eq!(
            std::fs::read_to_string(&path).expect("new file").trim(),
            existing,
            "新位置必须写着同一个 id"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// 持久目录已有身份时，旧运行时文件即使还在也不能覆盖它。
    #[test]
    fn the_state_dir_id_wins_over_a_stale_runtime_copy() {
        let root = std::env::temp_dir().join(format!("deepterm-id-wins-{}", uuid::Uuid::new_v4()));
        let legacy = root.join("run/core-id");
        let path = root.join("state/core-id");
        std::fs::create_dir_all(legacy.parent().unwrap()).expect("runtime dir");
        std::fs::create_dir_all(path.parent().unwrap()).expect("state dir");
        let durable = uuid::Uuid::new_v4().to_string();
        std::fs::write(&path, &durable).expect("durable id");
        std::fs::write(&legacy, uuid::Uuid::new_v4().to_string()).expect("stale id");

        assert_eq!(load_or_create_core_id_at(&path, &legacy).expect("load"), durable);
        std::fs::remove_dir_all(&root).ok();
    }

    /// 全新机器（两处都没有）：生成一个并落盘，第二次读到的必须是同一个。
    #[test]
    fn a_fresh_core_id_survives_the_next_load() {
        let root = std::env::temp_dir().join(format!("deepterm-id-fresh-{}", uuid::Uuid::new_v4()));
        let legacy = root.join("run/core-id");
        let path = root.join("state/core-id");

        let first = load_or_create_core_id_at(&path, &legacy).expect("create");
        assert!(path.is_file(), "身份必须立刻落盘，不能只留在内存里");
        let second = load_or_create_core_id_at(&path, &legacy).expect("reload");
        assert_eq!(first, second);
        std::fs::remove_dir_all(&root).ok();
    }

    #[cfg(target_os = "linux")]
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
