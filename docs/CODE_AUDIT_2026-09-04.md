# DeepTerm 全项目代码审计（2026-09-04）

范围：全部 11 个模块、约 8.6 万行 Rust 源码，由 7 路并行深度审查汇总而成。所有行号经现场核对；结论基于实际读取的代码上下文，宁缺毋滥。

分級说明：🔴 严重（崩溃/数据丢失/安全）/ 🟠 中等（功能缺陷或明显性能问题）/ 🟡 轻微（可维护性）/ 🔵 结构建议。

---

## 一、安全类

### 🔴 A1. 远端 shell 集成使用可预测 /tmp 路径，构成远程任意代码执行向量
`src/ssh/mod.rs:58-59,127-132`：bootstrap 脚本以 `${TMPDIR:-/tmp}/deepterm-shell-integration.$$`（仅 PID）建目录并 `export ZDOTDIR`。/tmp 为 1777，远端任意本地用户可预创建该目录并放入恶意 `.zshrc`（脚本只覆写 `.zshenv` 与 `deepterm-integration`），交互式 zsh 启动即以登录用户身份执行攻击代码。PID 可通过反复连接枚举。
**修复**：改用 `mktemp -d "${TMPDIR:-/tmp}/deepterm-shell-integration.XXXXXX" || exit 1`，失败中止而非 `2>/dev/null` 吞掉。

### 🔴 A2. zeroize 声明与实现不符：SSH 凭据全程普通 String，从未清零
`Cargo.toml:75` 注释承诺口令/私钥内存清零，但 `grep -rn zeroize src/ssh src/ssh_view` 为零命中。密码流转路径全为普通 String：`src/ssh/keychain.rs:570-573`（get_password_secret）、`keychain.rs:163`（ResolvedAuthMethod::Password 派生 Clone+Debug）、`russh_terminal.rs:324-328`、`ssh_pty.rs:115`（还 clone 进 env）、`agent.rs:72-73`（私钥字节普通 Vec）；对比 `file_manager/scp_key_lease.rs:311` 已用 Zeroizing，说明意识存在但 ssh 模块未落地。任何一处 `log::debug!("{:?}", config)` 即泄明文。
**修复**：`Zeroizing<String>` 贯穿 ResolvedAuthMethod/passphrase/get_*_secret；手写脱敏 Debug；agent.rs 对齐 scp_key_lease。

### 🔴 A3. 远程↔远程 scp 直连以 /dev/null 为 known_hosts，主机密钥验证完全失效
`src/file_manager/cross_remote.rs:644-645, 936-937`：`-o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=/dev/null`——每次执行都"首次连接"，accept-new 的 TOFU 语义为零，MITM 与已变更密钥均不可检测；`strict_host_key_checking=true`（host_key.rs 的 Strict 策略）对这条最重要的数据传输通道完全无效。`:915-918` 注释自认 TOFU 但实际更糟。
**修复**：在源服务器维护 per-app known_hosts（如 `~/.ssh/deepterm_known_hosts`），并把全局 HostKeyPolicy 映射进该选项。

### 🔴 A4. SFTP 原子写失败路径先删原文件，二次失败即永久丢数据
`src/ssh/sftp_fs.rs:669-680`：rename 失败 → 无条件 `remove_file(path)` → 再 rename 失败 → 连 tmp 也删。rename 失败的常见原因（断连/权限）大概率使第二次也失败，此时原文件与新内容全部丢失。
**修复**：rename 失败绝不删原文件；保留 tmp 并在错误信息中给出路径；覆盖语义走 `posix-rename@openssh.com` 扩展。

### 🔴 A5. 远端文件内容缓存把明文内容持久化到磁盘（含 hover 预取）
`src/file_manager/file_tree_cache.rs:269-275` + `sidebar_view/file_tree.rs:640-760`：鼠标悬停 80ms 即预取前 128KB、目录加载自动预取前 3 个小文件，全部进入持久化缓存（每服务器 200 个 × 512KB）落到 `sftp_cache.json`（0600 已做，但范围错误）。划过服务器上的 .env/私钥即明文落盘，且磁盘无过期机制（`load_from_disk` 恢复后 `loaded_at` 重置为"新鲜"）。
**修复**：`insert_file_content` 增加 `persist: bool`，仅显式打开过的文件落盘；磁盘缓存加时间戳与过期清理。

### 🔴 A6. TOFU 学习失败仍放行 + known_hosts 不可读被 russh 吞成"未知主机"
`src/ssh/host_key.rs:69-80`：`learn_known_hosts` 失败仅 warn 后仍 `Ok(true)`；russh `known_host_keys_path` 在文件不可读时返回空 vec。known_hosts 被移走/损坏后，accept-new 退化为"永远信任任意密钥"。
**修复**：学习失败应拒绝连接或强提醒 UI；连接前显式检查 known_hosts 可读写。

### 🟠 A7. host_form 编辑主机的钥匙串副作用（编辑即改端口也会重建密钥条目）
`src/sidebar_view/host_form.rs:430-445, 494-498`：编辑模式且 `selected_keychain_id` 缺失（如钥匙串项已删）时，`build_auth_method` 只要 key_path 非空就 `create_key` 新建条目并覆盖原 `auth_method`。
**修复**：编辑模式保留原 auth_method；create_key 仅在显式输入新凭据时触发。

### 🟠 A8. 其余安全中等问题
- `src/ssh/sftp_fs.rs:955-957`：明文密码长度编入缓存键（内存元信息泄露）。
- `src/ssh/config.rs:881-892`/`keychain.rs:477-487`：tmp/bak 在 chmod 前以 umask 默认权限短暂存在；`.bak` 从未收紧权限。
- `src/ssh/russh_terminal.rs:259-261`：`inactivity_timeout(60s)` 早于 keepalive 兜底（90s），后者不可达。
- `src/ssh/keychain.rs:225-234`：keychain.json 损坏静默返回空库，后续 save 会以空库覆盖，配置侧引用全部失效。
- `src/ssh/ssh_pty.rs:96`：48KB bootstrap 脚本整体进 argv，ps 可见且逼近 MAX_ARG_STRLEN 四成；建议 stdin 传递。
- `src/builtin_commands/mod.rs:917-935`：通用命令补全不重新引号化，靠下游 `trim_matches('"')` 意外正确（隐式耦合）；建议统一走 shell_quote。
- 亮点（维持）：`config.rs:859-871` 明文凭据阻断落盘、`keychain.rs:496-517` 0600 创建、`safe_open.rs` scheme 白名单、`utils.rs:108-127` 路径遍历防御、`cross_remote.rs` 双层 shell 转义。

---

## 二、UI 线程阻塞类（每帧/每键/启动路径的同步 IO 与 fork）

### 🔴 B1. 侧边栏每帧多次同步读磁盘 JSON
`src/sidebar_view/host_view.rs:1030,1031,1262`：单次 render 至少 3 次 `SshGlobalConfig::load()`（std::fs 读 + JSON 解析），`load_grouped_hosts` 内部再读 keychain.json；`keychain_view.rs:168` 同理。搜索输入、hover、动画帧都会触发。
**修复**：host/keychain 数据进 gpui Global 缓存 + 变更事件失效。

### 🔴 B2. 每键路径上的同步读盘/fork
- `src/terminal_view/terminal_view.rs:1958-1963`：`/scp` 每个字符触发 `SshGlobalConfig::load()`（TTL 5s 到期即读盘），UI 线程。
- `src/builtin_commands/path_completion.rs:130-185` + `mod.rs:1081-1110`：本地路径补全每键同步 `read_dir` + 逐项 stat（万级目录如 /usr/lib 逐字符输入 = 每键一次全量列举）。远端分支已是异步+LRU+防抖，本地应对齐。
- `src/builtin_commands/history.rs:155-159`：回车即同步全量序列化写盘（量小但与 file_tree_cache 的防抖后台写不一致）。
- `src/terminal_view/terminal_view.rs:1730-1733`：`execute_builtin_command` 全程持有全局 executor Mutex，命令可弹窗/异步，阻塞其它面板的 `/` 提交。

### 🔴 B3. 启动路径同步 IO/fork 聚集
- `src/ssh_view/picker.rs:145-188,207-244`：`SshPicker::new` 同步 fork `which` ×3（Windows 上 `wsl --list` 数百 ms）+ 读 ssh.json + 可能触发 keychain 迁移（触碰系统钥匙串）。
- `src/main.rs:122`→`persistence.rs:94-105`：`PersistenceManager::new` 同步 create_dir_all + 可能整目录递归迁移；theme::init / FontState::init 同步读配置。
- `src/terminal/terminal.rs:1543-1552`：`cached_hostname()` 同步 fork `/bin/hostname`（OSC7 host 校验路径）。
**修复**：统一移 background_executor / LazyLock 预取；hostname 改 libc::gethostname。

### 🔴 B4. macOS 文件选择对话框同步阻塞整个 UI
`src/sidebar_view/keychain_editor.rs:374-401, 835-838`：`import_key_from_file` 同步 `osascript ... .output()`，对话框期间界面假死；非 macOS 分支已正确用 `cx.prompt_for_paths`。
**修复**：macOS 分支对齐 `cx.prompt_for_paths`。

### 🔴 B5. markdown render 路径同步读本地图片（最大 20MB）
`src/editor/markdown_element.rs:1241-1243`（TODO 自认）：有扩展名/大小/`..` 三重防护（1235-1252），但同步 read 在 UI 线程，慢盘单帧卡数百 ms。
**修复**：`cx.spawn` + background_executor + Arc 缓存（作者已规划，应提优先级）。

---

## 三、正确性类

### 🔴 C1. 符号链接语义不一致（列表/传输/删除三处口径分裂）
`src/file_manager/types.rs:22-42`：`from_dir_entry` 用 `entry.file_type()`（不跟随，正确判类型）但 `entry.metadata()` 取 size（**跟随链接**）——悬空链接 size 恒 0 且吞掉 Err，断链与真空文件不可区分；与 `utils.rs:219-229` 删除路径刻意用 `symlink_metadata` 的口径冲突。`upload_download.rs:27-29`/`local_ops.rs:24-25` 顶层链接按目标内容复制，而目录内层链接计入 skipped——同一传输两种行为。
**修复**：统一 symlink_metadata；顶层链接显式策略（跳过/dereference）。

### 🔴 C2. scp 密钥租约 check-then-insert 竞态，并发传输互相销毁对方密钥
`src/file_manager/scp_key_lease.rs:207-233, 247-250`：get 与 insert 之间无原子性，两个 worker 同 key 并发未命中时各自 create_lease，后插者覆盖前者；先者 guard drop 会按 key 摘除并 `rm -rf` **对方正在使用的**远端密钥目录，后者目录与本地临时密钥永久残留。`manager.rs:185-220` 的 4 并发 worker 走 `copy_via_remote_scp` 是真实触发路径。
**修复**：临界区内 `entry().or_insert_with` 或 double-checked；插入撞 key 时清理重复目录。

### 🔴 C3. 其余严重正确性问题
- `src/terminal_view/box_drawing.rs:85`：U+2547 编码错误——表中 `enc(HEAVY,HEAVY,HEAVY,THIN)` 多画左右两条粗竖线，正确为 `enc(NONE,NONE,HEAVY,THIN)`；建议补全表与 Unicode 名称对照的单元测试。
- `src/terminal/terminal.rs:536-538`：注释承诺本地 PTY 走 OSC7/PwdChanged，但 alacritty_terminal 0.26 的 Event 枚举无此变体，本地数据流从不经过 `try_handle_osc_sequence`——本地 OSC 7/133/1337 全失效，仅靠 tcgetpgrp+lsof 回退。
- `src/terminal_view/terminal_view.rs:869-896`：SSH 重连替换终端后未清 `row_render_cache`/`flattened_render_cache`，新终端首帧 dirty_rows 为空可能复用旧终端文本缓存闪现旧内容；也未显式 kill 旧终端。
- `src/editor/goto_line.rs:110-121`：用 `line_end_offset`（保留 `\r`）遍历列偏移，CRLF 文件跳转列号错 1 字节；应改 `line_content_end_offset`（rope_ext.rs:200 已有）。
- `src/editor/markdown.rs:271-310, 378-399`：markdown 全选/选择依赖懒注册的 `fragment_texts`，视口外 item 未 build 即静默丢内容——cmd-a 复制不完整且用户无感知。全选应从 `item_ranges` 字节范围切片。
- `src/ssh/config.rs:352-354` + `host_form.rs:427,443,536`：`AuthMethod::Auto` 带 `#[serde(skip)]`，密码/密钥留空保存主机必然 serde 报错（英文错误用户不可理解）。
- `src/ssh/mod.rs:251-265`：`parse_ssh_target` 对 IPv6 解析错误（`::1` 被切成 host=":" port=1；`[::1]:22` 带方括号传入 russh）。
- `src/ssh/config.rs:123-125`：`SshConnectionConfig.timeout`（默认 30s）从未生效——三处 connect 均无 tokio::time::timeout 包裹，不可达主机要等 OS 级 75-130s。
- `src/workspace/panel.rs:912, 985` + `panel.rs:621-623`：拖拽落在面板中心 35%-65% 区时预览为 None（无任何提示），松手却以默认 Right 分屏——预览与行为脱节。drop 应吞掉中心区事件。
- `src/ssh/russh_terminal.rs:111-113, 178`：连接线程 spawn 失败 `.ok()` 吞掉，UI 永久卡"连接中"。

### 🟠 C4. 正确性中等问题（择要）
- `src/terminal/terminal.rs:1886-1888`：程序回写（ClipboardLoad/PtyWrite/TextAreaSizeRequest）走 `write`，含 `\r\n` 时误清输入锚点，导致 `/` 前缀误拦截；应提供 `write_raw`。
- `src/file_manager/local_ops.rs:99-141`：move 跨设备回退 copy 时对同一目标二次 `resolve_local_conflict`，目录递归逐条目再弹窗；远端 `remote_ops.rs:140-152` 同病。
- `src/file_manager/manager.rs:242-251` + `workspace.rs:1247-1254`：多源传输部分失败时 summary（已成功的 N 个文件）被整体丢弃，UI 只显示"失败"。
- `src/file_manager/cross_remote.rs:288-291, 398`：restore 备份 `?` 吞掉真实失败原因；取消+restore 失败时被归类为 Failed 而非 Cancelled。
- `src/ssh/sftp_fs.rs:423-427`：`copy_remote_file` 固定 `.deepterm-part` 后缀，并发同目标互踩（对比 write_file_bytes_atomic 645-653 已用 pid+随机）。
- `src/ssh/russh_terminal.rs:363-411`：Auto 认证不尝试 ssh-agent（auth.rs:45-113 的 try_agent_auth 已实现完整回退链却未被调用）。
- `src/ssh/sftp_fs.rs:995-1035`：SFTP ping 保活近乎死代码（pop 有会话即返回，ping 分支几乎不可达），且 ping 持 session 锁做阻塞网络 IO（其他线程可被阻塞 300s）。
- `src/ssh/russh_terminal.rs:426-504`：io_loop 退出后 ConnectionState 不更新，`is_ready()` 恒 true、输入静默丢失。
- `src/sidebar_view/file_tree.rs:1227-1264`：粘性目录锚点只在可见区起点恰为折叠边界时生效，快速滚动后粘性标题消失/错层；应改为向上回溯未闭合祖先。
- `src/sidebar_view/git_panel.rs:545, 725-737`：hover 用 entries 绝对下标，与 uniform_list 相对 enumerate 混用，滚动后高亮/预取作用到错误文件（应改 path 为 key）。
- `src/components/input/display_map/wrap_map.rs:181-205`：缓存跳过判据只看行数与 wrap 行数，"行数不变但排列变化"时前缀和过期，光标定位偶发错行。
- `src/components/input/state.rs:4104-4125`：折叠占位符右侧大片区域点击，光标跳文档末尾而非折叠头行尾；IME 定位同链路漂移。
- `src/workspace/session_state.rs:7` + `persistence.rs:180-195`：version 字段读出后从不校验，无降级闸门；`persistence.rs:123` `.bak` 复制失败被 `let _ =` 吞，存在"主文件坏+备份丢"窗口。
- `src/editor/encoding.rs:83-98`：UTF-16 文件先过 `is_binary_bytes`（大量 0x00），存在被误判二进制打不开的风险边界；lossy 兜底读入 + 按原编码写回会把 U+FFFD 固化。
- `src/editor/markdown_element.rs:1079-1084`：fragment_id 用 `(item_ix << 20) | counter` 位打包，32 位平台溢出、64 位下隐性契约无断言。
- `src/builtin_commands/executor.rs:425-444`：含冒号文件名（`myhost:2024-06-01.tar.gz`）兜底切分误匹配已存主机。

---

## 四、性能类

### 🟠 P1. 大文件编辑的全量路径
- `src/components/input/display_map/text_wrapper.rs:205-215`：任何换行编辑从 start_row 重排到文档末尾——10 万行文件第 10 行敲回车 = 全文档重排；`soft_lines`（273 行）也是全量重算。应 splice 受影响行 + 前缀和缓存。
- `src/components/input/state.rs:380`：Unicode 同词高亮（CJK 常见）每次光标落词整文档 `to_string()`。
- `src/components/input/element.rs:358-419, 1208-1227`：`layout_cursor` 与 `calculate_visible_range` 每帧从第 0 行线性扫到光标/视口行，而 `display_row_for_buffer_line` 已是 O(log n) 却未用。
- `src/components/input/display_map/fold_map.rs:102-107`：fold 去重 O(folds²)，`fold_all` 万行文档卡顿；已排序应二分。
- `src/components/input/element.rs:2400-2431`：prepaint 每帧写实体 `last_layout` 等；单行输入每个字符全 rope→String→shape（1639 行），应按 text_version 缓存。

### 🟠 P2. diff / markdown 的字符串快照与重复编译
- `src/editor/diff_view.rs:330-334, 364-365`：diff 编辑每次按键两次 O(n) 全量拷贝 + O(n) 比较（`InputState::value()` 即 rope 全遍历），未接入 editor.rs:1974-1991 已有的 text_version O(1) 闸门；`Rope::from_str` 每次 recompute 无条件重建（398-404）。
- `src/editor/editor.rs:1996-2017` + `markdown.rs:250-263`：编辑 tab 每字符向预览 tab 全量 set_text→全量 pulldown_cmark 重解析+清空高亮缓存，无防抖。
- `src/editor/syntax/simple.rs:79-218, 343-388` + `markdown.rs:580-581`：tree-sitter Query 编译（毫秒级）无缓存——markdown 每个代码块首次渲染一次，注入层每帧每 layer 一次；应把 `Arc<Query>` 缓存进 LanguageConfig。
- `src/editor/diff_model.rs:514-523`：同一次任务里 `build_unified_collapsed` 与 `merged_highlight_runs` 各自重算 line_starts 两遍。

### 🟠 P3. 终端渲染与输入
- `src/terminal_view/terminal_element.rs:936-960`：`try_update_runs_in_range` 扩展循环 O(runs²)/脏行/帧——彩色进度条/语法高亮输出 CPU 放大；应排序后线性扫描。
- `src/terminal/terminal.rs:2270-2288`：MOUSE_MOTION 模式每次移动事件都发 SGR 报文，未按网格坐标去重（alacritty 按 cell 去重）。
- `src/terminal_view/terminal_view.rs:2732-2751`：render 内推进动画并 `cx.notify()` 形成帧循环；应移独立任务。
- `src/terminal_view/terminal_view.rs:2538-2567`：每键 terminal.update 两次+可能第三次，input_bytes 再克隆一份。

### 🟠 P4. 传输与远端
- `src/file_manager/watcher.rs:478-499`：远端被 watch 的单个文件每 3 秒 SFTP 全量读取+哈希——打开 100MB 远端日志 = 每 3 秒拉 100MB；应先比对 size+mtime。
- `src/file_manager/utils.rs:373-398` + `cross_remote.rs:142-163`：AutoRename 线性探测 1..10000，每候选一次 SFTP 往返；应 read_dir 建 HashSet 一次定位。
- `src/file_manager/cross_remote.rs:839-883`：目录 scp 完成后为填 summary 再全树遍历两遍，"完成"反馈拖慢数秒。
- `src/file_manager/cross_remote.rs:798-837`：`monitor_dest_progress` 每条传输独立 connect 一条 SSH 连接，scp 24h 超时期间整周期持有；会话报错不退出静默空转。
- `src/ssh/sftp_fs.rs:781-817`：`execute_command` 输出无上限累积（一条失控命令耗尽内存）；`:1725` `read_file_bytes_pipelined` 按 size 全量预分配，公共 API 层无上限（仅靠调用方自律 + TOCTOU）。
- `src/file_manager/file_tree_cache.rs:34-48, 177-227`：磁盘缓存仅每服务器上限（200×512KB），服务器数量无上限，sftp_cache.json 单文件全量序列化，历史 50 台可 GB 级。
- `src/workspace/tab_bar.rs:552-565, 683-706`：拖拽期间每 tab 每帧重建 `Vec<Bounds>`。
- `src/editor/git_repository.rs:251-269`：本地 status 每次调用 fork git（远程有 5s 缓存，本地无）。

---

## 五、死代码与清理

| 位置 | 内容 | 处置 |
|---|---|---|
| `src/workspace/split_manager.rs:485-553, 257-291, 151-156` | `SplitManager` 整个结构体、`split_panel`、`get_panel_entity` 无任何使用方 | 删除约 120 行，文件可更名 split_container.rs |
| `src/terminal/terminal.rs:837-914` | `new_remote` 无调用方（new_ssh/new_ssh_pty 才在用） | 删除 |
| `src/terminal/terminal.rs:1269-1271` | `remote_working_directory` 无调用 | 删除 |
| `src/editor/git_repository.rs:293-319` | `diff_file` 双开 git 子进程，生产走 diff_file_contents | 删除或 #[cfg(test)] |
| `src/ssh/config.rs:438-479, 307` | `KnownHost`/`add_known_host`/`find_known_host`、`with_password` 无调用且语义误导 | 删除 |
| `src/builtin_commands/argument.rs:46-76` | `tokenize` 死代码且与 `shell_split_tokens` 语义冲突（引号/空格/中文路径行为不一致） | 删除，保留 extract_argument |
| `src/workspace/panel.rs:65-68, 48-51` | `PanelEvent::TabChanged`/`DragPreview` 无消费方 | 删除 |
| `src/sidebar_view/hosts_view.rs:179-183, 33-35` | `load_hosts_from_config`、单变体 HostType | 删除 |
| `src/theme_view.rs:63-85` | 搜索框纯装饰不过滤 | 接 InputState 或移除 |
| `src/main.rs:686` | 注释混入俄文单词 | 修正 |
| `docs/` 4 份带日期旧审计 | 行号已失效易误导 | 归档移出 |
| `script/` 与 `scripts/` | 两目录同类事 | 合并 |
| CLAUDE.md / AGENTS.md | 内容完全相同 | 合一 |

Cargo.toml 裁剪：`async-io`、`async-process`（src 零引用）、`tracing`（零使用，日志走 log+env_logger）、`rand`（仅 scp_key_lease.rs:323 一处，可换 uuid 派生）整行可删；`dirs` 5.0→6.x；`keyring` 仅 apple-native 却面向三平台（Windows 上 Entry::new 直接报错，应按 target 启用 windows-native）；`copyright 2024`→2024-2026。另：`builtin_commands/executor.rs:124` show_error_toast 是 TODO 空壳——所有内建命令失败用户不可见，NotificationManager 已在 main.rs:1311 全局接入，接上即可。

---

## 六、结构优化（架构层面）

### 🔵 R1. "通用组件层"反向依赖全部业务模块（最大架构债）
`src/components/macos_sidebar/mod.rs:35-48` 直接 use sidebar_view 5 个视图 + TerminalView + file_manager；`title_bar.rs:21-22` use workspace；`input/mode.rs:10`、`state.rs:31-33` use editor::syntax。components 无法复用、无法独立演进。
**建议**：TitleBar 移入 workspace/；MacosSidebar 改 `Vec<Entity<dyn Render>>` 插槽；语法依赖经 trait 注入。

### 🔵 R2. 三个模块环
- `terminal ↔ terminal_view`：terminal.rs:53 use terminal_view::CTRL_V（一个 pub const 形成的环，常量下沉即解）。
- `ssh ↔ file_manager`：sftp_fs.rs:17-18 use file_manager::{TRANSFER_CANCELLED_MSG, RemoteFileTreeCache}——基础设施被上层类型穿透，二者应下沉 ssh 或中性模块。
- `file_manager → editor`：file_tree_cache.rs:8 use editor::encoding::FileEncoding——FileEncoding 应独立成 encoding 模块共用。

### 🔵 R3. 巨型文件的拆分路线（全部有既成模式可循）
| 文件 | 行数 | 拆法 |
|---|---|---|
| `components/input/state.rs` | 5642 | 按 history.rs/movement.rs 的"impl InputState 分文件"模式拆 7 块：highlight_extras（同词高亮+括号+sticky，246-677 全是自由函数可先平移）、line_edits、fold_actions、decorations、scroll、ime、tests 独立文件 |
| `builtin_commands/mod.rs` | 3201 | 1410-1707 并入 scp_completion.rs；1709-1938 BuiltinCommandState → command_buffer.rs；1980-2440 渲染 → overlay_view.rs；余下即 manager.rs |
| `editor/editor.rs` | 3279 | 拆 file_io / fs_watch / tab_management；EditorFile 的 7 个流程布尔归并为 `enum TabLifecycle` 状态机 |
| `terminal_view/terminal_view.rs` | 3098 | TerminalView 100+ 字段拆 ScrollbarState/ToastState/SshConnectState/BuiltinCommandState 子结构；三份近乎相同的订阅闭包（505/696/837）抽 on_terminal_event |
| `terminal/terminal.rs` | 2886 | 三份 35 字段构造列表合并；remote_* 字段实为 OSC 上报语义，统一改 reported_*；事件泵模式重复 4 次抽 spawn_event_pump |
| `workspace/workspace.rs` | 2633 | 传输任务表+冲突编排（约 500 行）→ transfer_hub.rs；resize Element → resize.rs；广播队列全局锁 → per-workspace channel |
| `terminal_view/terminal_element.rs` prepaint | 620 行单函数 | 按 metrics→sync/resize→damage→缓存重建→光标→overlay 拆 5-6 个函数 |
| `ssh/sftp_fs.rs` | 1834 | 拆 session/transfer/commands/path 四块；认证逻辑三处重复收敛 auth.rs；upload/download 5 变体族统一 TransferOptions |

### 🔵 R4. 重复实现的收敛
- 配置"读+迁移+bak+原子写"模式 5 份（theme、font_settings、ssh config、keychain、persistence），行为细节各异 → 统一 JsonConfigStore。
- shell quoting 双实现（file_manager/utils.rs:77-86 与 editor/git_repository.rs:684）→ 合并一处。
- 侧边栏三视图（host/keychain/theme）header+搜索框+行渲染同构 → 抽 SidebarHeaderView/SidebarRow（省 300+ 行）。
- 体积/速率格式化多处内联复制（cross_remote.rs:713-716, 997-1006 vs view.rs:12）；MAX_IMAGE_BYTES 20MB 常量双份。
- 冲突"缓存命中即回复"语义分散三处（conflict.rs:78-83 / workspace.rs:1060-1067 / 1093-1099）→ 收敛进 SharedConflictResolver::prompt。
- `file_manager/view.rs:525-617` 的 remote_total_size（含 connect 副作用）是领域逻辑混进 view 模块 → 移 manager/sizing。

### 🔵 R5. 其它结构性事项
- `src/components/input/search.rs:322-339`：SearchPanel 持编辑器强引用、InputState.search_panel 反向持有——Entity 双向强引用，tab 复用生命周期下成常驻环；改 WeakEntity。
- `src/workspace/workspace.rs:994-1011`：100ms 轮询代替 recv().await 是发送端所有权结构问题的 workaround；发送端移交传输任务即回归 await。
- `src/main.rs:1228-1243`：每条通知 spawn 一个 detached 4s 轮询任务；应由 NotificationManager 单一定时器驱动。
- CI（.github/workflows/ci.yml）三个 job 全 macos-latest，Windows/Linux 的 russh feature 差异与 bundle-windows.ps1 无编译回归兜底。
- markdown 渲染三件套缺"渲染产物缓存生命周期"文档（fragment_texts 懒注册语义未在类型层表达）。
- `InputState` masked 换算两套独立实现（state.rs:4096-4107 与 element.rs:96），应抽唯一函数。
- 主题硬编码明暗分支（macos_select.rs:337-341、modal.rs:341）→ UiTheme 增加语义色。
- 光标移动按单 char 而非字素簇：合成 emoji（ZWJ 序列/国旗）被拆开，建议列已知限制并择机引入 grapheme 边界（state.rs:4213-4234）。

---

## 七、值得肯定的方面（避免误改）

- **错误处理纪律**：287 处 unwrap 中 95% 隔离在 #[cfg(test)]；生产 18 处多有显式守卫。
- **异步纪律**：文件树/git 面板全走 background executor + 防抖 + generation 丢弃过期结果；远端补全"pending+LRU+失败占位"模式质量高。
- **持久化**：原子写 + .bak + legacy 迁移三层兜底（theme/ssh/keychain/persistence 一致）。
- **安全基线**：明文凭据阻断落盘、keyring 写后回读校验、0600 secret file、safe_open scheme 白名单、双层 shell 转义、路径遍历防御、粘贴清洗防注入。
- **diff/markdown 质量**：imara-diff 移植配 CRLF/CJK/emoji 回归测试；display_map 的折叠语义移植（offset 域 fold、边界编辑丢弃）完成度高。
- **注释文化**：普遍解释"为什么"而非"是什么"；TODO 仅 4 处，无 FIXME/HACK。
- 全局状态克制（5 处 Global 均有明确 init 时机）；build.rs 干净。

---

## 八、修复优先级建议

**第一批（安全 + 数据安全，1-2 天量级）**
A1 mktemp / A3 scp known_hosts / A4 SFTP 原子写 / A2 zeroize（最小改动：Zeroizing 贯穿 + 脱敏 Debug）/ C2 租约竞态 / A5 hover 预取不落盘 / A6 TOFU 失败拒连。

**第二批（用户可感知的正确性）**
C3 全部（U+2547、拖拽死区、Auto serde、IPv6、timeout、重连缓存清理、goto_line CRLF、markdown 全选）/ A7 host_form 副作用 / B4 osascript 假死 / C4 的 write_raw、move 二次弹窗、部分失败 summary。

**第三批（性能热路径）**
P1 text_wrapper splice + layout_cursor 二分 / P2 text_version 闸门贯通 diff+markdown、Query 缓存 / P3 runs O(n²)、mouse 去重 / P4 远端 watch 元数据优先、AutoRename HashSet / B1 侧边栏 Global 缓存 / B2 本地补全异步化。

**第四批（结构重构，随迭代进行）**
R1 components 解耦 → R2 模块环解除 → R3 巨型文件按表逐个拆（先 builtin_commands mod.rs，投入最小收益最大）→ R4 收敛重复 → Cargo 裁剪 + CI 跨平台。
