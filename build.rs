#[cfg(target_os = "windows")]
fn main() {
    // 关键构建修复（中文）：声明 `rerun-if-changed`，改图标/manifest/rc 后
    // 能正确触发重编；否则增量构建会沿用旧资源，导致图标更新不生效。
    println!("cargo:rerun-if-changed=windows/deepterm.rc");
    println!("cargo:rerun-if-changed=windows/deepterm.manifest");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    // 编译 Windows 资源文件（图标 + manifest）
    embed_resource::compile("windows/deepterm.rc", embed_resource::NONE);

    copy_conpty_runtime();
}

/// 把 `windows/conpty/` 的 Windows Terminal ConPTY 拷到可执行文件旁。
///
/// 系统 conhost 的 ConPTY 在输出方向丢弃 APC（Kitty 图形协议的载体），
/// 必须让 alacritty 的 Windows PTY 后端从 exe 目录加载随包的 `conpty.dll`
/// （加载逻辑见 `alacritty_terminal::tty::windows::conpty`，背景见
/// `windows/conpty/README.md`）。
///
/// 目标目录由 `OUT_DIR`（`target/<profile>/build/<pkg>-<hash>/out`）向上三级
/// 得到 `target/<profile>/`：
/// - app 的可执行文件直接在 `target/<profile>/`（`cargo run` / `cargo build`）；
/// - 测试可执行文件在 `target/<profile>/deps/`，也拷一份，`local_pty` 的
///   Windows e2e 测试才能加载到它。
///
/// 只在内容有差异时写入：`build.rs` 的产物会触发依赖方重编，不做无谓触碰。
/// 拷贝失败只警告不失败——终端仍能打开（退化为系统 ConPTY，图片不显示），
/// 不该因为一个可选优化把整个构建卡死。
#[cfg(target_os = "windows")]
fn copy_conpty_runtime() {
    use std::{env, path::PathBuf};

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let source_dir = manifest_dir.join("windows").join("conpty");
    println!("cargo:rerun-if-changed=windows/conpty/conpty.dll");
    println!("cargo:rerun-if-changed=windows/conpty/OpenConsole.exe");

    let Ok(out_dir) = env::var("OUT_DIR").map(PathBuf::from) else {
        return;
    };
    // OUT_DIR = target/<profile>/build/<pkg>-<hash>/out → 上三级是 <profile>。
    let Some(profile_dir) = out_dir.ancestors().nth(3) else {
        return;
    };

    for name in ["conpty.dll", "OpenConsole.exe"] {
        let source = source_dir.join(name);
        if !source.is_file() {
            // 目录被整体删掉时不报错：属"没随包分发"，不是构建缺陷。
            continue;
        }
        for dir in [profile_dir.to_path_buf(), profile_dir.join("deps")] {
            let dest = dir.join(name);
            if let Err(err) = copy_if_changed(&source, &dest) {
                println!(
                    "cargo:warning=拷贝 {} 到 {} 失败：{err}（终端仍可用，但 TUI 图片将无法显示）",
                    source.display(),
                    dest.display()
                );
            }
        }
    }
}

/// 内容相同则跳过（避免触碰时间戳），不同则覆盖。
#[cfg(target_os = "windows")]
fn copy_if_changed(source: &std::path::Path, dest: &std::path::Path) -> std::io::Result<()> {
    use std::fs;

    if dest.is_file() {
        let same = match (fs::read(source), fs::read(dest)) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        };
        if same {
            return Ok(());
        }
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(source, dest)?;
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn main() {}
