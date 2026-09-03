# DeepTerm 全代码审查报告

审查范围：`src/` 全部 147 个 Rust 文件（约 78,000 行）。
审查时间：2026-08-28。
发现按 **安全 > 高 > 中 > 低/性能 > 结构** 排序，每条含文件:行号与修复建议。

---

## 修复进度（更新于 2026-08-29）

S1–S8 安全项与 H1–H8 高危项已全部修复，`cargo test` 429 passed / 0 failed。

| 编号 | 修复内容 | 涉及文件 |
|------|---------|---------|
| S1 | 密码改用 `sshpass -e`（SSHPASS 环境变量）传递，不再进 argv；新增 `redact_sensitive_args` 日志脱敏 | `ssh/ssh_pty.rs` |
| S2 | 新增 `keychain::write_secret_file`（创建即 0600 + 写后收紧），私钥落盘使用之 | `ssh/keychain.rs`、`sidebar_view/keychain_editor.rs` |
| S3 | shell integration 目录按 uid 隔离；写入前校验属主/权限，不可信则清除重建；目录 0700、脚本 0600 | `terminal/terminal.rs` |
| S4 | scp 的 `user@host:path` 参数两层转义（源机 shell + 目标机 shell），堵住用户名/主机名元字符注入 | `file_manager/cross_remote.rs` |
| S5 | 新增 `HostKeyPolicy`；known_hosts 检查出错由 fail-open 改为 **fail-closed**，未知主机在严格模式下拒绝；`strict_host_key_checking` 配置真正接入 russh/SFTP 两条通道 | `ssh/host_key.rs`、`ssh/russh_terminal.rs`、`ssh/sftp_fs.rs` |
| S6 | 新增 `upload_file_from_local_with_permissions`，SFTP open 时携带 0600 属性，消除"按 umask 创建 → 事后 chmod"窗口 | `ssh/sftp_fs.rs`、`file_manager/scp_key_lease.rs` |
| S7 | `sftp_cache.json` 改走 `write_secret_file`（0600） | `file_manager/file_tree_cache.rs` |
| S8 | 新增 `sanitize_pasted_text`：非 bracketed 模式剔除 ESC/C0/C1/DEL 并归一 CRLF，bracketed 模式剔除定界符；粘贴改为单次原子写入；拖放路径同样清洗 | `terminal/terminal.rs`、`terminal_view/terminal_view.rs` |
| H1 | 按"只保存主窗口"实现：`WindowManager` 维护主窗口槽位，非主窗口跳过写盘；主窗口关闭后由存活窗口接管 | `workspace/window_manager.rs`、`main.rs` |
| H2 | 新增 `normalize_sizes`，会话恢复与 resize 时修正 sizes/children 长度不一致，消除越界 panic | `workspace/split_manager.rs` |
| H3 | 拆分 `replace_text_in_range_inner(allow_auto_pair)`：undo/redo/粘贴/缩进等程序化插入绕过自动配对 | `components/input/state.rs` |
| H4 | IME 选区映射基准由 `range.end` 改为 `range.start` | `components/input/state.rs` |
| H5 | outdent 按**字符**裁剪缩进前缀，修复多字节空白（U+3000）切片 panic | `components/input/state.rs` |
| H6 | F1-F4 走 `encode_ss3_function_key`（无修饰 `ESC O P`，带修饰 `ESC [1;m P`）；顺带把 F5-F12 对齐 xterm 标准编码（原 16~/22~ 非标准） | `terminal/mappings/keys.rs` |
| H7 | 本地侧改为读字节 + `decode_bytes` + 二进制判定；新增 `DiffSideContent` 区分"文件不存在"与"二进制/无法解码"，后者给占位文本而非空串 | `editor/git_repository.rs` |
| H8 | `revert_hunk` 前校验 hunks 指纹，陈旧时用 HEAD 侧行号同步重算并重新定位，定位不到则放弃还原 | `editor/diff_view.rs` |

新增单元测试 20 个（键序列 6、粘贴清洗 6、`normalize_sizes` 5、日志脱敏 2、主机密钥策略 1）。

---

## 一、安全问题（建议立即修复）

| # | 位置 | 问题 | 建议 |
|---|------|------|------|
| S1 | `src/ssh/ssh_pty.rs:93` | **明文密码写入日志**：`log::info!("SSH 命令: {:?}")` 输出 ssh_args，密码认证时 argv 含明文密码（`:79` 重组为 `-p <密码>`），且密码进 argv 会被 `ps` 全局可见 | 日志脱敏；改用 `sshpass -e`（SSHPASS 环境变量） |
| S2 | `src/sidebar_view/keychain_editor.rs:532` | **私钥 0644 落盘**：`fs::write` 未设权限，同机其他用户可读，ssh 也会因权限拒绝 | `OpenOptions::mode(0o600)` |
| S3 | `src/terminal/terminal.rs:190-222` | shell integration 脚本写固定共享路径 `$(temp_dir)/deepterm-shell-integration`，多实例写竞态，其他用户可预建目录替换脚本 → 以当前用户身份执行任意 shell 代码 | 路径加 uid 隔离；写入前校验属主/权限 |
| S4 | `src/file_manager/cross_remote.rs:634-646, 920-933` | scp 远程命令中 `dst_host`（username@host）未经 shell 引用拼接，用户名含元字符即对源服务器命令注入（remote_path 已处理，dest_host 没有） | 对 `dest_host` 一并 `shell_single_quote` |
| S5 | `src/ssh/host_key.rs:26-44` | known_hosts 检查出错时 fail-open（`Ok(true)`）；未知主机静默 TOFU 无确认；`config.rs:361` 的 `strict_host_key_checking` 全仓库无消费方（摆设） | 环境错误 fail-closed；首次连接弹窗确认 |
| S6 | `cross_remote.rs:637,923` + `scp_key_lease.rs:319-371` | scp 直连用 `UserKnownHostsFile=/dev/null`（完全不校验主机密钥）；私钥上传到源端 /tmp 后、`chmod 600` 前按远端 umask 可读，崩溃后残留 60 分钟 | SFTP create 带 0600 属性再写；scp 走 known_hosts |
| S7 | `src/file_manager/file_tree_cache.rs:38-42,199-210` | 远程文件内容（≤512KB×200/主机）明文落盘 `sftp_cache.json`，0644 权限，可能缓存 .env/凭据 | 0600 权限（与 ssh.json 一致） |
| S8 | `src/terminal_view/terminal.rs:1799-1821` / `terminal_view.rs:2387-2396` | 普通模式粘贴未过滤 ESC 等控制序列（粘贴注入面）；bracketed paste 分 3 次 write 非原子 | 过滤控制序列；拼单次写入 |

---

## 二、高严重度 Bug

| # | 位置 | 问题 | 建议 |
|---|------|------|------|
| H1 | `src/main.rs:576-581,1330-1342` + `window_manager.rs:30` | **多窗口会话互相覆盖**：每个 AppWindow 各持 PersistenceManager，但 `session_state.json` 是全局单文件，后保存者覆盖先保存者 → 另一窗口全部标签页/SSH 会话丢失 | 按窗口 ID 分文件存储，或全局聚合后单点写入 |
| H2 | `src/workspace/split_manager.rs:399-400` | `resize_child` 读取用 `get().unwrap_or(1.0)` 但赋值直接 `sizes[child_index] = ...`；sizes 来自会话 JSON 反序列化，长度可不一致 → 拖动分割条 panic | 先校验长度，否则 return false |
| H3 | `src/components/input/state.rs:3769-3776` | **redo 与 auto-pair 冲突**：redo 回放单字符 `(`、`"` 插入时再次自动配对，重做结果变 `()` 而非 `(` | undo/redo 走绕过 auto-pair 的内部直写路径 |
| H4 | `src/components/input/state.rs:3886-3891` | IME `replace_and_mark_text_in_range` 选区映射写成 `new_range.end + range.end`，应为 `+ range.start`；组字第二帧起选区终点漂移 | 修正映射公式 |
| H5 | `src/components/input/state.rs:2057-2066` | outdent 分支 `prefix[..keep]`：行首为多字节空白（如 U+3000）时切片落在字符中间 → **panic (char boundary)** | 按字符裁剪 |
| H6 | `src/terminal/mappings/keys.rs:88-121` | F1-F4 键序错误：无修饰键输出 `ESC[OP`（应为 SS3 `ESC O P`），带修饰键输出 `ESC[1;2OP`（应为 `ESC[1;2P`），F1-F4 在 vim/htop 失效 | 对 "OP"-"OS" 后缀特判 |
| H7 | `src/editor/git_repository.rs:296,344` | `diff_file_contents` 本地侧 `read_to_string` 对 GBK/二进制文件 Err→`unwrap_or_default()`，new_text 变空 → diff 视图显示"整个文件被删除" | 走 `decode_bytes`，二进制显式占位 |
| H8 | `src/editor/diff_view.rs:867-913 + 368-383` | hunks 经 200ms 防抖重算；编辑后立刻"还原此块"，revert 用过期 `hunk.new_rows` 做字节级替换，可能删错行 | revert 前校验 new_text 指纹或同步重算 |

---

## 三、中严重度问题

### 可靠性 / 正确性
- `src/ssh/sftp_fs.rs:958-979` — `with_cached_session` 对**任意**错误都重连重跑闭包：用户取消（TRANSFER_CANCELLED_MSG）会触发整个传输重跑；rm/scp 类非幂等命令被二次执行。应区分连接错误与业务错误。
- `src/ssh/connection_manager.rs:200-257,362-369` — `do_connect` 是永远返回 Ok 的桩、断线监听为空 TODO，连接记录只增不删（内存泄漏+假状态）。建议删除或真正实现。
- `src/ssh/sftp_fs.rs:524-533,1574` — `read_file_bytes_pipelined` 一次性分配整个文件大小，仅 512KB 下限无上限，打开大文件会 OOM。加内存上限。
- `src/workspace/workspace.rs:1572-1579` — 拖拽 tab 时先 `tabs.remove(dragged_tab_id)` 再判断目标，提前 return 时 tab 实体已 drop（进程被杀）但 TabBar 未同步 → 幽灵标签。
- `src/terminal/terminal.rs:1907-1918` — resize 双重调整：直接 `term.resize` 后又发 `Msg::Resize`（EventLoop 再 resize 一次），有冗余与竞态。
- `src/terminal/terminal.rs:626-643`（Windows）— `Terminal::new` 在 UI 线程同步 `powershell .output()` 探测默认 shell，阻塞开窗数百 ms。
- `src/components/input/text_wrapper.rs:197-202` — 多行编辑重算 longest_row 只扫新行，历史最长行丢失，横向滚动宽度缩水。
- `src/components/input/wrap_map.rs:181-186` — `rebuild_cache` 仅比较行数与 wrap 总数，一增一减相抵时缓存漏更新，行映射错位。
- `src/components/input/state.rs:2257-2312` — move_line_up/down：CRLF 文件交换行块后残留孤立 `\r`；选区平移未计入补/删 `\n` 的字节差。
- `src/components/input/search.rs:562` — replace_next 用替换前坐标滚动，替换使后续偏移漂移。
- `src/builtin_commands/path_completion.rs:216-237` — `parse_line_column`：路径含 `:` 时非数字段被吞掉，返回截断路径；`3` 与 `_` 分支代码完全重复。
- `src/builtin_commands/scp_completion.rs:92-95` — 双引号内 `\<任意字符>` 一律吞反斜杠，`"C:\Users\x"` → `C:Usersx`，与 shell 语义不符。
- `src/builtin_commands/path_completion.rs:143` — `entry.file_type()` 不跟随符号链接，指向目录的 symlink 补全不追加 `/`。
- `src/editor/git_status.rs:307-329` — `parse_numstat` 未加 `-c core.quotePath=false`/`-z`，中文/空格文件名与 `-z` status 的 path 不一致，diff stat 匹配失败（+0/-0）。
- `src/editor/git_marks.rs:27-49` — `dedup_by` 按插入序保留先到的 Deleted，与注释声称的优先级相反。
- `src/sidebar_view/keychain_editor.rs:104-114` — 自绘 MultilineTextArea：`key.len()==1` 只接受 ASCII，无法输入 IME 文本。
- `src/main.rs:693-708` — 标签组只增不减，无任何删除路径；创建过的 Workspace（含全部终端进程/SSH 连接）被强引用至窗口关闭，资源单调增长。
- `src/main.rs:111` — 会话双文件均损坏时静默回退空会话，下次保存覆盖旧数据，无提示。
- `src/workspace/workspace.rs:454` — `next_tab_id` 恢复下界 `max(tabs.len())` 未解析现有 id 最大序号，可能产生重复 tab id 覆盖 HashMap 中的 TabInfo。
- `src/sidebar_view/keychain_editor.rs:782-806` — `sanitize_filename` 所有非法字符映射为 `-`，不同标签可碰撞互相覆盖密钥文件。
- `src/sidebar_view/host_form.rs:335,361-396` — 端口校验接受 `0`；每次保存内联密码/密钥都新建钥匙串条目，反复编辑产生重复凭据。

### 性能（每帧/每击键路径上的重活）
- `src/builtin_commands/path_resolver.rs:10-15`（被 `terminal_view.rs:1862,998` 调用）— macOS 下同步 spawn `lsof`（常 >100ms），**每敲一个字符调一次** + 每次 Wakeup 防抖也调，全部主线程 → 输入卡顿。同理 `terminal.rs:1156` 同步跑 `/bin/ps`。缓存 + 后台化。
- `src/components/input/element.rs:2374-2430` — indent guides **每帧**对全文件逐行 `slice_line→String` 分配并三遍 O(n) 扫描，无缓存。按 text_version 缓存。
- `src/components/input/element.rs:319-383,1097-1125` + `state.rs:2975-3067` — 光标布局/可视范围/滚动居中均为 O(总行数) 线性扫。应复用 WrapMap 前缀和做 O(log n)。
- `src/terminal/terminal.rs:1648-1766` — `update_last_content` 持 `FairMutex<Term>` 期间做全视口 cell 拷贝及逐行 URL 正则扫描，阻塞 PTY 事件循环线程。锁外扫描。
- `src/terminal_view/terminal_view.rs:1460-1496` — 拖拽选择时 selection_cols 每帧全量重算 + flattened cache 全量重建（O(rows×cols)/帧）。只重算新旧光标行之间的行。
- `src/terminal_view/terminal_view.rs:1883-1891` — 输入 `/scp` 期间每个按键同步 `SshGlobalConfig::load()`（磁盘 IO）。
- `src/editor/diff_view.rs:362,396` — 每次按键 `value().to_string()` 全文深拷贝判 dirty；editor.rs:1911 已用 text_version 闸门，此处未复用。
- `src/editor/diff_view.rs:751` — CRLF 文件行尾含 `\r` 导致高亮批处理每行断组，每次触发 `SimpleHighlighter::styles()` 都 `tree.clone()` 整树（simple.rs:283）→ O(行数×树克隆)。
- `src/file_manager/watcher.rs:476-483` — RemoteFsWatcher 每 3 秒对被监视文件全量下载后哈希。改 size+mtime 判重。
- `src/builtin_commands/mod.rs:1062-1064` — 本地路径补全在 UI 线程同步 `read_dir`，大目录卡帧。
- `src/sidebar_view/host_view.rs:230`、`keychain_view.rs:169` — **render() 内同步读盘/读钥匙串**，搜索框每次击键都重读。缓存为 entity 字段。

---

## 四、低严重度 / 债务

- `src/terminal/terminal.rs:1303-1311` — OSC 残尾只缓存一次，跨 3+ chunk 的 OSC 被透传 vte 丢失拦截；`extract_osc_sequence`(:1533) BEL 优先于 ST 会截错序列。
- `src/terminal/terminal.rs:570` — `event_loop.spawn()` 失败被静默吞掉，终端永久黑屏无提示。
- `src/terminal/terminal.rs:1907-1918` — 每个 Wakeup 单独 content_generation+=1，高输出时事件风暴，可合并。
- `src/terminal/mappings/mod.rs:90` — `first.as_bytes()[0]` 空 name 时 panic（当前不可达，属隐患）。
- `src/components/input/element.rs:2703-2717` — paint 末尾无条件 `cx.notify()`，有持续多余渲染循环风险。
- `src/components/input/element.rs:1428` — `expect("line should exists in wrapper")`，display_map 失步时 panic（旁有防御日志说明失步发生过）。
- `src/components/input/movement.rs:162,178` — `previous_boundary(x-1)` 双重回退，shift+up 少选一个位置。
- `src/components/input/state.rs:2319-2343` — `duplicate_line` 注释称保留原选区但未恢复。
- `src/components/input/rope_ext.rs:324-328` — `slice_lines(n..n)` 空区间构造 start>end range，Rope::slice 会 panic（现无调用点，建议防御）。
- `src/editor/git_repository.rs:303-306` — `thread::scope` 中 `join().unwrap()`，子线程 panic 击穿 UI 线程。
- `src/editor/editor.rs:1907` — `editor_state_subscriptions` 只增不清，随开/关 tab 无界增长。
- `src/editor/registry.rs:166` — `FontStyle::Underline => Normal`，下划线样式静默丢失。
- `src/workspace/persistence.rs:96-110` — atomic_write 无 fsync（有 .bak 兜底）；`:101` 临时文件名只含 pid，多窗口并发写竞争同一 tmp 路径（与 H1 一并解决）。
- `src/theme/mod.rs:605-616` — 主题偏好 `fs::write` 直写，与原子写标准不一致。
- `src/workspace/tab_bar.rs:23-35` — `Rc<dyn Fn + Send + Sync>`：Rc 非 Send，约束冗余误导。
- `src/workspace/panel.rs:379-393` — `with_ssh_terminal` 未复用 `new_common`，字段手抄易漏。
- `src/sidebar_view/git_panel.rs:476,503` — stage/unstage 失败完全静默无反馈；`:205-237` watcher 不校验 event.path 是否属于当前 workspace。
- `src/sidebar_view/file_tree.rs:92-98,135` — 本地 home 解析失败时 root 落到 `/` 并递归监听整个文件系统。
- `src/builtin_commands/path_completion.rs:180-189` — `unescape_path` 会破坏字面 `\ ` 路径；`:123-126` read_dir 权限错误静默返回空。
- `src/ssh/sftp_fs.rs:361`、`file_manager/utils.rs:136-140` — `.deepterm-part` 固定临时名，并发传输互相覆盖。
- `src/file_manager/manager.rs:213` — `h.join().unwrap()`，工作线程 panic 传染整个传输任务。
- `src/ssh/russh_terminal.rs:203`、`sftp_fs.rs:659-695` — 队列/输出缓冲无限累积（限条数不限字节）。
- `src/main.rs:623` — 注释混入俄语 "удерживать"；`window_manager.rs:25-26` 重复 doc 注释。

---

## 五、结构优化建议

### 上帝对象拆分
| 文件 | 行数 | 问题与建议 |
|------|------|-----------|
| `components/input/state.rs` | 4370 | 混装编辑原语/编辑命令/视图缓存/搜索集成 60+ 字段。拆为 `state/core`、`commands_*`（行操作、注释、缩进）、`view_state.rs` |
| `components/input/element.rs` | 2850 | 拆为 `layout`（cursor/selection/visible_range）、`decorations`（indent/rainbow/occurrence）、`paint` |
| `terminal/terminal.rs` | 2718 | PTY/SSH/OSC/选区/滚动/内建命令/广播/IME 混在一个 impl。prepaint 约 600 行、render 约 390 行需拆长函数 |
| `terminal_view/terminal_view.rs` | 3033 | 同上；六段重复的滚动方法可提取 |
| `workspace/workspace.rs` | 2632 | tab 生命周期+分割布局+resize+广播分发+文件传输+会话序列化。至少拆出 `TransferCoordinator`（900-1198 行传输逻辑与 3 个 detached 任务）与 `BroadcastHub` |
| `editor/editor.rs` | 3131 | tab 管理/文件加载/保存/markdown/image/diff 分发/动画/状态栏纠缠。按"tab 会话管理 / 文件 IO 与重载 / 渲染"拆 3 个子模块 |
| `builtin_commands/mod.rs` | 3012 | 装了 4 件事：状态机(~1230)+浮层 UI(~740)+缓冲区编辑器(~230)+测试(~1140)。拆为 `manager.rs`、`state.rs`、`overlay_ui.rs`，测试移出 |
| `main.rs` | 1358 | `AppWindow::new` 260 行内联完成会话加载+9 个实体+十余组回调接线，无法单测。抽 `AppWindowBuilder` |

### 双轨/重复实现（收敛到一处）
1. **命令检测两套状态机**：`command_model.rs::BuiltinCommandModel::detect_command` 与 `mod.rs::detect_entry_phase` 并行（且 `ArgumentType::None` 语义有差异）；`argument.rs::tokenize` 只支持双引号，与 `scp_completion::shell_split_tokens`（完整 shell 分词）不一致 → 统一到后者。
2. **SSH 连接三套并行逻辑**：`SshConnectionManager`（基本是死代码）、`sftp_fs` 会话池、`RusshTerminal` 自建连接，认证代码复制两份 → 收敛为统一 ConnectionProvider。
3. **sidebar_view 模板复制**：`host_view.rs` 与 `keychain_view.rs` 的 header/搜索框/Modal 接线/列表行渲染几乎逐行相同（各 ~250 行），theme_view 再复制一份 → 提取 `SidebarHeader`/`SidebarListRow`/modal-wiring 组件。`hosts_view.rs:17` 的 `HostType::Ssh` 单变体枚举是无效抽象。
4. **vendored 上游代码**：`rope_ext.rs`、`selection.rs`、`history.rs`、`search.rs`（自注 Ported from gpui-component）与 Zed/gpui-component 上游重复，`fold_map` 双层投影架构与 Zed DisplayMap 同构 → 抽公共 crate 或明确标记 vendored。
5. **文件读写重复**：editor.rs 的 `load_local_text` 与 git_repository 的 `read_file` 各写一套，DiffView 与 Editor 各自实现 dirty/saving/load 防护 → 抽公共"文件缓冲区状态"类型。

### 死代码清理（grep 证实无调用方）
- `workspace/template.rs` 全文（TemplateManager）
- `workspace/persistence.rs:134,248-287,305-346` — `WorkspaceState`、`save_state`、`AutoSaveManager`、`export/import_config`、`state_exists/clear_state`
- `editor/diff_model.rs:26-101,591-723` — `DiffFile::build`/`DiffRow`/`rows` 整套 collapsed 旧路径（~500 行，仅测试引用），且与新 collapsed builder 阈值不一致
- `editor/diff_view.rs:141` `DiffView::new`、`git_repository.rs:247` `diff_file`（伪 diff 头 `@@ -0,0 +1 @@` 行数也是错的）
- `ssh/connection_manager.rs` — do_connect 桩 + 空断线监听

---

## 六、总体评价

**亮点**：damage-based 脏行追踪与行级渲染缓存、持久化原子写+备份+回退、keychain 凭据设计（系统 Keychain + 明文落盘阻断 + 0600）、diff/编码/引号处理等高风险纯函数测试覆盖扎实、注释详实多记录踩坑原因。theme/mod.rs 是全项目质量最好的文件。

**三大系统性风险**：
1. **安全边界**：ssh_pty 明文密码日志 + scp 直连不校验主机密钥 + 私钥/缓存 0644 落盘 —— 建议最优先处理。
2. **同步 IO 进入了按键/渲染路径**：lsof、ps、SshGlobalConfig::load、render 内读盘、UI 线程 read_dir，这是输入卡顿的直接来源。
3. **持久化"全局单文件 × 多窗口"模型从根上不成立**（H1），需按窗口分文件或聚合写入。

**建议修复顺序**：S1-S8 安全项 → H1-H8 高危 bug → 每帧/每击键性能热点 → 上帝对象拆分与死代码清理。
