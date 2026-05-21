#[cfg(target_os = "windows")]
fn main() {
    // 编译 Windows 资源文件（图标 + manifest）
    embed_resource::compile("windows/deepterm.rc", embed_resource::NONE);
}

#[cfg(not(target_os = "windows"))]
fn main() {}
