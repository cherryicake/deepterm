//! A one-shot launchd job gives the core its own coalition.
//!
//! This is a direct port of herdr-core's macOS launchd handoff. `setsid` alone
//! leaves the core in the GUI's coalition, which macOS can tear down when the
//! GUI exits together with its privacy responsibility.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DETACH: &str = "DEEPTERM_LAUNCHD_DETACH";
const SENTINEL: &str = "--deepterm-launchd";
pub(super) const DIRECT: &str = "DEEPTERM_LAUNCHD_DIRECT";
const LAUNCHCTL: &str = "/bin/launchctl";
const PREFIX: &str = "dev.deepterm.core.";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(serde::Serialize, serde::Deserialize)]
struct Inherited {
    env: Vec<(Vec<u8>, Vec<u8>)>,
    limits: Vec<(libc::c_int, libc::rlim_t, libc::rlim_t)>,
}

impl Inherited {
    fn capture(cmd: &Command) -> io::Result<Self> {
        let mut env: BTreeMap<_, _> = std::env::vars_os().collect();
        for (key, value) in cmd.get_envs() {
            match value {
                Some(value) => {
                    env.insert(key.to_owned(), value.to_owned());
                }
                None => {
                    env.remove(key);
                }
            }
        }
        env.remove(OsStr::new(DETACH));
        env.remove(OsStr::new(DIRECT));
        env.remove(OsStr::new("XPC_FLAGS"));
        env.insert("XPC_SERVICE_NAME".into(), "0".into());
        let mut limits = Vec::new();
        // Darwin's RLIM_NLIMITS is not stable across OS versions.
        for resource in 0.. {
            let mut limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY: `limit` 是本地 `libc::rlimit`，指针有效且可写；
            // `getrlimit` 只写它。
            if unsafe { libc::getrlimit(resource, &mut limit) } != 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EINVAL) {
                    break;
                }
                return Err(error);
            }
            limits.push((resource, limit.rlim_cur, limit.rlim_max));
        }
        Ok(Self {
            env: env
                .into_iter()
                .map(|(key, value)| (key.into_vec(), value.into_vec()))
                .collect(),
            limits,
        })
    }

    /// Runs only in the newly forked core, before it creates any threads.
    fn restore(self) -> io::Result<()> {
        for (resource, soft, hard) in self.limits {
            let mut current = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY: `current` 是本地 `libc::rlimit`，指针有效且可写。
            if unsafe { libc::getrlimit(resource, &mut current) } != 0 {
                return Err(io::Error::last_os_error());
            }
            // Darwin can clamp NPROC even when setting its inherited value.
            if (current.rlim_cur, current.rlim_max) == (soft, hard) {
                continue;
            }
            let limit = libc::rlimit {
                rlim_cur: soft,
                rlim_max: hard,
            };
            // SAFETY: `limit` 由上面的 `getrlimit` 读出的真实值构造，只提升
            // 到继承的值；指针有效。
            if unsafe { libc::setrlimit(resource, &limit) } != 0 {
                return Err(io::Error::other(format!(
                    "cannot restore resource limit {resource}: {}",
                    io::Error::last_os_error()
                )));
            }
            // SAFETY: 同前，`current` 是本地的可写 `libc::rlimit`。
            if unsafe { libc::getrlimit(resource, &mut current) } != 0 {
                return Err(io::Error::last_os_error());
            }
            if (current.rlim_cur, current.rlim_max) != (soft, hard) {
                return Err(io::Error::other(format!(
                    "resource limit {resource} was clamped instead of inherited"
                )));
            }
        }
        // SAFETY: 本函数只在新 fork 出来、**还没起任何线程**的 core 里跑，
        // 因此没有其它线程可能在并发读写环境表——这是 `set_var`/`remove_var`
        // 被标 unsafe 的唯一原因。`key` 来自 `vars_os()`，是表里真实存在的键。
        unsafe {
            for (key, _) in std::env::vars_os() {
                std::env::remove_var(key);
            }
            for (key, value) in self.env {
                std::env::set_var(OsString::from_vec(key), OsString::from_vec(value));
            }
        }
        Ok(())
    }
}

/// Detach only the internal launchd sentinel before logging or runtime setup.
pub fn startup() -> bool {
    if std::env::args_os().nth(1).as_deref() != Some(OsStr::new(SENTINEL)) {
        return false;
    }
    let result = if let Some(socket) = std::env::var_os(DETACH) {
        detach(Path::new(&socket))
    } else if let Some(socket) = std::env::args_os().nth(2) {
        receive(Path::new(&socket))
    } else {
        return false;
    };
    if let Err(error) = result {
        eprintln!("deepcore: launchd detach failed: {error}");
        std::process::exit(1);
    }
    true
}

/// Also catches the first core started directly by an older launcher.
pub fn prepare_core(log: &Path) {
    let direct = std::env::var(DIRECT)
        .ok()
        .and_then(|pid| pid.parse::<u32>().ok())
        == Some(parent_pid());
    // SAFETY: 这是 core 的启动早期路径，还没有起任何线程，因此不存在与
    // `getenv` 的并发访问——`remove_var` 的 unsafe 前提即在于此。
    unsafe {
        std::env::remove_var(DIRECT);
    }
    if direct
        || isolated_runtime()
        || ssh()
        || std::env::args_os().nth(1).as_deref() == Some(OsStr::new(SENTINEL))
    {
        return;
    }
    let result = (|| {
        let mut cmd = Command::new(std::env::current_exe()?);
        cmd.env_remove(DIRECT);
        launch(&cmd, log)
    })();
    match result {
        Ok(()) => std::process::exit(0),
        Err(error) => fallback(log, &error),
    }
}

fn ssh() -> bool {
    std::env::var_os("SSH_CONNECTION").is_some()
}

/// Tests and parallel installations use a private runtime directory. Never move
/// those cores into the user's real GUI launchd domain.
fn isolated_runtime() -> bool {
    std::env::var_os(super::super::RUNTIME_DIR_ENV).is_some_and(|value| !value.is_empty())
}

pub(super) fn must_skip() -> bool {
    ssh() || isolated_runtime()
}

fn parent_pid() -> u32 {
    // SAFETY: `getppid` 只读取内核里的父进程号，无参数、无内存访问，不会失败。
    unsafe { libc::getppid() as u32 }
}

pub fn spawn(cmd: &mut Command, log: &Path) -> bool {
    if must_skip() {
        return false;
    }
    match launch(cmd, log) {
        Ok(()) => true,
        Err(error) => {
            fallback(log, &error);
            // Do not retry in a directly spawned fixed binary after a failure.
            cmd.env(DIRECT, std::process::id().to_string());
            false
        }
    }
}

fn fallback(log: &Path, error: &io::Error) {
    let reason = format!(
        "deepcore: launchd startup failed; starting directly (may end with the launching app): {error}"
    );
    eprintln!("{reason}");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
    {
        let _ = writeln!(file, "{reason}");
    }
}

fn launch(cmd: &Command, log: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let inherited = Inherited::capture(cmd)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?;
    // SAFETY: `getuid` 只读当前进程的真实 uid，无参数、无内存访问，不会失败。
    let domain = format!("gui/{}", unsafe { libc::getuid() });
    remove_stale(&domain, now);
    remove_stale_dirs(Path::new("/tmp"), now);
    let dir = PathBuf::from(format!(
        "/tmp/deepterm-launchd-{}-{}-{}",
        std::process::id(),
        now.as_nanos(),
        random_hex()
    ));
    std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    let mut job = Job {
        dir,
        target: format!("{domain}/{PREFIX}{}.{}", std::process::id(), now.as_nanos()),
        loaded: false,
    };
    let socket = job.dir.join("ready");
    let listener = UnixListener::bind(&socket)?;
    listener.set_nonblocking(true)?;
    let label = job.target.rsplit('/').next().unwrap();
    // SAFETY: `umask` 只读写进程级的文件创建掩码，是纯标量操作。先设成 077
    // 再立刻还原，读回原值以便下面构造 launchd 的 `Umask` 键；两次调用之间
    // 本进程没有创建文件（同一线程，顺序执行）。
    let mask = unsafe {
        let mask = libc::umask(0o077);
        libc::umask(mask);
        mask
    };
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)?;
    let contents = plist(cmd, log, label, &socket, mask)?;
    let file = job.dir.join("job.plist");
    std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&file)?
        .write_all(contents.as_bytes())?;
    let bootstrapped = launchctl(&[
        OsStr::new("bootstrap"),
        OsStr::new(&domain),
        file.as_os_str(),
    ]);
    let _ = std::fs::remove_file(file);
    if let Err(error) = bootstrapped {
        let _ = launchctl(&[OsStr::new("bootout"), OsStr::new(&job.target)]);
        return Err(error);
    }
    job.loaded = true;
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    // RunAtLoad is disabled: a second trigger could relaunch the short-lived
    // parent after it has already detached the core.
    if let Err(error) = job.request("kickstart", deadline) {
        return Err(job.failure("kickstart", error));
    }
    loop {
        match listener.accept() {
            Ok((mut ready, _)) => {
                ready.set_nonblocking(false)?;
                ready.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
                ready.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
                job.remove()?;
                serde_json::to_writer(&mut ready, &inherited)?;
                ready.shutdown(std::net::Shutdown::Write)?;
                let mut ack = [0];
                ready.read_exact(&mut ack).map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!(
                            "{}: waiting for inherited-state acknowledgement (job removed): {error}",
                            job.target
                        ),
                    )
                })?;
                return Ok(());
            }
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                return Err(job.failure(
                    "waiting for detach connection",
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        "no detach connection within 10 s (the target may be an older binary)",
                    ),
                ));
            }
            Err(error) => return Err(error),
        }
    }
}

fn detach(socket: &Path) -> io::Result<()> {
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg(SENTINEL).arg(socket).env_remove(DETACH);
    // SAFETY: 这里 `fork` 之后子进程只做 async-signal-safe 的事——`setsid`
    // 与紧接着的 `execvp`——不会再碰父进程的任何锁或堆状态；父进程立刻
    // `_exit`，不跑析构、不碰 stdio 缓冲。这正是 fork 在多线程进程里安全
    // 使用的前提。
    match unsafe { libc::fork() } {
        -1 => return Err(io::Error::last_os_error()),
        0 => {}
        // SAFETY: 在父进程里用 `_exit`（而不是 `exit`），跳过 atexit 与
        // 缓冲区 flush，避免与子进程重复写出同一批数据。
        _ => unsafe { libc::_exit(0) },
    }
    // SAFETY: 刚 fork 出的子进程一定不是进程组组长，`setsid` 必然成功；
    // 它无参数、不改内存。
    if unsafe { libc::setsid() } < 0 {
        return Err(io::Error::last_os_error());
    }
    // libSystem can initialize libnotify before main. fork disables it in the
    // child; exec gives the detached core fresh libnotify/libdispatch state.
    super::macos_responsibility::exec(cmd)
}

fn receive(socket: &Path) -> io::Result<()> {
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    // SAFETY: `getppid` 只读内核里的父进程号；这里用它判断自己是否已被
    // 交给 init（ppid == 1），是脱离原会话成功的标志。
    while unsafe { libc::getppid() } != 1 {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "launcher did not exit",
            ));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let mut ready = UnixStream::connect(socket)?;
    ready.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let inherited: Inherited = serde_json::from_reader(&mut ready)?;
    inherited.restore()?;
    ready.write_all(&[1])
}

struct Job {
    dir: PathBuf,
    target: String,
    loaded: bool,
}

impl Job {
    fn request(&self, action: &str, deadline: Instant) -> io::Result<String> {
        let path = self.dir.join("launchctl.out");
        let output = std::fs::File::create(&path)?;
        let mut child = Command::new(LAUNCHCTL)
            .args([action, &self.target])
            .stdin(Stdio::null())
            .stderr(output.try_clone()?)
            .stdout(output)
            .spawn()?;
        let status = wait_startup_command(&mut child, deadline)?;
        let mut output = String::new();
        std::fs::File::open(path)?
            .take(64 * 1024)
            .read_to_string(&mut output)?;
        if status.success() {
            Ok(output)
        } else {
            Err(io::Error::other(format!(
                "launchctl {action}: {} ({status})",
                output.trim()
            )))
        }
    }

    fn failure(&self, phase: &str, error: io::Error) -> io::Error {
        let state = match self.request("print", Instant::now() + Duration::from_secs(1)) {
            Ok(output) => job_state(&output),
            Err(error) => format!("job state unavailable: {error}"),
        };
        io::Error::new(
            error.kind(),
            format!("{}: {phase}: {error}; {state}", self.target),
        )
    }

    fn remove(&mut self) -> io::Result<()> {
        if self.loaded {
            launchctl(&[OsStr::new("bootout"), OsStr::new(&self.target)])?;
            self.loaded = false;
        }
        Ok(())
    }
}

fn wait_startup_command(
    child: &mut std::process::Child,
    deadline: Instant,
) -> io::Result<std::process::ExitStatus> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            result => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(result.err().unwrap_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        "launchctl startup command exceeded its deadline",
                    )
                }));
            }
        }
    }
}

fn job_state(output: &str) -> String {
    let fields: Vec<_> = output
        .lines()
        .filter(|line| line.starts_with('\t') && !line.starts_with("\t\t"))
        .map(str::trim)
        .filter(|line| {
            [
                "state =",
                "pid =",
                "runs =",
                "last exit code =",
                "last terminating signal =",
            ]
            .iter()
            .any(|key| line.starts_with(key))
        })
        .collect();
    if fields.is_empty() {
        "no spawn state reported".into()
    } else {
        fields.join("; ")
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        if let Err(error) = self.remove() {
            eprintln!(
                "deepcore: cannot remove launchd job {}: {error}",
                self.target
            );
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn launchctl(args: &[&OsStr]) -> io::Result<String> {
    let out = Command::new(LAUNCHCTL)
        .args(args)
        .stdin(Stdio::null())
        .output()?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(io::Error::other(format!(
            "launchctl {}: {} ({})",
            args[0].to_string_lossy(),
            String::from_utf8_lossy(&out.stderr).trim(),
            out.status
        )))
    }
}

fn remove_stale(domain: &str, now: Duration) {
    let Ok(list) = launchctl(&[OsStr::new("list")]) else {
        return;
    };
    for label in stale_labels(&list, now) {
        let target = format!("{domain}/{label}");
        if let Err(error) = launchctl(&[OsStr::new("bootout"), OsStr::new(&target)]) {
            eprintln!("deepcore: cannot remove finished launchd job {target}: {error}");
        }
    }
}

fn remove_stale_dirs(root: &Path, now: Duration) {
    use std::os::unix::fs::MetadataExt;
    for entry in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        // SAFETY: `getuid` 只读当前进程的真实 uid，无参数、无内存访问。
        if !meta.is_dir() || meta.uid() != unsafe { libc::getuid() } {
            continue;
        }
        let name = entry.file_name();
        let Some(attempt) = name
            .to_str()
            .and_then(|name| name.strip_prefix("deepterm-launchd-"))
        else {
            continue;
        };
        let mut fields = attempt.splitn(3, '-');
        let Some(pid) = fields
            .next()
            .and_then(|pid| pid.parse::<libc::pid_t>().ok())
            .filter(|pid| *pid > 0)
        else {
            continue;
        };
        let Some(stamp) = fields.next().and_then(|stamp| stamp.parse::<u128>().ok()) else {
            continue;
        };
        if !fields
            .next()
            .is_some_and(|id| id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
            || now.as_nanos().saturating_sub(stamp) < Duration::from_secs(60).as_nanos()
        {
            continue;
        }
        // ESRCH only: a live/reused pid is never evidence that this launch ended.
        // SAFETY: 信号 0 只做存在性检查，不投递任何信号；`pid` 已过滤为正数，
        // 且来自本进程自己写的目录名。即便 pid 被复用，下面的判据也只认
        // ESRCH，不会误杀活进程。
        if unsafe { libc::kill(pid, 0) } == 0
            || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
        {
            continue;
        }
        if let Err(error) = std::fs::remove_dir_all(&path) {
            eprintln!(
                "deepcore: cannot remove abandoned launchd directory {}: {error}",
                path.display()
            );
        }
    }
}

fn stale_labels(list: &str, now: Duration) -> Vec<&str> {
    list.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            if fields.next()? != "-" {
                return None;
            }
            let label = fields.nth(1)?;
            let stamp = label
                .strip_prefix(PREFIX)?
                .rsplit_once('.')?
                .1
                .parse::<u128>()
                .ok()?;
            (now.as_nanos().saturating_sub(stamp) >= Duration::from_secs(60).as_nanos())
                .then_some(label)
        })
        .collect()
}

fn plist(
    cmd: &Command,
    log: &Path,
    label: &str,
    socket: &Path,
    mask: libc::mode_t,
) -> io::Result<String> {
    let mut result =
        String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict>");
    let pair = |key: &str, value: &OsStr| -> io::Result<String> {
        Ok(format!("<key>{key}</key><string>{}</string>", xml(value)?))
    };
    result.push_str(&pair("Label", OsStr::new(label))?);
    result.push_str("<key>ProgramArguments</key><array>");
    // An older target must reject the job, not start a core with launchd's
    // default environment before it knows how to receive the caller's state.
    for arg in [cmd.get_program(), OsStr::new(SENTINEL)] {
        result.push_str(&format!("<string>{}</string>", xml(arg)?));
    }
    result.push_str("</array><key>EnvironmentVariables</key><dict>");
    result.push_str(&pair(DETACH, socket.as_os_str())?);
    result.push_str("</dict>");
    let cwd = cmd
        .get_current_dir()
        .map(Path::to_path_buf)
        .map_or_else(std::env::current_dir, Ok)?;
    for (key, value) in [
        ("WorkingDirectory", cwd.as_os_str()),
        ("StandardInPath", OsStr::new("/dev/null")),
        ("StandardOutPath", OsStr::new("/dev/null")),
        ("StandardErrorPath", log.as_os_str()),
        ("ProcessType", OsStr::new("Interactive")),
    ] {
        result.push_str(&pair(key, value)?);
    }
    result.push_str(&format!(
        "<key>RunAtLoad</key><false/><key>KeepAlive</key><false/><key>AbandonProcessGroup</key><true/><key>Umask</key><integer>{mask}</integer></dict></plist>"
    ));
    Ok(result)
}

fn xml(value: &OsStr) -> io::Result<String> {
    let text = value
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "launchd value is not UTF-8"))?;
    let mut result = String::new();
    for c in text.chars() {
        match c {
            '&' => result.push_str("&amp;"),
            '<' => result.push_str("&lt;"),
            '>' => result.push_str("&gt;"),
            '"' => result.push_str("&quot;"),
            '\t' | '\n' => result.push(c),
            c if c < ' ' || c == '\u{fffe}' || c == '\u{ffff}' => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "launchd value contains an XML control character",
                ));
            }
            c => result.push(c),
        }
    }
    Ok(result)
}

fn random_hex() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_command_timeout_reaps_only_its_child() {
        let mut child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        let start = Instant::now();
        let error =
            wait_startup_command(&mut child, start + Duration::from_millis(30)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(child.try_wait().unwrap().is_some());
        // SAFETY: 信号 0 只做存在性检查；这里断言子进程已被回收，
        // 因此应当返回 -1/ESRCH。
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1, "child was reaped");
    }

    #[test]
    fn only_old_finished_core_jobs_are_removed() {
        let now = Duration::from_secs(1000);
        let old = Duration::from_secs(900).as_nanos();
        let recent = Duration::from_secs(990).as_nanos();
        let label = format!("{PREFIX}1.{old}");
        let list = format!(
            "PID\tStatus\tLabel\n-\t0\t{label}\n42\t0\t{PREFIX}2.{old}\n-\t0\t{PREFIX}3.{recent}\n-\t0\tother.4.{old}\n"
        );
        assert_eq!(stale_labels(&list, now), vec![label.as_str()]);
    }

    #[test]
    fn plist_escapes_paths_and_rejects_control_characters() {
        assert_eq!(
            xml(OsStr::new("<&>\"'\t\n")).unwrap(),
            "&lt;&amp;&gt;&quot;'\t\n"
        );
        assert!(xml(OsStr::new("line\rbreak")).is_err());
    }
}
