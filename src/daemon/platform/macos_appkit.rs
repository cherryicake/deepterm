//! Each holder keeps its coalition represented after its GUI children quit.

use std::ffi::{c_char, c_void};
use std::io;

pub fn check_in() -> io::Result<()> {
    let exe = std::env::current_exe()?.canonicalize()?;
    if !super::macos_bundle::source_is_bundle(&exe) {
        return Ok(());
    }
    // 打包漏掉 `DeepHolder.app` 时 holder 会退回 core 的二进制：功能正常，
    // 但活动监视器里显示成 `DeepTerm Core`。这条日志是排查“名字怎么又一样
    // 了”的唯一线索，必须留下。
    if !super::macos_bundle::holder_has_own_bundle(&exe) {
        log::warn!(
            "holder 运行在 {} 而不是 DeepHolder.app：活动监视器会把它显示成 core，\
             检查分发包里是否缺了 DeepHolder.app",
            exe.display()
        );
    }
    let Some(session) = super::macos_session::graphic_session()? else {
        return Ok(());
    };
    if !gui_session_usable(session) {
        return Err(io::Error::other(
            "GUI login unavailable; retaining headless holder startup",
        ));
    }
    // SAFETY: 本块只与 AppKit / Objective-C 运行时交互。每一步的前置条件
    // 都在用之前显式检查（dlopen/dlsym 的空指针、两个对象是否真的建出来），
    // 下面逐条说明。
    unsafe {
        let framework = libc::dlopen(
            c"/System/Library/Frameworks/AppKit.framework/AppKit".as_ptr(),
            libc::RTLD_LAZY | libc::RTLD_GLOBAL,
        );
        if framework.is_null() {
            return Err(io::Error::other("cannot load AppKit"));
        }
        // SAFETY: `dlsym` 只读取已加载的 framework；返回的指针在 dlclose 之前
        // 一直有效，而本进程不会 dlclose（它要活到退出）。
        let class = libc::dlsym(framework, c"objc_getClass".as_ptr());
        let selector = libc::dlsym(framework, c"sel_registerName".as_ptr());
        let send = libc::dlsym(framework, c"objc_msgSend".as_ptr());
        if class.is_null() || selector.is_null() || send.is_null() {
            return Err(io::Error::other("AppKit Objective-C runtime unavailable"));
        }
        // SAFETY: 这三个符号的真实类型就是下面声明的签名（Objective-C 运行时
        // 的公开 ABI）：`objc_getClass`/`sel_registerName` 取一个 C 字符串，
        // `objc_msgSend` 按 selector 的签名调用。`objc_msgSend` 分成几个
        // 不同签名的别名，是因为它的实际签名随被调 selector 变化——这里
        // 每个别名都只在下方以匹配的参数调用。
        let class: unsafe extern "C" fn(*const c_char) -> *mut c_void = std::mem::transmute(class);
        let selector: unsafe extern "C" fn(*const c_char) -> *mut c_void =
            std::mem::transmute(selector);
        let object: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
            std::mem::transmute(send);
        let policy: unsafe extern "C" fn(*mut c_void, *mut c_void, isize) -> i8 =
            std::mem::transmute(send);
        let finish: unsafe extern "C" fn(*mut c_void, *mut c_void) = std::mem::transmute(send);
        // SAFETY: 传进去的都是 NUL 结尾的 C 字符串字面量（`c"..."`），
        // 且这两个符号已在上面确认非空。
        let pool = object(
            class(c"NSAutoreleasePool".as_ptr()),
            selector(c"new".as_ptr()),
        );
        let app = object(
            class(c"NSApplication".as_ptr()),
            selector(c"sharedApplication".as_ptr()),
        );
        // 先判空再发消息：向 nil 发消息在 Objective-C 里是空操作而不是 UB，
        // 但那样 `policy`/`finishLaunching` 会被静默跳过，进程看起来"初始化过"
        // 其实没有——错的是次序，不是崩溃。这里显式失败更好。
        if app.is_null() {
            // `pool` 可能已经建出来了，先还回去再报错，别把它漏在这个
            // 提前返回上（autorelease pool 不 drain 就不会释放）。
            if !pool.is_null() {
                // SAFETY: `pool` 非空且是上面 `+new` 出来的 NSAutoreleasePool。
                finish(pool, selector(c"drain".as_ptr()));
            }
            return Err(io::Error::other(
                "cannot register holder as an accessory application",
            ));
        }
        // SAFETY: 三个对象/字符串均非空（上面的检查），selector 由运行时注册。
        // `setActivationPolicy:` 返回 BOOL（此处以 `i8` 接收），忽略返回值是
        // 有意的：进不了 accessory 模式只影响 Dock 表现，不该阻止 holder 启动。
        policy(app, selector(c"setActivationPolicy:".as_ptr()), 1);
        finish(app, selector(c"finishLaunching".as_ptr()));
        finish(pool, selector(c"drain".as_ptr()));
    }
    Ok(())
}

pub fn gui_session_usable(session: u32) -> bool {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    let Ok(mut child) = Command::new("/bin/launchctl")
        .args(["print", &format!("login/{session}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if start.elapsed() < Duration::from_secs(1) => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}
