//! 远端 `deepcore` 的发布包解析、下载、校验与安装。
//!
//! 发布格式与 herdr 对齐：GitHub Release 上每个 Linux target 一份
//! `deepcore-linux-<arch>.tar.gz`，旁边是 `deepcore-manifest.json`。manifest
//! 记录版本、下载地址与 SHA-256；只有本地校验通过的归档才会被上传。
//!
//! 安装过程刻意分成“先在临时路径验证、最后原子替换”两步：
//!
//! 1. 解出归档里的唯一普通文件 `deepcore`，限制大小；
//! 2. 通过 SFTP 上传到目标目录的临时文件，创建权限直接是 0700；
//! 3. 远端执行 `--version` 探针，确认架构/链接兼容；
//! 4. 探针成功后才 `mv -f` 覆盖固定安装路径。
//!
//! 任何一步失败都保留旧二进制。远端已有 core 的更新还需要停掉旧 core，
//! 但绝不碰 holder——holder 持有 PTY，终端跨 core 更新保留。

use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ssh::SshConnectionConfig;
use crate::ssh::sftp_fs::BlockingSftpSession;

/// 远端固定安装目录（相对 `$HOME`）。
const INSTALL_DIR: &str = ".local/share/deepterm";
const CORE_BINARY: &str = "deepcore";
const MANIFEST_FILE: &str = "deepcore-manifest.json";

/// 发布 manifest 的环境变量覆盖。
///
/// 值应当是包含 manifest 与各 target 归档的目录 URL。测试可以把本地 HTTP
/// 服务指过来；正常发布不设置，走仓库的 `releases/download/v{version}`。
pub const RELEASES_URL_ENV: &str = "DEEPTERM_RELEASES_URL";

const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
const MAX_ARCHIVE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_BINARY_BYTES: u64 = 128 * 1024 * 1024;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(90);

const MISSING_PROBE_MARKER: &str = "__DEEPTERM_CORE_MISSING__";

/// 本进程期望的 core 版本。
pub fn core_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// 默认发布目录。
///
/// # 为什么是精确版本而不是 `releases/latest/download`
///
/// 发布侧的 manifest 里写的是**各自 tag** 下的归档 URL（见
/// `.github/workflows/release.yml`）。而 GitHub 的 `releases/latest` 只指向
/// 最新的**正式**发布：打预发布 tag（`0.2.0-rc1`）时它仍指向旧的正式版，
/// 客户端会把那个版本的 manifest 取回来，然后因为「manifest 版本 ≠ 内置
/// 版本」而报错——安装永远走不通。
///
/// 用 `releases/download/v{version}` 让请求与内置版本严格对应：manifest 里
/// 的版本、归档的版本、远端探针校验的版本三者天然一致，预发布也能按
/// tag 直接访问。
pub fn default_releases_url() -> String {
    format!(
        "{}/releases/download/v{}",
        env!("CARGO_PKG_REPOSITORY").trim_end_matches('/'),
        core_version()
    )
}

/// 实际使用的发布目录。
pub fn releases_url() -> String {
    resolve_releases_url(std::env::var(RELEASES_URL_ENV).ok())
}

/// [`releases_url`] 的纯函数形式。
///
/// 把环境变量读取拆出来，测试才能在不修改进程环境的情况下验证覆盖逻辑。
/// （`std::env::set_var` 在多线程下是 unsafe 的，而 `cargo test` 默认
/// 并行跑用例——真的去改环境会引入数据竞争。）
fn resolve_releases_url(override_value: Option<String>) -> String {
    override_value
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(default_releases_url)
}

fn manifest_url() -> String {
    format!("{}/{MANIFEST_FILE}", releases_url())
}

/// 远端 `uname` 能识别的 Linux target。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoreTarget {
    LinuxX86_64,
    LinuxAarch64,
}

impl CoreTarget {
    pub fn key(self) -> &'static str {
        match self {
            Self::LinuxX86_64 => "linux-x86_64",
            Self::LinuxAarch64 => "linux-aarch64",
        }
    }
}

/// `uname -sm` → 发布 target。
pub fn target_from_uname(output: &str) -> Result<CoreTarget, String> {
    let mut fields = output.split_whitespace();
    let system = fields.next().unwrap_or_default();
    let machine = fields.next().unwrap_or_default();
    if !system.eq_ignore_ascii_case("linux") {
        return Err(format!("远端不是 Linux（uname -s = {system:?}）"));
    }
    match machine.to_ascii_lowercase().as_str() {
        "x86_64" | "amd64" => Ok(CoreTarget::LinuxX86_64),
        "aarch64" | "arm64" => Ok(CoreTarget::LinuxAarch64),
        _ => Err(format!(
            "暂不支持远端架构 {machine:?}，目前发布 linux-x86_64 与 linux-aarch64"
        )),
    }
}

/// `deepcore --version` 的版本字段。
pub fn parse_core_version(output: &str) -> Option<String> {
    let line = output.lines().find(|line| !line.trim().is_empty())?.trim();
    let version = line.strip_prefix("deepcore ")?.trim();
    if valid_version(version) {
        Some(version.to_string())
    } else {
        None
    }
}

fn valid_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 64
        && version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+' | b'_'))
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CoreArtifact {
    pub url: String,
    pub sha256: String,
    #[serde(default)]
    pub size: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CoreManifest {
    pub version: String,
    pub targets: HashMap<String, CoreArtifact>,
}

impl CoreManifest {
    fn parse(bytes: &[u8]) -> Result<Self, String> {
        let manifest: Self = serde_json::from_slice(bytes)
            .map_err(|e| format!("core manifest 不是有效 JSON: {e}"))?;
        manifest.validate()?;
        Ok(manifest)
    }

    fn validate(&self) -> Result<(), String> {
        if !valid_version(&self.version) {
            return Err(format!("core manifest 的版本无效: {:?}", self.version));
        }
        if self.targets.is_empty() {
            return Err("core manifest 没有 targets".into());
        }
        for (key, artifact) in &self.targets {
            if !valid_sha256(&artifact.sha256) {
                return Err(format!("target {key} 的 sha256 无效"));
            }
            if artifact.url.trim().is_empty()
                || !(artifact.url.starts_with("https://")
                    || artifact.url.starts_with("http://")
                    || artifact.url.starts_with("file://"))
            {
                return Err(format!("target {key} 的下载地址无效: {}", artifact.url));
            }
            if artifact.size.is_some_and(|size| size > MAX_BINARY_BYTES) {
                return Err(format!("target {key} 声明的大小超过限制"));
            }
        }
        Ok(())
    }

    fn artifact_for(&self, target: CoreTarget) -> Result<&CoreArtifact, String> {
        self.targets
            .get(target.key())
            .ok_or_else(|| format!("core manifest 没有 {} 产物", target.key()))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallMode {
    /// 已有同版本且能运行就不再联网。
    Ensure,
    /// 显式安装/修复：同版本也重新下载并替换。
    Force,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallOutcome {
    AlreadyInstalled {
        version: String,
        target: CoreTarget,
    },
    Installed {
        version: String,
        target: CoreTarget,
        previous_version: Option<String>,
    },
}

/// 在远端探测、下载并安装/更新 core。阻塞调用，应在机器监督线程里跑。
pub fn ensure_remote_core(
    config: &SshConnectionConfig,
    session: Option<&str>,
    mode: InstallMode,
) -> Result<InstallOutcome, String> {
    let mut sftp = BlockingSftpSession::connect(config)
        .map_err(|e| format!("无法通过 SFTP 连接远端（安装 deepcore）: {e}"))?;

    let (code, stdout, stderr) = sftp
        .exec_capture("uname -sm")
        .map_err(|e| format!("无法探测远端平台: {e}"))?;
    if code != Some(0) {
        return Err(format!(
            "远端 uname 失败（退出码 {code:?}）: {}",
            format_remote_error(&stdout, &stderr)
        ));
    }
    let target = target_from_uname(&stdout)?;

    let home = remote_home(&sftp)?;
    let install_path = remote_core_path(&home, session);
    let previous_version = probe_remote_version(&sftp, &install_path)?;

    if mode == InstallMode::Ensure && previous_version.as_deref() == Some(core_version()) {
        return Ok(InstallOutcome::AlreadyInstalled {
            version: core_version().to_string(),
            target,
        });
    }

    let manifest_bytes = fetch_url(&manifest_url(), MAX_MANIFEST_BYTES)
        .map_err(|e| format!("下载 core manifest 失败: {e}"))?;
    let manifest = CoreManifest::parse(&manifest_bytes)?;
    if manifest.version != core_version() {
        return Err(format!(
            "远端需要版本 {}，但应用内置版本是 {}；请先更新 DeepTerm",
            manifest.version,
            core_version()
        ));
    }
    let artifact = manifest.artifact_for(target)?.clone();

    let archive = fetch_url(&artifact.url, MAX_ARCHIVE_BYTES)
        .map_err(|e| format!("下载 {} core 失败: {e}", target.key()))?;
    if let Some(size) = artifact.size
        && archive.len() as u64 != size
    {
        return Err(format!(
            "{} 归档大小不符：manifest {size} 字节，实际 {} 字节",
            target.key(),
            archive.len()
        ));
    }
    let actual_sha = sha256_hex(&archive);
    if !actual_sha.eq_ignore_ascii_case(&artifact.sha256) {
        return Err(format!(
            "{} 归档 SHA-256 校验失败：期望 {}，实际 {}",
            target.key(),
            artifact.sha256,
            actual_sha
        ));
    }

    let extracted = extract_core_binary(&archive)?;
    let expected_size = std::fs::metadata(extracted.path())
        .map_err(|e| format!("读取已校验 core 大小失败: {e}"))?
        .len();
    if expected_size > MAX_BINARY_BYTES {
        return Err(format!("core 二进制过大（{expected_size} 字节）"));
    }

    let install_dir = Path::new(&install_path)
        .parent()
        .ok_or_else(|| "远端安装路径没有父目录".to_string())?;
    let install_dir = install_dir.to_string_lossy();
    let mkdir = sftp.exec_capture(&format!(
        "mkdir -p {} && chmod 700 {}",
        shell_quote(&install_dir),
        shell_quote(&install_dir)
    ));
    let (code, stdout, stderr) = mkdir.map_err(|e| format!("创建远端安装目录失败: {e}"))?;
    if code != Some(0) {
        return Err(format!(
            "创建远端安装目录失败: {}",
            format_remote_error(&stdout, &stderr)
        ));
    }

    let temp_path = format!("{install_path}.new-{}", uuid::Uuid::new_v4());
    let cancelled = AtomicBool::new(false);
    let uploaded = sftp.upload_file_from_local_with_permissions(
        extracted.path(),
        &temp_path,
        0o700,
        &cancelled,
    );
    let uploaded = match uploaded {
        Ok(uploaded) => uploaded,
        Err(e) => {
            let _ = remove_remote_file(&sftp, &temp_path);
            return Err(format!("上传 core 到远端失败: {e}"));
        }
    };
    if uploaded != expected_size {
        let _ = remove_remote_file(&sftp, &temp_path);
        return Err(format!(
            "core 上传不完整：期望 {expected_size} 字节，实际 {uploaded} 字节"
        ));
    }

    let probe = probe_version_at(&sftp, &temp_path);
    let probed_version = match probe {
        Ok(version) => version,
        Err(e) => {
            let _ = remove_remote_file(&sftp, &temp_path);
            return Err(format!("远端 core 探针失败，保留旧版本: {e}"));
        }
    };
    if probed_version != core_version() {
        let _ = remove_remote_file(&sftp, &temp_path);
        return Err(format!(
            "远端 core 探针版本不符：期望 {}，实际 {probed_version}",
            core_version()
        ));
    }

    let replace = sftp.exec_capture(&format!(
        "chmod 700 {temp} && mv -f {temp} {target}",
        temp = shell_quote(&temp_path),
        target = shell_quote(&install_path)
    ));
    let (code, stdout, stderr) = replace.map_err(|e| format!("替换远端 core 失败: {e}"))?;
    if code != Some(0) {
        let _ = remove_remote_file(&sftp, &temp_path);
        return Err(format!(
            "替换远端 core 失败: {}",
            format_remote_error(&stdout, &stderr)
        ));
    }

    if previous_version.is_some() {
        restart_remote_core(&sftp, session)?;
    }

    Ok(InstallOutcome::Installed {
        version: core_version().to_string(),
        target,
        previous_version,
    })
}

fn remote_home(sftp: &BlockingSftpSession) -> Result<String, String> {
    let (code, stdout, stderr) = sftp
        .exec_capture("printf '%s' \"$HOME\"")
        .map_err(|e| format!("读取远端 HOME 失败: {e}"))?;
    let home = stdout.trim();
    if code != Some(0) || !home.starts_with('/') || home.contains('\n') {
        return Err(format!(
            "远端 HOME 无效: {}",
            format_remote_error(&stdout, &stderr)
        ));
    }
    Ok(home.to_string())
}

/// 固定安装路径。带 session 的测试/并行安装写到 session 自己的目录，
/// 不会覆盖用户正在使用的生产 core。
pub fn remote_core_path(home: &str, session: Option<&str>) -> String {
    match session {
        Some(session) => format!("{home}/{INSTALL_DIR}/sessions/{session}/{CORE_BINARY}"),
        None => format!("{home}/{INSTALL_DIR}/{CORE_BINARY}"),
    }
}

fn probe_remote_version(sftp: &BlockingSftpSession, path: &str) -> Result<Option<String>, String> {
    match probe_version_at(sftp, path) {
        Ok(version) => Ok(Some(version)),
        Err(e) if e == MISSING_PROBE_MARKER => Ok(None),
        Err(e) => Err(format!("检查远端 {path} 失败: {e}")),
    }
}

fn probe_version_at(sftp: &BlockingSftpSession, path: &str) -> Result<String, String> {
    let command = format!(
        "if [ ! -x {path} ]; then echo {marker}; exit 3; fi; exec {path} --version",
        path = shell_quote(path),
        marker = shell_quote(MISSING_PROBE_MARKER)
    );
    let (code, stdout, stderr) = sftp
        .exec_capture(&command)
        .map_err(|e| format!("执行远端探针失败: {e}"))?;
    if code == Some(3) && stdout.trim() == MISSING_PROBE_MARKER {
        return Err(MISSING_PROBE_MARKER.to_string());
    }
    if code != Some(0) {
        return Err(format!(
            "--version 退出码 {code:?}: {}",
            format_remote_error(&stdout, &stderr)
        ));
    }
    parse_core_version(&stdout).ok_or_else(|| format!("无法解析 --version 输出: {}", stdout.trim()))
}

/// 只停 core，不碰 holder。`daemon.pid` 是 core 自己写的；先确认进程确实
/// 是可执行的 deepcore 且不是 holder，避免异常 pid 文件误杀别的进程。
fn restart_remote_core(sftp: &BlockingSftpSession, session: Option<&str>) -> Result<(), String> {
    let query = match session {
        Some(session) => format!("printf '%s' \"$HOME/{INSTALL_DIR}/sessions/{session}\""),
        None => "runtime=${DEEPTERM_RUNTIME_DIR:-${XDG_RUNTIME_DIR:+$XDG_RUNTIME_DIR/deepterm}}; \
                 runtime=${runtime:-$HOME/.config/deepterm}; printf '%s' \"$runtime\""
            .to_string(),
    };
    let (code, stdout, stderr) = sftp
        .exec_capture(&query)
        .map_err(|e| format!("读取远端运行时目录失败: {e}"))?;
    if code != Some(0) {
        return Err(format!(
            "读取远端运行时目录失败: {}",
            format_remote_error(&stdout, &stderr)
        ));
    }
    let runtime = stdout.trim();
    if !runtime.starts_with('/') {
        return Err(format!("远端运行时目录无效: {runtime:?}"));
    }

    let script = format!(
        "runtime={runtime}; pidfile=\"$runtime/daemon.pid\"; \
         [ -r \"$pidfile\" ] || exit 0; \
         pid=$(cat \"$pidfile\" 2>/dev/null) || exit 0; \
         case \"$pid\" in ''|*[!0-9]*) exit 0;; esac; \
         exe=$(readlink \"/proc/$pid/exe\" 2>/dev/null) || exit 0; \
         case \"$exe\" in */deepcore|*/deepcore\\ \\(deleted\\)) ;; *) exit 0;; esac; \
         if tr '\\0' ' ' < \"/proc/$pid/cmdline\" 2>/dev/null | grep -q -- '--holder'; then exit 0; fi; \
         kill \"$pid\" 2>/dev/null || true; \
         i=0; while kill -0 \"$pid\" 2>/dev/null && [ \"$i\" -lt 50 ]; do sleep 0.1; i=$((i+1)); done; \
         if kill -0 \"$pid\" 2>/dev/null; then kill -9 \"$pid\" 2>/dev/null || true; sleep 0.2; fi",
        runtime = shell_quote(runtime),
    );
    let (code, stdout, stderr) = sftp
        .exec_capture(&script)
        .map_err(|e| format!("停止旧远端 core 失败: {e}"))?;
    if code != Some(0) {
        return Err(format!(
            "停止旧远端 core 失败: {}",
            format_remote_error(&stdout, &stderr)
        ));
    }
    Ok(())
}

fn remove_remote_file(sftp: &BlockingSftpSession, path: &str) -> Result<(), String> {
    let (code, stdout, stderr) = sftp
        .exec_capture(&format!("rm -f -- {}", shell_quote(path)))
        .map_err(|e| e.to_string())?;
    if code != Some(0) {
        return Err(format_remote_error(&stdout, &stderr));
    }
    Ok(())
}

fn format_remote_error(stdout: &str, stderr: &str) -> String {
    let text = format!("{stdout}{stderr}");
    let trimmed = text.trim();
    if trimmed.is_empty() {
        "（远端没有输出）".into()
    } else {
        trimmed.chars().take(4096).collect()
    }
}

/// POSIX 单引号转义。安装路径来自远端 HOME 与已校验 session，仍然统一转义，
/// 防止用户名或 HOME 中的引号改变远端 shell 语义。
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 已解出的本地临时文件；离开作用域就删除。
struct ExtractedCore {
    path: PathBuf,
}

impl ExtractedCore {
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ExtractedCore {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// 只接受归档根目录下唯一的普通文件 `deepcore`。
///
/// 不做通用解包：解包器本身就是攻击面（路径穿越、symlink、hardlink）。
/// 发布包格式由 CI 固定，严格拒绝其他布局比“宽松兼容”安全得多。
fn extract_core_binary(archive: &[u8]) -> Result<ExtractedCore, String> {
    let decoder = GzDecoder::new(Cursor::new(archive));
    let mut archive = tar::Archive::new(decoder);
    let temp_path = std::env::temp_dir().join(format!(
        "deepterm-core-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));

    let extracted = ExtractedCore {
        path: temp_path.clone(),
    };
    let mut found = false;
    let mut entry_count = 0usize;
    let entries = archive
        .entries()
        .map_err(|e| format!("core 归档无法读取: {e}"))?;
    for entry in entries {
        entry_count += 1;
        if entry_count > 64 {
            return Err("core 归档条目过多".into());
        }
        let mut entry = entry.map_err(|e| format!("core 归档条目损坏: {e}"))?;
        let path = entry
            .path()
            .map_err(|e| format!("core 归档路径无效: {e}"))?
            .into_owned();
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            continue;
        }
        if !kind.is_file() {
            return Err(format!("core 归档不允许非普通文件: {}", path.display()));
        }
        if !is_core_entry(&path) {
            return Err(format!("core 归档包含意外文件: {}", path.display()));
        }
        if found {
            return Err("core 归档包含多个 deepcore 文件".into());
        }
        let size = entry.header().size().unwrap_or(0);
        if size > MAX_BINARY_BYTES {
            return Err(format!("core 二进制过大（归档声明 {size} 字节）"));
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|e| format!("创建临时 core 文件失败: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&temp_path, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| format!("设置临时 core 权限失败: {e}"))?;
        }
        let mut limited = (&mut entry).take(MAX_BINARY_BYTES + 1);
        let written = std::io::copy(&mut limited, &mut file)
            .map_err(|e| format!("写出临时 core 失败: {e}"))?;
        if written > MAX_BINARY_BYTES {
            return Err("core 二进制超过大小限制".into());
        }
        if written != size {
            return Err(format!(
                "core 二进制被截断：归档声明 {size} 字节，实际 {written} 字节"
            ));
        }
        file.sync_all()
            .map_err(|e| format!("刷新临时 core 失败: {e}"))?;
        found = true;
    }

    if !found {
        return Err("core 归档里没有 deepcore".into());
    }
    Ok(extracted)
}

fn is_core_entry(path: &Path) -> bool {
    let mut components = path.components();
    matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(name)), None) if name == CORE_BINARY
    )
}

fn fetch_url(url: &str, max_bytes: u64) -> Result<Vec<u8>, String> {
    if let Some(path) = url.strip_prefix("file://") {
        let decoded = percent_decode_file_path(path)?;
        let path = Path::new(&decoded);
        let metadata = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if metadata.len() > max_bytes {
            return Err(format!("文件超过 {} 字节限制", max_bytes));
        }
        return std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()));
    }
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(format!("不支持的 URL: {url}"));
    }

    let response = ureq::get(url)
        .config()
        .timeout_global(Some(DOWNLOAD_TIMEOUT))
        .max_redirects(5)
        .build()
        .call()
        .map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    if let Some(length) = response.body().content_length()
        && length > max_bytes
    {
        return Err(format!("响应超过 {} 字节限制", max_bytes));
    }
    let bytes = response
        .into_body()
        .into_with_config()
        .limit(max_bytes + 1)
        .read_to_vec()
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > max_bytes {
        return Err(format!("响应超过 {} 字节限制", max_bytes));
    }
    Ok(bytes)
}

fn percent_decode_file_path(value: &str) -> Result<String, String> {
    let mut bytes = Vec::with_capacity(value.len());
    let raw = value.as_bytes();
    let mut index = 0;
    while index < raw.len() {
        if raw[index] == b'%' {
            if index + 2 >= raw.len() {
                return Err("file URL 的百分号编码不完整".into());
            }
            let hex = std::str::from_utf8(&raw[index + 1..index + 3])
                .map_err(|_| "file URL 的百分号编码无效".to_string())?;
            let byte =
                u8::from_str_radix(hex, 16).map_err(|_| "file URL 的百分号编码无效".to_string())?;
            bytes.push(byte);
            index += 3;
        } else {
            bytes.push(raw[index]);
            index += 1;
        }
    }
    String::from_utf8(bytes).map_err(|_| "file URL 不是 UTF-8".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;

    fn manifest() -> CoreManifest {
        CoreManifest {
            version: "0.1.0".into(),
            targets: HashMap::from([(
                "linux-x86_64".into(),
                CoreArtifact {
                    url: "https://example.test/deepcore-linux-x86_64.tar.gz".into(),
                    sha256: "a".repeat(64),
                    size: Some(42),
                },
            )]),
        }
    }

    fn archive_with(entries: &[(&str, &[u8], tar::EntryType)]) -> Vec<u8> {
        let mut output = GzEncoder::new(Vec::new(), Compression::default());
        {
            let mut tar = tar::Builder::new(&mut output);
            for (path, body, kind) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(body.len() as u64);
                header.set_mode(0o755);
                header.set_cksum();
                header.set_entry_type(*kind);
                tar.append_data(&mut header, path, *body).unwrap();
            }
            tar.finish().unwrap();
        }
        output.finish().unwrap()
    }

    #[test]
    fn uname_maps_only_supported_linux_targets() {
        assert_eq!(
            target_from_uname("Linux x86_64\n").unwrap(),
            CoreTarget::LinuxX86_64
        );
        assert_eq!(
            target_from_uname("Linux aarch64").unwrap(),
            CoreTarget::LinuxAarch64
        );
        assert!(target_from_uname("Linux armv7l").is_err());
        assert!(target_from_uname("Darwin arm64").is_err());
    }

    #[test]
    fn manifest_rejects_bad_checksums_and_unknown_targets() {
        let mut value = manifest();
        assert!(value.artifact_for(CoreTarget::LinuxX86_64).is_ok());
        assert!(value.artifact_for(CoreTarget::LinuxAarch64).is_err());
        value.targets.get_mut("linux-x86_64").unwrap().sha256 = "xyz".into();
        assert!(value.validate().is_err());
    }

    #[test]
    fn manifest_round_trips_through_json() {
        let original = manifest();
        let bytes = serde_json::to_vec(&original).unwrap();
        assert_eq!(CoreManifest::parse(&bytes).unwrap(), original);
    }

    /// **客户端请求的目录必须与发布侧写的 tag 对得上。**
    ///
    /// 回归价值：`default_releases_url` 曾经用 `releases/latest/download`，
    /// 而 manifest 生成时写的是 `releases/download/<tag>`。两者在正式版上
    /// 恰好等价，**打预发布 tag 时就分叉**：`latest` 仍指向旧的正式版，
    /// 客户端取回旧 manifest，然后因为版本不符而永远装不上。
    ///
    /// 这里钉住协议：目录是 `releases/download/v{version}`，且版本号与
    /// `Cargo.toml` 一致——发布工作流正是要求 tag 去掉 `v` 前缀后等于它。
    #[test]
    fn default_releases_url_matches_the_release_tag() {
        let url = default_releases_url();
        let expected = format!(
            "{}/releases/download/v{}",
            env!("CARGO_PKG_REPOSITORY").trim_end_matches('/'),
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(url, expected);
        assert!(
            !url.contains("/releases/latest/"),
            "latest 无法定位预发布 tag，会让自动安装 404 或取到旧版本"
        );
    }

    /// 环境变量覆盖优先于默认目录，且忽略空值与尾部斜杠。
    ///
    /// 走纯函数而不是 `std::env::set_var`：后者在多线程下是 unsafe 的，
    /// 而 `cargo test` 并行跑用例，改环境会让其它用例读到中间状态。
    #[test]
    fn releases_url_prefers_the_override() {
        assert_eq!(
            resolve_releases_url(Some("http://127.0.0.1:9/custom/".into())),
            "http://127.0.0.1:9/custom"
        );
        assert_eq!(
            resolve_releases_url(Some("   ".into())),
            default_releases_url(),
            "空值应当回落到默认目录"
        );
        assert_eq!(resolve_releases_url(None), default_releases_url());
    }

    #[test]
    fn version_parser_requires_the_deepcore_prefix() {
        assert_eq!(parse_core_version("deepcore 0.1.0\n"), Some("0.1.0".into()));
        assert_eq!(
            parse_core_version("deepcore 0.1.0-rc.1"),
            Some("0.1.0-rc.1".into())
        );
        assert_eq!(parse_core_version("0.1.0"), None);
        assert_eq!(parse_core_version("deepcore bad version"), None);
    }

    #[test]
    fn archive_extracts_only_the_fixed_core_entry() {
        let bytes = archive_with(&[("deepcore", b"binary", tar::EntryType::Regular)]);
        let extracted = extract_core_binary(&bytes).unwrap();
        assert_eq!(std::fs::read(extracted.path()).unwrap(), b"binary");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(extracted.path())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn archive_rejects_path_traversal_and_symlinks() {
        assert!(!is_core_entry(Path::new("../deepcore")));
        assert!(!is_core_entry(Path::new("./deepcore")));
        assert!(!is_core_entry(Path::new("nested/deepcore")));
        let nested = archive_with(&[("nested/deepcore", b"x", tar::EntryType::Regular)]);
        assert!(extract_core_binary(&nested).is_err());

        let symlink = archive_with(&[("deepcore", b"../../bin/sh", tar::EntryType::Symlink)]);
        assert!(extract_core_binary(&symlink).is_err());
    }

    #[test]
    fn checksum_has_a_stable_known_value() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn remote_core_paths_keep_test_sessions_isolated() {
        let home = "/home/tester";
        assert_eq!(
            remote_core_path(home, None),
            "/home/tester/.local/share/deepterm/deepcore"
        );
        assert_eq!(
            remote_core_path(home, Some("e2e-1")),
            "/home/tester/.local/share/deepterm/sessions/e2e-1/deepcore"
        );
    }

    #[test]
    fn shell_quoting_survives_single_quotes() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }
}
