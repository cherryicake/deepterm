#[cfg(target_os = "windows")]
fn main() {
    // 关键构建修复（中文）：声明 `rerun-if-changed`，改图标/manifest/rc 后
    // 能正确触发重编；否则增量构建会沿用旧资源，导致图标更新不生效。
    println!("cargo:rerun-if-changed=windows/deepterm.rc");
    println!("cargo:rerun-if-changed=windows/deepterm.manifest");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    // 编译 Windows 资源文件（图标 + manifest）
    embed_resource::compile("windows/deepterm.rc", embed_resource::NONE);
}

#[cfg(not(target_os = "windows"))]
fn main() {}
