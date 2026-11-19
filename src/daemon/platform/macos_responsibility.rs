//! Core and holder own their privacy responsibility, not terminal programs.

use std::ffi::{CString, OsStr};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::process::Command;

type Disclaim = unsafe extern "C" fn(*mut libc::posix_spawnattr_t, libc::c_int) -> libc::c_int;
const MARKER: &str = "_DEEPTERM_RESPONSIBLE_PID";

pub fn holder_startup() {
    if std::env::args_os().nth(1).as_deref() != Some(OsStr::new(super::super::holder::HOLDER_FLAG))
    {
        return;
    }
    let marked = std::env::var(MARKER).ok() == Some(std::process::id().to_string());
    // SAFETY: 这是单线程的启动早期路径（holder 还没起任何线程，也没有 reader
    // 在并发读环境变量），`setenv`/`unsetenv` 在此不会与其它线程的 `getenv`
    // 争用同一份环境表——这正是它被标成 unsafe 的唯一原因。
    unsafe { std::env::remove_var(MARKER) };
    if marked {
        return;
    }
    let result = (|| {
        // AppKit must discover the signed bundle, including when invoked
        // through a symlink or from inside the host app.
        let exe = std::env::current_exe()?;
        let exe = exe
            .canonicalize()
            .ok()
            .filter(|path| super::macos_bundle::source_is_bundle(path))
            .unwrap_or(exe);
        let mut cmd = Command::new(exe);
        cmd.args(std::env::args_os().skip(1))
            .env(MARKER, std::process::id().to_string());
        exec(cmd)
    })();
    if let Err(error) = result {
        eprintln!("deepcore: cannot disclaim holder responsibility: {error}");
        std::process::exit(1);
    }
}

/// SETEXEC leaves no short-lived wrapper as the privacy subject.
pub fn exec(cmd: Command) -> io::Result<()> {
    // SAFETY: `dlsym` 只按符号名查找已加载镜像，不访问本进程内存；
    // 找不到时返回空指针，下面显式跳过。
    let symbol = unsafe {
        libc::dlsym(
            libc::RTLD_DEFAULT,
            c"responsibility_spawnattrs_setdisclaim".as_ptr(),
        )
    };
    // SAFETY: 该符号在 libSystem 里的真实签名就是 `Disclaim`（
    // `int(int, posix_spawnattr_t *)`）；上面已确认非空才转换。
    let disclaim = (!symbol.is_null())
        .then(|| unsafe { std::mem::transmute::<*mut libc::c_void, Disclaim>(symbol) });
    exec_or_inherit(cmd, disclaim)
}

fn exec_or_inherit(mut cmd: Command, disclaim: Option<Disclaim>) -> io::Result<()> {
    match disclaim {
        Some(disclaim) => {
            let error = exec_with(&cmd, disclaim).unwrap_err();
            eprintln!(
                "deepcore: responsibility disclaimer failed ({error}); using inherited responsibility"
            );
        }
        None => eprintln!(
            "deepcore: responsibility disclaimer unavailable; using inherited responsibility"
        ),
    }
    Err(cmd.exec())
}

fn cstring(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes()).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
}

fn exec_with(cmd: &Command, disclaim: Disclaim) -> io::Result<()> {
    let program = cstring(cmd.get_program())?;
    let argv: Vec<_> = std::iter::once(cmd.get_program())
        .chain(cmd.get_args())
        .map(cstring)
        .collect::<Result<_, _>>()?;
    let mut env: std::collections::BTreeMap<_, _> = std::env::vars_os().collect();
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
    let env: Vec<_> = env
        .into_iter()
        .map(|(key, value)| {
            let mut pair = key;
            pair.push("=");
            pair.push(value);
            cstring(&pair)
        })
        .collect::<Result<_, _>>()?;
    let pointers = |strings: &[CString]| {
        strings
            .iter()
            .map(|s| s.as_ptr().cast_mut())
            .chain(std::iter::once(std::ptr::null_mut()))
            .collect::<Vec<_>>()
    };
    let argv = pointers(&argv);
    let env = pointers(&env);
    let check = |code| {
        if code == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(code))
        }
    };
    // SAFETY: `posix_spawnattr_t` 必须先由 `posix_spawnattr_init` 初始化才能
    // 使用，所以这里先 `zeroed()` 再立刻 init（init 会覆盖它）。其余调用都
    // 只操作这个已初始化的本地 `attrs`，错误码由 `check` 转成 `io::Error`。
    unsafe {
        let mut attrs = std::mem::zeroed();
        check(libc::posix_spawnattr_init(&mut attrs))?;
        let result = (|| {
            check(libc::posix_spawnattr_setflags(
                &mut attrs,
                libc::POSIX_SPAWN_SETEXEC as libc::c_short,
            ))?;
            check(disclaim(&mut attrs, 1))?;
            check(libc::posix_spawn(
                std::ptr::null_mut(),
                program.as_ptr(),
                std::ptr::null(),
                &attrs,
                argv.as_ptr(),
                env.as_ptr(),
            ))?;
            Err(io::Error::other("SETEXEC unexpectedly returned"))
        })();
        libc::posix_spawnattr_destroy(&mut attrs);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" fn refuse(_: *mut libc::posix_spawnattr_t, value: i32) -> i32 {
        assert_eq!(value, 1);
        libc::EPERM
    }

    #[test]
    fn disclaimer_failure_falls_back_to_plain_exec() {
        let error = exec_or_inherit(Command::new("/nonexistent/deepcore-disclaim"), Some(refuse))
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn c_arguments_reject_embedded_nuls() {
        assert_eq!(
            cstring(OsStr::from_bytes(b"bad\0arg")).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
    }
}
