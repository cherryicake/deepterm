//! builtin_commands 的单元测试（从 mod.rs 拆出以控制单文件体积）。
//! 通过 `#[path]` 挂载为 mod.rs 的 `tests` 子模块。

use super::{BuiltinCommandManager, CommandHistory, find_builtin_command, resolve_command_line};

#[test]
fn parses_builtin_open_command() {
    assert_eq!(
        find_builtin_command("/open").map(|cmd| cmd.name),
        Some("/open")
    );
    assert_eq!(
        find_builtin_command("/open my-target").map(|cmd| cmd.name),
        Some("/open")
    );
}

#[test]
fn leaves_unknown_slash_command_to_shell() {
    assert_eq!(find_builtin_command("/unknown"), None);
    assert_eq!(find_builtin_command("ls"), None);
}

#[test]
fn resolves_selected_command_when_query_is_a_prefix() {
    let registry = super::global_registry();
    let selected = registry.find("/open");
    assert_eq!(
        resolve_command_line("/op target", selected.as_deref(), false),
        "/open target"
    );
}

#[test]
fn keeps_unknown_query_when_selection_only_comes_from_fallback() {
    let registry = super::global_registry();
    let selected = registry.find("/open");
    assert_eq!(
        resolve_command_line("/unknown target", selected.as_deref(), false),
        "/unknown target"
    );
}

#[test]
fn pick_completes_selected_command_and_keeps_args() {
    let mut manager = BuiltinCommandManager::new();
    manager.update("/op target");
    manager.pick(0);

    // 缓冲被替换为完整命令
    assert_eq!(manager.query(), Some("/open target"));
    // 选择已应用 ⇒ 高亮重置为 None（"无选中" 是新的默认）
    assert_eq!(manager.selected_index(), None);
    assert!(!manager.is_dismissed());
}

#[test]
fn set_buffer_replaces_buffer_and_clamps_cursor() {
    let mut manager = BuiltinCommandManager::new();
    manager.set_buffer("/scp alice:/etc/hosts ./", 5);

    assert_eq!(manager.query(), Some("/scp alice:/etc/hosts ./"));
    assert_eq!(manager.cursor(), Some(5));
    assert!(manager.is_active());

    // 光标超出缓冲长度时 clamp 到末尾
    manager.set_buffer("/scp a", 999);
    assert_eq!(manager.cursor(), Some(6));

    // 空缓冲清空状态
    manager.set_buffer("", 0);
    assert_eq!(manager.query(), None);
    assert!(!manager.is_active());
}

#[test]
fn set_cursor_snaps_to_char_boundary() {
    let mut manager = BuiltinCommandManager::new();
    // "中" 占 3 字节，光标落在多字节字符中间时回退到最近边界
    manager.set_buffer("/scp 中", 5);
    assert_eq!(manager.cursor(), Some(5)); // "/scp " 末尾是合法边界

    manager.set_buffer("中文", 4);
    assert_eq!(manager.cursor(), Some(3)); // 回退到第一个"中"之后

    manager.set_buffer("中文", 999);
    assert_eq!(manager.cursor(), Some(6));
}

// ── overlay 状态机行为 ─────────────────────────────────────────────────────

#[test]
fn hide_overlay_only_hides_overlay_but_keeps_builtin_semantics() {
    let mut manager = BuiltinCommandManager::new();
    manager.update("/scp alice:/a /tmp/");
    manager.hide_overlay();
    assert_eq!(
        manager.overlay_state(),
        super::OverlayState::HiddenAfterPick
    );
    assert!(manager.is_overlay_hidden());
    // HiddenAfterPick 不应被解读为 "ESC 取消"
    assert!(!manager.is_dismissed());
}

#[test]
fn dismiss_persists_until_buffer_cleared() {
    let mut manager = BuiltinCommandManager::new();
    manager.update("/scp something");
    manager.dismiss();
    assert_eq!(
        manager.overlay_state(),
        super::OverlayState::DismissedUntilReset
    );
    // 继续输入普通字符不应让浮层复活
    manager.update("x");
    assert!(manager.is_dismissed());
    // 删光字符后应复位
    while manager.query().is_some() {
        manager.pop();
    }
    assert_eq!(manager.overlay_state(), super::OverlayState::Visible);
}

#[test]
fn dismiss_revives_when_slash_prepended_at_front() {
    // Warp 语义：从没有 `/` 的缓冲插一个 `/` 回前头，视作重新进入内建命令
    let mut manager = BuiltinCommandManager::new();
    manager.update("abc");
    manager.dismiss();
    assert!(manager.is_dismissed());
    // 当前 update 只支持追加 —— 我们用"清空 + 重打 `/`" 模拟"首字符变 `/`"
    while manager.query().is_some() {
        manager.pop();
    }
    manager.update("/open");
    assert_eq!(manager.overlay_state(), super::OverlayState::Visible);
}

#[test]
fn space_after_scp_enters_argument_phase() {
    // `/scp ` 末尾空格 + 精确命令匹配 ⇒ 立即进入参数补全阶段
    let mut manager = BuiltinCommandManager::new();
    manager.update("/scp");
    assert!(!manager.is_argument_input());
    manager.update(" ");
    assert!(manager.is_argument_input());
}

#[test]
fn remote_completion_replacement_keeps_colon_outside_quotes() {
    // host 含空格时，补全输出必须是 `"host":path`，否则提交时 `:` 会被当成
    // 引号内部字符，导致 parse_scp_token 把整段识别成本地路径。
    let entries = vec![super::RemoteEntrySnapshot {
        name: "app.tar".to_string(),
        is_directory: false,
        full_path: "/home/me/app.tar".to_string(),
    }];
    let items = super::build_remote_completions(
        &entries,
        "",              // file_prefix
        Some("my host"), // host_token（含空格）
        "/home/me/",     // current_path_prefix（光标处字面量）
    );
    assert_eq!(items.len(), 1);
    let replacement = items[0].replacement.clone().unwrap();
    // `:` 必须在 `my host` 的闭合引号之后
    let colon_pos = replacement.find(':').expect(": must exist");
    let close_quote_pos = replacement.find("\" ").or_else(|| {
        // 若后续没有空格，直接取第一个闭合引号
        replacement.match_indices('"').nth(1).map(|(i, _)| i)
    });
    assert!(
        close_quote_pos.is_some_and(|p| p < colon_pos),
        "expected closing quote before colon, got {}",
        replacement
    );
    // 完整格式
    assert_eq!(replacement, "\"my host\":/home/me/app.tar");
}

#[test]
fn remote_completion_replacement_quotes_user_at_host_with_port() {
    // user@host:port 含 `:`，必须整体引号化，否则 first_unquoted_colon
    // 会切到端口前的 `:`。
    let entries = vec![super::RemoteEntrySnapshot {
        name: "x.txt".to_string(),
        is_directory: false,
        full_path: "/tmp/x.txt".to_string(),
    }];
    let items = super::build_remote_completions(
        &entries,
        "",
        Some("deploy@1.2.3.4:2222"), // user@host:port
        "/tmp/",
    );
    assert_eq!(items.len(), 1);
    let r = items[0].replacement.clone().unwrap();
    assert_eq!(r, "\"deploy@1.2.3.4:2222\":/tmp/x.txt");
}

#[test]
fn match_hosts_returns_user_at_host_replacement() {
    use crate::ssh::config::{AuthMethod, SshConnectionConfig};
    let mut cfg =
        SshConnectionConfig::new("1.2.3.4".to_string(), "root".to_string(), AuthMethod::Agent);
    cfg.nickname = Some("alice".to_string());
    let saved = vec![cfg];
    let suggestions = super::scp_completion::match_hosts_by_prefix("ali", &saved);
    let nickname_hit = suggestions
        .iter()
        .find(|s| s.kind == super::scp_completion::HostSuggestionKind::Nickname)
        .expect("nickname hit");
    // display 仍是匹配源（保留 nickname 用于 UI 描述）
    assert_eq!(nickname_hit.display, "alice");
    // replacement 统一为 user@host
    assert_eq!(nickname_hit.replacement, "root@1.2.3.4");
}

#[test]
fn bare_remote_completion_uses_terminal_cwd_and_writes_naked_path() {
    // 远程终端 + 没有显式 host 的 token：补全应当走 SFTP 列出当前 shell 的 cwd，
    // replacement 为裸路径（不带 host:），与"裸路径跟随当前终端"一致。
    use crate::ssh::config::{AuthMethod, SshConnectionConfig};
    let cfg =
        SshConnectionConfig::new("1.2.3.4".to_string(), "root".to_string(), AuthMethod::Agent);
    let cfg_id = cfg.id();

    let mut manager = BuiltinCommandManager::new();
    manager.set_terminal_context(Some(cfg.clone()), Some("/srv/app".to_string()));
    manager.update("/scp lo");

    // 第一次刷新会标记 loading，并把 (cfg_id, "/srv/app") 这把 key 注入待加载队列
    assert!(manager.is_loading_remote);
    let req = manager
        .pending_remote_request()
        .expect("pending remote request must be issued");
    assert_eq!(req.config, cfg);
    assert_eq!(req.absolute_dir, "/srv/app");

    // 模拟 SFTP 加载完成
    manager.apply_remote_dir_result(
        cfg_id.clone(),
        "/srv/app".to_string(),
        Ok(vec![
            super::RemoteEntrySnapshot {
                name: "logs".to_string(),
                is_directory: true,
                full_path: "/srv/app/logs".to_string(),
            },
            super::RemoteEntrySnapshot {
                name: "log.txt".to_string(),
                is_directory: false,
                full_path: "/srv/app/log.txt".to_string(),
            },
        ]),
    );

    // 远程终端 HostOrLocal：host 候选（含 Local 关键字）排在前，
    // 后面才是 SFTP 列出的远程文件。
    let names: Vec<&str> = manager
        .path_completions
        .iter()
        .map(|i| i.name.as_str())
        .collect();
    assert!(
        names.contains(&"logs") && names.contains(&"log.txt"),
        "expected remote files in completion list, got {:?}",
        names
    );

    // 找出 logs/ 与 log.txt 的条目，验证 replacement 是裸路径（不带 host: 前缀）
    let logs = manager
        .path_completions
        .iter()
        .find(|i| i.name == "logs" && i.is_directory)
        .expect("logs/ entry");
    assert_eq!(logs.replacement.as_deref(), Some("logs/"));
    let log_txt = manager
        .path_completions
        .iter()
        .find(|i| i.name == "log.txt")
        .expect("log.txt entry");
    assert_eq!(log_txt.replacement.as_deref(), Some("log.txt"));
}

#[test]
fn remote_dir_failure_shows_error_then_retries_without_loop() {
    // 远端目录读取失败时：① 收敛成错误占位（不再永远卡在"加载中"），
    // ② 不重新注册请求（断开失败→刷新→重试的无限循环），
    // ③ 下一次用户输入触发刷新时重新注册请求，自动重试。
    use crate::ssh::config::{AuthMethod, SshConnectionConfig};
    let cfg =
        SshConnectionConfig::new("1.2.3.4".to_string(), "root".to_string(), AuthMethod::Agent);
    let cfg_id = cfg.id();

    let mut manager = BuiltinCommandManager::new();
    manager.set_terminal_context(Some(cfg.clone()), Some("/srv/app".to_string()));
    manager.update("/scp lo");

    // 首次：标记 loading 并注册请求
    assert!(manager.is_loading_remote);
    let req = manager
        .pending_remote_request()
        .expect("first request issued");
    assert_eq!(req.absolute_dir, "/srv/app");

    // 模拟读取失败
    manager.apply_remote_dir_result(
        cfg_id.clone(),
        "/srv/app".to_string(),
        Err("权限不足".into()),
    );

    // ① 错误占位应出现在补全列表中（HostOrLocal 阶段它排在 host 候选之后）
    let failed_item = manager
        .path_completions
        .iter()
        .find(|i| i.name == "无法读取远端目录")
        .expect("failure placeholder must be shown instead of staying on loading");
    assert_eq!(failed_item.description.as_deref(), Some("权限不足"));
    // ② 失败后不得再有挂起请求（否则就是无限重试）
    assert!(
        manager.pending_remote_request().is_none(),
        "failure must not re-register a pending request"
    );

    // ③ 用户继续输入触发刷新 ⇒ 失败标记已消费，重新注册请求重试
    manager.update("g");
    let retry = manager
        .pending_remote_request()
        .expect("next input should retry");
    assert_eq!(retry.absolute_dir, "/srv/app");
}

#[test]
fn bare_local_completion_in_local_terminal_keeps_host_listing() {
    // 本地终端不应改变行为：HostOrLocal 仍然能列出本机文件 + 已保存主机
    let mut manager = BuiltinCommandManager::new();
    // 没有 set_terminal_context ⇒ ssh = None ⇒ 走原有 build_host_or_local_completions
    manager.update("/scp ");
    // 至少应有本地条目；不强求具体文件名（依赖 cwd）
    // —— 关键是不能进入 loading_remote 分支
    assert!(!manager.is_loading_remote);
}

#[test]
fn path_overlay_starts_without_highlight() {
    // 路径建议浮层弹出时不应预选任何项 —— 用户必须主动按 ↑/↓/Tab 才能选中。
    let mut manager = BuiltinCommandManager::new();
    manager.update("/open ");
    assert!(manager.is_argument_input());
    assert_eq!(manager.selected_path_index(), None);
}

#[test]
fn dir_pick_keeps_overlay_but_does_not_preselect() {
    // /open 选中目录后浮层保持可见，但不应自动选中第一项。
    // 用 Local 关键字模拟"本机目录补全 + 选中目录"路径，规避对真实 fs 的依赖。
    let mut manager = BuiltinCommandManager::new();
    manager.update("/scp Local:/");
    // 保证 path_completions 非空再走 complete_path：直接构造一个 Directory 项
    manager.path_completions = vec![super::PathCompletionItem {
        name: "etc".to_string(),
        path: std::path::PathBuf::from("/etc"),
        is_directory: true,
        kind: super::CompletionKind::Directory,
        description: None,
        icon: None,
        replacement: Some("Local:/etc/".to_string()),
    }];
    manager.path_selected_index = Some(0); // 用户已主动选中
    manager.complete_path();
    // 选中目录后浮层应当 Visible，但不应再自动预选
    assert_eq!(manager.overlay_state(), super::OverlayState::Visible);
    assert_eq!(manager.selected_path_index(), None);
}

// ── 渐进补全流程 ────────────────────────────────────────────────────────

#[test]
fn overlay_starts_without_highlight() {
    // /sc 弹出命令候选，默认无高亮
    let mut manager = BuiltinCommandManager::new();
    manager.update("/sc");
    assert!(!manager.suggestions().is_empty());
    assert_eq!(manager.selected_index(), None);
    assert!(!manager.has_active_completion());
}

#[test]
fn arrow_down_first_press_lands_on_first_item() {
    let mut manager = BuiltinCommandManager::new();
    manager.update("/sc");
    manager.select_next();
    assert_eq!(manager.selected_index(), Some(0));
    assert!(manager.has_active_completion());
}

#[test]
fn arrow_up_first_press_lands_on_last_item() {
    let mut manager = BuiltinCommandManager::new();
    manager.update("/sc");
    let total = manager.suggestions().len();
    manager.select_previous();
    assert_eq!(manager.selected_index(), Some(total - 1));
}

#[test]
fn typing_after_highlight_resets_to_no_selection() {
    let mut manager = BuiltinCommandManager::new();
    manager.update("/sc");
    manager.select_next();
    assert_eq!(manager.selected_index(), Some(0));
    manager.update("p");
    assert_eq!(manager.selected_index(), None);
}

#[test]
fn enter_with_no_selection_leaves_buffer_unchanged() {
    // 模拟 TerminalView 的 Enter 分支 B：浮层 visible 但未高亮 ⇒ 仅关浮层
    let mut manager = BuiltinCommandManager::new();
    manager.update("/sc");
    let buffer_before = manager.query().map(str::to_string);
    // 没按 ↓/Tab，has_active_completion 为 false
    assert!(!manager.has_active_completion());
    manager.hide_overlay();
    assert_eq!(manager.query().map(str::to_string), buffer_before);
    assert_eq!(
        manager.overlay_state(),
        super::OverlayState::HiddenAfterPick
    );
}

#[test]
fn advance_after_pick_appends_space_when_more_args_needed() {
    // /scp 第一段补全完后，应自动追加空格触发第二段浮层
    let mut manager = BuiltinCommandManager::new();
    manager.update("/scp Local:/tmp/a.txt");
    manager.advance_after_pick();
    // 末尾应已加空格，进入第 2 个 token 阶段
    assert!(manager.query().unwrap().ends_with(' '));
    // 第二段 host 浮层应当 visible 且无高亮
    assert!(manager.is_argument_input());
    assert_eq!(manager.path_selected_index, None);
}

#[test]
fn advance_after_pick_does_not_append_when_dir_selected() {
    // 选中目录后 path 末尾是 `/`，用户继续在该 token 内展开 ⇒ 不 advance
    let mut manager = BuiltinCommandManager::new();
    manager.update("/scp Local:/tmp/");
    manager.advance_after_pick();
    // 不应加空格
    assert_eq!(manager.query(), Some("/scp Local:/tmp/"));
    // 浮层保持 Visible —— 让用户继续选择子目录
    assert_eq!(manager.overlay_state(), super::OverlayState::Visible);
}

#[test]
fn advance_after_pick_hides_overlay_when_args_complete() {
    // 两个 token 都填了 ⇒ 不再 advance，浮层进入 HiddenAfterPick 等回车提交
    let mut manager = BuiltinCommandManager::new();
    manager.update("/scp Local:/tmp/a.txt Local:/var/");
    // 注意：尾斜杠会让 advance 提前返回（dir 分支），先去掉它来构造"两段都已填具体文件"
    manager.update("b.txt");
    manager.advance_after_pick();
    assert_eq!(
        manager.query(),
        Some("/scp Local:/tmp/a.txt Local:/var/b.txt")
    );
    assert_eq!(
        manager.overlay_state(),
        super::OverlayState::HiddenAfterPick
    );
}

#[test]
fn advance_after_pick_does_not_append_after_host_colon() {
    // 选中 host 后缓冲变成 `/scp <host>:`，path 还没输入。
    // 不应追加空格，否则 `:` 会被切成 token 边界，scp 参数解析失败。
    let mut manager = BuiltinCommandManager::new();
    manager.update("/scp root@1.2.3.4:");
    manager.advance_after_pick();
    assert_eq!(manager.query(), Some("/scp root@1.2.3.4:"));
}

// ── 光标编辑（cursor-aware buffer）─────────────────────────────────────────

#[test]
fn insert_at_cursor_after_moving_left() {
    let mut manager = BuiltinCommandManager::new();
    manager.update("/scp ab");
    // 光标默认在末尾（7）
    assert_eq!(manager.cursor(), Some(7));
    manager.move_cursor_left(); // 光标到 6（"a" 与 "b" 之间）
    assert_eq!(manager.cursor(), Some(6));
    manager.update("X"); // 在光标处插入
    assert_eq!(manager.query(), Some("/scp aXb"));
    // 插入后光标落在插入串之后
    assert_eq!(manager.cursor(), Some(7));
}

#[test]
fn delete_forward_removes_char_after_cursor() {
    let mut manager = BuiltinCommandManager::new();
    manager.update("/scp ab");
    manager.move_cursor_left(); // 光标在 "a" 与 "b" 之间
    manager.delete_forward(); // 删 "b"
    assert_eq!(manager.query(), Some("/scp a"));
}

#[test]
fn backspace_deletes_before_cursor_in_middle() {
    let mut manager = BuiltinCommandManager::new();
    manager.update("/scp ab");
    manager.move_cursor_left(); // 光标在 6
    manager.pop(); // 删光标前的 "a"
    assert_eq!(manager.query(), Some("/scp b"));
    assert_eq!(manager.cursor(), Some(5));
}

#[test]
fn home_and_end_jump_to_edges() {
    let mut manager = BuiltinCommandManager::new();
    manager.update("/scp ab");
    manager.move_cursor_home();
    assert_eq!(manager.cursor(), Some(0));
    manager.move_cursor_end();
    assert_eq!(manager.cursor(), Some(7));
}

#[test]
fn word_motion_and_word_delete() {
    let mut manager = BuiltinCommandManager::new();
    manager.update("/scp foo bar");
    // 末尾按词左移：跳过 "bar" 到其词首（位置 9）
    manager.move_cursor_word_left();
    assert_eq!(manager.cursor(), Some(9));
    // 再按词左移：跳过 "foo " 到 "foo" 词首（位置 5）
    manager.move_cursor_word_left();
    assert_eq!(manager.cursor(), Some(5));
    // 删词：删掉 "foo "（光标前一个词含尾随空格的处理由 prev_word_boundary 决定）
    manager.move_cursor_end();
    manager.delete_word_backward();
    assert_eq!(manager.query(), Some("/scp foo "));
}

#[test]
fn cursor_moves_respect_cjk_grapheme_boundary() {
    let mut manager = BuiltinCommandManager::new();
    manager.update("/scp 中文");
    // 末尾光标在 "/scp ".len()(5) + 6 = 11
    assert_eq!(manager.cursor(), Some(11));
    manager.move_cursor_left(); // 跨过 "文"（3 字节）
    assert_eq!(manager.cursor(), Some(8));
    manager.move_cursor_left(); // 跨过 "中"（3 字节）
    assert_eq!(manager.cursor(), Some(5));
}

#[test]
fn completion_follows_cursor_to_local_source_token() {
    // 复现截图场景：`/scp Downloads/ root@1.2.3.4:~/`，光标在本地源 token。
    // 弹窗应列本地文件（HostOrLocal），而不是末尾远端目的 token 的远端目录。
    let mut manager = BuiltinCommandManager::new();
    manager.update("/scp Downloads/ root@1.2.3.4:~/");
    // 把光标移回到 "Downloads/" 之后（本地源 token 末尾）
    // 末尾长度 - "root@1.2.3.4:~/".len() - 1(空格) = "/scp Downloads/".len()
    let target = "/scp Downloads/".len();
    while manager.cursor().unwrap() > target {
        manager.move_cursor_left();
    }
    assert_eq!(manager.cursor(), Some(target));
    // 移动光标已触发参数阶段补全重算（move_cursor_with）。
    // 不应进入远端加载（说明没把焦点落在 host:path 远端 token 上）
    assert!(
        !manager.is_loading_remote,
        "cursor on local source must not trigger remote dir load"
    );
}

// ── 历史候选选择与回填 ─────────────────────────────────────────────────────

fn manager_with_history(lines: &[&str]) -> BuiltinCommandManager {
    let mut history = CommandHistory::default();
    for (i, l) in lines.iter().enumerate() {
        history.record(l, 100 + i as i64);
    }
    let mut manager = BuiltinCommandManager::new();
    manager.set_history(history);
    manager
}

#[test]
fn history_matches_surface_in_command_name_phase() {
    let mut manager = manager_with_history(&["/scp alice:/etc/hosts ./"]);
    manager.update("/sc");
    // 输入 /sc 时应召回历史 /scp 整行
    assert_eq!(manager.history_matches().len(), 1);
    assert!(manager.should_render_overlay());
}

#[test]
fn combined_selection_traverses_history_then_base() {
    let mut manager = manager_with_history(&["/scp alice:/x ./"]);
    manager.update("/sc");
    // 第一次 ↓ 落在历史段第 0 项
    manager.select_next();
    assert_eq!(manager.history_selected_index(), Some(0));
    assert_eq!(manager.selected_index(), None);
    // 再 ↓ 进入原候选段（命令名建议）第 0 项
    manager.select_next();
    assert_eq!(manager.history_selected_index(), None);
    assert_eq!(manager.selected_index(), Some(0));
}

#[test]
fn complete_history_fills_whole_line() {
    let mut manager = manager_with_history(&["/scp alice:/etc/hosts ./"]);
    manager.update("/sc");
    manager.select_next(); // 选中历史段第 0 项
    assert!(manager.has_active_completion());
    manager.complete_history();
    // 整行回填，光标落末尾，浮层收起
    assert_eq!(manager.query(), Some("/scp alice:/etc/hosts ./"));
    assert_eq!(manager.cursor(), Some("/scp alice:/etc/hosts ./".len()));
    assert_eq!(
        manager.overlay_state(),
        super::OverlayState::HiddenAfterPick
    );
}

#[test]
fn history_matches_surface_in_argument_phase() {
    let mut manager = manager_with_history(&["/scp alice:/etc/hosts ./"]);
    manager.update("/scp ali");
    assert!(manager.is_argument_input());
    // 整行子串匹配：输入 /scp ali 召回含 alice 的历史
    assert_eq!(manager.history_matches().len(), 1);
}

use super::*;

fn remote_terminal_manager() -> (BuiltinCommandManager, String) {
    use crate::ssh::config::{AuthMethod, SshConnectionConfig};
    let cfg =
        SshConnectionConfig::new("1.2.3.4".to_string(), "root".to_string(), AuthMethod::Agent);
    let cfg_id = cfg.id();
    let mut manager = BuiltinCommandManager::new();
    manager.set_terminal_context(Some(cfg), Some("/srv/app".to_string()));
    (manager, cfg_id)
}

#[test]
fn clicking_loading_placeholder_does_not_touch_buffer() {
    let (mut manager, _cfg_id) = remote_terminal_manager();
    manager.update("/scp /srv/ap");
    assert!(manager.is_loading_remote);
    // 模拟鼠标点击 loading 占位项（此前会把「读取远端目录中…」写进命令行）
    manager.pick_path(0);
    assert_eq!(manager.query(), Some("/scp /srv/ap"));
}

#[test]
fn clicking_failed_placeholder_does_not_touch_buffer() {
    let (mut manager, cfg_id) = remote_terminal_manager();
    manager.update("/scp /srv/ap");
    let _ = manager.pending_remote_request().expect("request issued");
    manager.apply_remote_dir_result(cfg_id, "/srv".to_string(), Err("权限不足".into()));
    let idx = manager
        .path_completions
        .iter()
        .position(|i| i.name == "无法读取远端目录")
        .expect("failure placeholder present");
    // 模拟鼠标点击失败占位项（此前会把「无法读取远端目录」写进命令行）
    manager.pick_path(idx);
    assert_eq!(manager.query(), Some("/scp /srv/ap"));
}

#[test]
fn placeholder_items_are_marked_non_interactive() {
    let (mut manager, cfg_id) = remote_terminal_manager();
    manager.update("/scp /srv/ap");
    let _ = manager.pending_remote_request().expect("request issued");
    manager.apply_remote_dir_result(cfg_id, "/srv".to_string(), Err("权限不足".into()));
    let failed = manager
        .path_completions
        .iter()
        .find(|i| i.name == "无法读取远端目录")
        .unwrap();
    assert!(failed.is_placeholder());
}

use crate::ssh::config::{AuthMethod, SshConnectionConfig};

fn port_config() -> SshConnectionConfig {
    let mut cfg = SshConnectionConfig::new(
        "1.2.3.4".to_string(),
        "deploy".to_string(),
        AuthMethod::Agent,
    );
    cfg.port = 2222;
    cfg
}

#[test]
fn port_syntax_completion_resolves_host_with_port() {
    // 输入 `deploy@1.2.3.4:2222:/tm`（未加引号的手写端口形式）：
    // host 匹配应把端口并回 host，按 /tm 列远端目录
    let mut manager = BuiltinCommandManager::new();
    manager.set_ssh_configs(vec![port_config()]);
    manager.update("/scp deploy@1.2.3.4:2222:/var/lo");

    let req = manager
        .pending_remote_request()
        .expect("should request remote dir for host:port");
    // `/var/lo` ⇒ 列 /var 目录、按 "lo" 过滤（端口已并回 host，不是路径一部分）
    assert_eq!(req.absolute_dir, "/var");
    // 请求应携带端口配置的完整快照
    assert_eq!(req.config, port_config());
}

#[test]
fn port_syntax_localhost_not_completed_as_local() {
    // `user@localhost:2222:/tm` 不应走本机路径补全（否则会把 `2222:/tm`
    // 当本机路径，列出本机文件误导用户）
    let mut manager = BuiltinCommandManager::new();
    manager.set_ssh_configs(vec![]);
    manager.update("/scp someone@localhost:2222:/tm");
    // 无保存连接 ⇒ 无 host 匹配 ⇒ 不应有本机文件候选，也不应有远端请求
    assert!(manager.pending_remote_request().is_none());
    let has_local_files = manager
        .path_completions
        .iter()
        .any(|i| i.kind == CompletionKind::File || i.kind == CompletionKind::Directory);
    assert!(
        !has_local_files,
        "user@localhost:2222: 前缀不应触发本机文件补全"
    );
}

#[test]
fn plain_local_keyword_completion_still_works() {
    // `Local:/tm`（无端口）仍走本机补全
    let mut manager = BuiltinCommandManager::new();
    manager.set_ssh_configs(vec![]);
    manager.update("/scp Local:/tm");
    assert!(manager.pending_remote_request().is_none());
    // 本机补全由 cwd 决定，可能有或没有条目，但绝不应出现远端 loading
    assert!(!manager.is_loading_remote);
}

fn temp_completion_dir(tag: &str) -> std::path::PathBuf {
    let dir =
        std::env::temp_dir().join(format!("deepterm-completion-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn local_completion_is_async_and_discards_stale_results() {
    let dir = temp_completion_dir("plain");
    std::fs::write(dir.join("alpha.txt"), "x").unwrap();
    std::fs::write(dir.join("beta.txt"), "x").unwrap();

    let mut manager = BuiltinCommandManager::new();
    manager.set_local_cwd(Some(dir.clone()));
    manager.update("/open ");
    let first = manager.pending_local_request().expect("local request");
    assert_eq!(first.context, LocalCompletionContext::Plain);
    assert_eq!(first.cwd.as_deref(), Some(dir.as_path()));
    assert!(manager.is_loading_local);
    assert!(
        manager.path_completions.iter().any(|i| i.is_placeholder()),
        "读取期间应显示占位项"
    );

    manager.update("/open a");
    let second = manager.pending_local_request().expect("second request");
    assert_ne!(first.id, second.id);

    // 旧代次的结果必须丢弃。
    manager.apply_local_completion_result(
        first.id,
        vec![PathCompletionItem {
            name: "stale.txt".to_string(),
            path: dir.join("stale.txt"),
            is_directory: false,
            kind: CompletionKind::File,
            description: None,
            icon: None,
            replacement: None,
        }],
    );
    assert!(
        !manager
            .path_completions
            .iter()
            .any(|i| i.name == "stale.txt"),
        "过期结果不得覆盖当前补全"
    );

    let items = PathCompletionEngine::complete(&second.prefix, second.cwd.as_deref());
    manager.apply_local_completion_result(second.id, items);
    assert!(
        manager
            .path_completions
            .iter()
            .any(|i| i.name == "alpha.txt"),
        "当前代次的结果应写入列表"
    );
    assert!(!manager.is_loading_local);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn scp_local_path_completion_is_async_with_host_prefix() {
    let dir = temp_completion_dir("scp-local");
    std::fs::write(dir.join("remote.txt"), "x").unwrap();

    let mut manager = BuiltinCommandManager::new();
    manager.set_ssh_configs(vec![]);
    manager.set_local_cwd(Some(dir.clone()));
    manager.update(&format!("/scp Local:{}/", dir.display()));
    let request = manager.pending_local_request().expect("local request");
    assert_eq!(
        request.context,
        LocalCompletionContext::ScpLocalPath {
            host_token: "Local".to_string()
        }
    );
    assert!(
        manager.path_completions.iter().any(|i| i.is_placeholder()),
        "读取期间应显示占位项"
    );

    let items = PathCompletionEngine::complete(&request.prefix, request.cwd.as_deref());
    manager.apply_local_completion_result(request.id, items);
    let hit = manager
        .path_completions
        .iter()
        .find(|i| i.name == "remote.txt")
        .expect("local file should be listed");
    assert!(
        hit.replacement
            .as_deref()
            .is_some_and(|r| r.starts_with("Local:")),
        "补全 token 应保留 host 前缀"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn scp_host_or_local_appends_local_files_to_host_candidates() {
    let dir = temp_completion_dir("scp-host-local");
    std::fs::write(dir.join("local-file.txt"), "x").unwrap();

    let mut manager = BuiltinCommandManager::new();
    manager.set_ssh_configs(vec![]);
    manager.set_local_cwd(Some(dir.clone()));
    manager.update("/scp ");
    let request = manager.pending_local_request().expect("local request");
    assert_eq!(request.context, LocalCompletionContext::ScpHostOrLocal);

    let items = PathCompletionEngine::complete(&request.prefix, request.cwd.as_deref());
    manager.apply_local_completion_result(request.id, items);
    assert!(
        manager
            .path_completions
            .iter()
            .any(|i| i.name == "local-file.txt"),
        "主机候选之后应追加本机文件"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
