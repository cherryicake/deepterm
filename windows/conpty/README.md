# Windows Terminal ConPTY（随 deepterm 分发）

本目录的 `conpty.dll` 与 `OpenConsole.exe` 来自 Microsoft Windows Terminal，
以 MIT 协议分发（见同目录 [LICENSE](LICENSE)）。

## 为什么必须随包分发

Windows 自带的 conhost ConPTY（`CreatePseudoConsole`）在输出方向会**丢弃
APC 序列**（探测验证：含 `\x1b_G…\x1b\\` 的负载被整段吞掉，`CSI 16t` 正常）。
APC 是 Kitty 图形协议的载体，因此用系统 ConPTY 时 TUI 的图片永远无法显示。

Windows Terminal 的 `conpty.dll` + `OpenConsole.exe` 在输出**与输入**两个
方向都原样透传 APC（输入方向自 v1.25 起也支持，kitty 能力探测的应答
`\x1b_Gi=…;OK\x1b\\` 能完整送达子进程）。

`alacritty_terminal` 的 Windows PTY 后端会优先加载 exe 目录下的
`conpty.dll`（见其 `tty/windows/conpty.rs` 的 `LoadLibraryW("conpty.dll")`），
所以这两个文件必须放在 `deepterm.exe` 旁边：

- 开发运行：`build.rs` 构建时自动拷到 `target/<profile>/`（app）与
  `target/<profile>/deps/`（测试 exe）；
- 安装包：`windows/deepterm.iss` 的 `[Files]` 直接引用本目录。

`conpty.dll` 创建伪控制台时会拉起同目录的 `OpenConsole.exe`，两个文件
必须成对出现——只放 `conpty.dll` 会导致 PTY 创建失败（终端打不开）。

## 版本与更新方式

- 来源包：`Microsoft.Windows.Console.ConPTY` v1.25.260930003
  （对应 Windows Terminal v1.25.2733.0 的 release 资产）
- 下载：<https://github.com/microsoft/terminal/releases> → 对应版本的
  `Microsoft.Windows.Console.ConPTY.*.nupkg`
- nupkg 内路径：x64 的 `runtimes\win-x64\native\conpty.dll` 与
  `build\native\runtimes\x64\OpenConsole.exe`

更新时保持两个文件同版本。当前只收录 x64（与 `windows/deepterm.iss` 的
`x64compatible` 及 `script/bundle-windows.ps1` 的 `x86_64-pc-windows-msvc`
一致）；若将来发 arm64 包，从同一 nupkg 的 `win-arm64` / `arm64` 目录取，
并把 `build.rs` 的选择逻辑扩到 `target_arch = "aarch64"`。
