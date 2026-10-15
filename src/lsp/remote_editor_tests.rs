//! 远端 LSP 的**编辑器侧**端到端用例：菜单启停、跨文件跳转归属。
//!
//! 与 `remote_integration_tests` 的分工：那个验「传输层能把服务器跑起来」
//! （握手、PATH 探测、帧循环）；这里验「服务器跑起来之后，UI 侧的操作
//! 与判定都对」——这正是本次需求里「用户可手动开启/关闭」与「跨文件跳转」
//! 两条的落点。
//!
//! 同样默认跳过：需要 `DEEPTERM_LSP_REMOTE_HOST`。

use std::path::PathBuf;
use std::time::Duration;

use gpui::TestAppContext;

use crate::lsp::project::{LspManager, ServerKey, ServerState, remote_project_root};
// 测试主机配置与 `remote_integration_tests` 共用同一份（含端口环境变量）。
use crate::lsp::remote_integration_tests::remote_config;
use crate::ssh::SshConnectionConfig;

/// 起一个只装 LSP 管理器与设置存储的最小 App。
fn app(cx: &mut TestAppContext) {
    cx.update(|cx| {
        crate::settings::SettingsStore::init_for_test(cx);
        LspManager::init(cx);
    });
}

/// 该 key 属于哪台主机（panic 消息里要说清是哪台机器）。
fn host_of(key: &ServerKey) -> String {
    key.ssh_config
        .as_ref()
        .map(|c| format!("{}@{}", c.username, c.host))
        .unwrap_or_else(|| "本机".to_string())
}

/// 这个 key 的握手报文（可能为空 = 连进程都没起来）。
fn dump_log(key: &ServerKey) -> String {
    let messages = crate::lsp::log::messages(key);
    if messages.is_empty() {
        return "  （无报文：进程没有启动，多半是候选链在这台主机上选不出命令）".to_string();
    }
    messages
        .iter()
        .map(|m| format!("  {} {}", m.arrow(), m.label()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 真机上 rust-analyzer 就绪的预算。
///
/// 放宽到 180s 不是随手加的：`/tmp` 下现写的空工程冷启动要跑
/// `cargo metadata`，而这台测试机是多人共用的（`uptime` 常年 1.4~2.0），
/// 120s 曾在串行批次里实测不够、单独跑又必过。真机用例是显式开启的，
/// 宁可慢，也不要报一个「偶发没就绪」让人怀疑产品逻辑。
const READY_BUDGET: Duration = Duration::from_secs(180);

/// 诊断到达的预算（didOpen 之后）。
const DIAGNOSTIC_BUDGET: Duration = Duration::from_secs(120);

/// 等 `server_for_key*` 的结果落地，**失败时把原因一起返回**。
///
/// 三条出路必须分开，且都不 panic：
/// - `Ok(Some)` → 就绪；
/// - `Ok(None)` → **终局拒绝**（这台主机上选不出可运行的命令，或握手失败），
///   渠道此后永远是空的。把它当成「还没好」继续轮询，结果是白等满超时再报
///   一句看不出原因的话——第一版就是这么浪费了一次真机运行；
/// - 超时 / `Closed` → 见名。
///
/// 返回而不是 panic，是为了让调用方的**收尾一定跑得到**：真机用例在测试机上
/// 起了进程、写了目录，panic 会跳过清理，于是这一条用例失败之后，剩下的整批
/// 都要背着这个泄漏的 rust-analyzer 跑（实测确实连带弄坏了批次里最后一条
/// `remote_rust_analyzer_reports_diagnostics`）。
///
/// 报文全量附在返回串里：真机故障千奇百怪，重跑一遍去猜不如一次看清。
fn wait_until_ready(
    rx: async_channel::Receiver<Option<std::sync::Arc<crate::lsp::client::LspClient>>>,
    key: &ServerKey,
) -> Result<std::sync::Arc<crate::lsp::client::LspClient>, String> {
    let deadline = std::time::Instant::now() + READY_BUDGET;
    loop {
        match rx.try_recv() {
            Ok(Some(client)) => return Ok(client),
            Ok(None) => {
                return Err(format!(
                    "start rejected for {:?} on {} (root={:?}):\n{}",
                    key.language,
                    host_of(key),
                    key.root,
                    dump_log(key)
                ));
            }
            Err(async_channel::TryRecvError::Empty) => {
                if std::time::Instant::now() >= deadline {
                    return Err(format!(
                        "no result for {:?} on {} within {}s (root={:?}):\n{}",
                        key.language,
                        host_of(key),
                        READY_BUDGET.as_secs(),
                        key.root,
                        dump_log(key)
                    ));
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(async_channel::TryRecvError::Closed) => {
                return Err("start channel closed without delivering a result".to_string());
            }
        }
    }
}

/// 远端项目根探测：真连测试机，验证「有 Cargo.toml 的那一级」被找到。
///
/// 这条守的是 `ServerKey.root` 的正确性：根算错会让编辑器与服务器表
/// 用两个不同的键，表现为「起了服务器但连不上已打开的标签」。
#[gpui::test]
fn remote_project_root_finds_the_marker_directory(_cx: &mut TestAppContext) {
    let Some(config) = remote_config() else {
        eprintln!("skipping: DEEPTERM_LSP_REMOTE_HOST not set");
        return;
    };
    // 测试机上现成的 Rust 工程：`~/Documents/deepterm`。
    let path = format!("/home/{}/Documents/deepterm/src/main.rs", config.username);
    let root = remote_project_root(&config, &path);
    assert_eq!(
        root,
        PathBuf::from(format!("/home/{}/Documents/deepterm", config.username)),
        "root must be the directory holding Cargo.toml, not src/"
    );
}

/// 菜单「Start」→ 表里出现 Running；「Stop」→ 变 Stopped 且不再自动复活。
///
/// 这条走的是 `LspManager::server_for_key` / `stop_server_for_key_and_purge_logs`
/// 的真实路径（编辑器菜单的三个动作最终都落到这两个），不是 mock。
#[gpui::test]
fn menu_start_then_stop_moves_the_server_through_its_states(cx: &mut TestAppContext) {
    let Some(config) = remote_config() else {
        eprintln!("skipping: DEEPTERM_LSP_REMOTE_HOST not set");
        return;
    };
    app(cx);
    // 默认关闭：不手动开启的话 `server_for_key` 直接返回 None。
    // 这一条同时验证「默认关闭」在远端路径上也生效。
    let key = ServerKey {
        root: PathBuf::from(format!("/home/{}/Documents/deepterm", config.username)),
        language: "rust".to_string(),
        ssh_config: Some(config.clone()),
    };
    let gated = cx.update(|cx| LspManager::server_for_key(cx, &key));
    assert!(
        gated.is_none(),
        "LSP must be off by default, including for remote hosts"
    );

    // 用户按「Start」：先解标记，再用**带 override 的入口**拉起
    // （与 `Editor::start_lsp_server_for_key` 同序同参数）。
    let rx = cx.update(|cx| {
        LspManager::resume_server_for_key(cx, &key);
        LspManager::server_for_key_with_override(cx, &key, true)
            .expect("start must be accepted once un-gated")
    });

    // 握手在 `smol::spawn` 的真实线程上跑，`TestScheduler` 的假时钟推不动它
    // （advance_clock 只唤醒注册在 gpui executor 上的任务）。所以这里用**真**
    // 睡眠轮询，与 `integration_tests` 的 `wait_for` 同思路。
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    let mut saw_running = false;
    while std::time::Instant::now() < deadline {
        // channel 侧：握手完成时 `finish` 会投递结果。
        if rx.try_recv().is_ok_and(|c| c.is_some()) {
            saw_running = true;
            break;
        }
        // 表侧：以快照为准（上面的 channel 可能已被别的等待者取走）。
        let running = cx.update(|cx| {
            LspManager::snapshots(cx)
                .into_iter()
                .any(|s| s.key == key && s.state == ServerState::Running)
        });
        if running {
            saw_running = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    assert!(saw_running, "remote server never reached Running");

    // 用户按「Stop」。
    cx.update(|cx| {
        LspManager::stop_server_for_key_and_purge_logs(cx, &key);
    });
    cx.update(|cx| {
        assert!(
            LspManager::snapshots(cx)
                .into_iter()
                .all(|s| s.key != key || s.state == ServerState::Stopped),
            "stopped server must not report Running"
        );
        // 关键：停过之后不能再自动拉起。`server_for_key` 直接 None。
        assert!(
            LspManager::server_for_key(cx, &key).is_none(),
            "a user-stopped server must stay stopped"
        );
    });
}

/// 远端 `ServerKey` 与本地同名路径必须是两个不同的键。
///
/// 两台机器都有 `/home/user/project`，若键里不带主机维度，两个进程会
/// 互相顶掉——表现为「连了 B 机之后 A 机的诊断没了」。
#[gpui::test]
fn same_path_on_two_hosts_are_two_keys(_cx: &mut TestAppContext) {
    let a = SshConnectionConfig::new("10.0.0.1", "u", crate::ssh::AuthMethod::Agent);
    let b = SshConnectionConfig::new("10.0.0.2", "u", crate::ssh::AuthMethod::Agent);
    let local = ServerKey {
        root: PathBuf::from("/home/u/project"),
        language: "rust".into(),
        ssh_config: None,
    };
    let remote_a = ServerKey {
        root: PathBuf::from("/home/u/project"),
        language: "rust".into(),
        ssh_config: Some(a.clone()),
    };
    let remote_b = ServerKey {
        root: PathBuf::from("/home/u/project"),
        language: "rust".into(),
        ssh_config: Some(b.clone()),
    };
    assert_ne!(local, remote_a);
    assert_ne!(remote_a, remote_b);
    // Hash 必须与 PartialEq 同口径，否则 HashMap 会把它俩判成同一个键。
    use std::collections::HashMap;
    let mut m: HashMap<ServerKey, u8> = HashMap::new();
    m.insert(remote_a.clone(), 1);
    m.insert(remote_b.clone(), 2);
    m.insert(local.clone(), 3);
    assert_eq!(m.len(), 3, "three distinct hosts must be three keys");
    assert_eq!(m[&remote_a], 1);
    assert_eq!(m[&remote_b], 2);
}

/// **探测到 `os` 之后**，同一主机上的服务器仍是同一个键。
///
/// `os` 是连上以后才写回配置的（`workspace.rs`：`conn.os = Some(os_id)`），
/// 所以标签上的配置会在「打开文件时 None」与「起服务器时 Some("linux")」之间
/// 变。若键按整份 `SshConnectionConfig` 比较，同一个进程空间就有两把锁：
/// - `server_for_key` 查不到已在跑的进程 → 再起一个 rust-analyzer，
///   两份文档互相打脸（诊断时有时无、补全回 `-32801`）；
/// - 菜单 Start 后按 key 回接已打开标签（`tabs_for_server_key`）全部落空，
///   表现正是「服务器起来了但标签没接上」。
#[gpui::test]
fn detecting_the_os_must_not_change_the_key(_cx: &mut TestAppContext) {
    let before = SshConnectionConfig::new("10.0.0.1", "u", crate::ssh::AuthMethod::Agent);
    let mut after = before.clone();
    after.os = Some("linux".to_string());
    // 前提：整份配置的相等性确实认为两者不同（不然这条用例什么也没守）。
    assert_ne!(
        before, after,
        "test premise: `os` is part of config equality"
    );

    let root = PathBuf::from("/home/u/project");
    let early = ServerKey {
        root: root.clone(),
        language: "rust".into(),
        ssh_config: Some(before),
    };
    let late = ServerKey {
        root,
        language: "rust".into(),
        ssh_config: Some(after),
    };
    assert_eq!(early, late, "same host must stay one key");

    use std::collections::HashMap;
    let mut m: HashMap<ServerKey, u8> = HashMap::new();
    m.insert(early.clone(), 1);
    // 写回 os 之后再查必须命中同一条目——服务器表、stopped 标记、日志桶
    // 都靠这个查找。
    assert_eq!(m.get(&late), Some(&1), "the key must survive os detection");
}

/// 已经跑起来的服务器，在全局开关关闭时也要能被找到。
///
/// 回归一个真实的顺序 bug：`server_for_key` 原来先查设置、后查 `ready` 表。
/// 于是用户按「Start」起进程（授权过一次），再在同一个项目里打开**第二个**
/// 文件时，自动路径被全局开关挡在门外，看不到那个已经在跑的进程——
/// 于是又起一个。两个 rust-analyzer 各持一份文档，诊断与补全互相错。
///
/// 默认关闭的语义是「不自动起」，不是「起了也装作没起」。
#[gpui::test]
fn a_running_server_is_reused_even_when_globally_disabled(cx: &mut TestAppContext) {
    let Some(config) = remote_config() else {
        eprintln!("skipping: DEEPTERM_LSP_REMOTE_HOST not set");
        return;
    };
    app(cx);
    let key = ServerKey {
        root: PathBuf::from(format!("/home/{}/Documents/deepterm", config.username)),
        language: "rust".to_string(),
        ssh_config: Some(config.clone()),
    };

    // 确认此刻全局开关确实是关的（`init_for_test` 用默认设置）。
    let enabled = cx.update(|cx| {
        crate::settings::SettingsStore::global(cx)
            .content()
            .lsp
            .enabled
    });
    assert!(!enabled, "test premise: LSP must default to off");

    // 用户按「Start」。
    let rx = cx.update(|cx| {
        LspManager::resume_server_for_key(cx, &key);
        LspManager::server_for_key_with_override(cx, &key, true).expect("start must be accepted")
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    let mut ready = false;
    while std::time::Instant::now() < deadline {
        if rx.try_recv().is_ok_and(|c| c.is_some()) {
            ready = true;
            break;
        }
        let running = cx.update(|cx| {
            LspManager::snapshots(cx)
                .into_iter()
                .any(|s| s.key == key && s.state == ServerState::Running)
        });
        if running {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    assert!(ready, "server never became ready");
    // 放开 rx 的借用，下面要用同一个 key 再查一次。

    // **不走 override** 的普通查询（打开第二个文件走的就是这条）也必须
    // 拿到同一个进程，而不是 None、更不是新起一个。
    let reused = cx.update(|cx| LspManager::server_for_key(cx, &key));
    assert!(
        reused.is_some(),
        "a running server must be reused by the automatic path"
    );

    // 收尾：停掉，别在测试机上留进程。
    cx.update(|cx| {
        LspManager::stop_server_for_key_and_purge_logs(cx, &key);
    });
}

/// 远端 Python：候选链必须挑那台机器上**真的有**的那个。
///
/// 实测测试机 `192.168.194.4`：`~/.local/bin/` 下有 basedpyright-langserver
/// 与 ruff，两者都只出现在登录 shell 的 PATH 里。若候选链按本机 PATH 挑，
/// 本机有 pyright 时会选中它并在远端失败——而用户在自己机器上明明看得到。
///
/// 断言挑中的命令**在远端真的解析得到**，而不是钉死某个具体名字：用户在
/// 测试机上装什么会变（先只有 ruff，后来装了 basedpyright），钉死名字会让
/// 这条用例变成维护负担。真正要守的是「按目标主机挑」这一点。
#[gpui::test]
fn remote_python_candidate_chain_picks_what_the_host_has(_cx: &mut TestAppContext) {
    let Some(config) = remote_config() else {
        eprintln!("skipping: DEEPTERM_LSP_REMOTE_HOST not set");
        return;
    };
    let settings = crate::lsp::registry::LspContent {
        // 开关必须开：`server_for_language_on` 的第一道 gate 就是它。
        enabled: true,
        servers: Default::default(),
    };
    let picked = crate::lsp::registry::server_for_language_on(
        "python",
        Some(&settings),
        Some(&config),
        false,
    )
    .expect("python must have a candidate on the test host");

    assert!(
        crate::ssh::lsp_channel::resolve_remote_binary(&config, &picked.command).is_ok(),
        "picked `{}` must exist on the remote host",
        picked.command
    );
    assert!(
        crate::lsp::registry::builtin_candidates()
            .get("python")
            .is_some_and(|list| list.iter().any(|(c, _)| *c == picked.command)),
        "`{}` must be a known python candidate",
        picked.command
    );

    // 候选顺序：pyright 系（带 `--stdio`）在前，ruff 垫底（只做 lint）。
    let order: Vec<&str> = crate::lsp::registry::builtin_candidates()
        .get("python")
        .map(|list| list.iter().map(|(c, _)| *c).collect())
        .unwrap_or_default();
    assert_eq!(order.first(), Some(&"basedpyright-langserver"));
    assert_eq!(order.last(), Some(&"ruff"));
}

/// 候选链的判定必须按主机分开缓存。
///
/// 同一命令在 A 机有、B 机没有时，两次查询要给出不同答案。缓存键若不带
/// 主机维度，第一次的结果会被复用到另一台机器上——表现为「明明装了却说没装」。
#[gpui::test]
fn candidate_probe_is_cached_per_host(_cx: &mut TestAppContext) {
    let Some(config) = remote_config() else {
        eprintln!("skipping: DEEPTERM_LSP_REMOTE_HOST not set");
        return;
    };
    let settings = crate::lsp::registry::LspContent {
        enabled: true,
        servers: Default::default(),
    };
    // 同一台主机问两次：答案必须一致（第二次走缓存）。
    let first = crate::lsp::registry::server_for_language_on(
        "python",
        Some(&settings),
        Some(&config),
        false,
    );
    let second = crate::lsp::registry::server_for_language_on(
        "python",
        Some(&settings),
        Some(&config),
        false,
    );
    assert_eq!(first, second, "cached probe must be stable");
    assert!(first.is_some());

    // 一台必然连不上的主机：探测失败时不能 panic，且要退回第一个候选
    // （这样失败信息能指名道姓地说「找不到 pyright-langserver」）。
    let unreachable =
        SshConnectionConfig::new("127.0.0.1", "nobody", crate::ssh::AuthMethod::Agent).with_port(1);
    let degraded = crate::lsp::registry::server_for_language_on(
        "python",
        Some(&settings),
        Some(&unreachable),
        false,
    )
    .expect("unreachable host must still yield the first candidate");
    assert_eq!(degraded.command, "basedpyright-langserver");
}

/// 「未启动」占位的判定必须按目标主机，且一个都没装时要如实说没有。
///
/// 回归一个真实 bug：`language_has_server_on` 原来只问**本机**的 PATH
/// （`language_known` 完全不探测），于是：
/// - 本机装了 pyright、远端什么都没装 → 远端标签也显示「未启动」，
///   用户点开菜单按启动，然后在远端失败；
/// - 本机没装、远端装了 → 该显示却留白。
///
/// 这台测试机属于第二种的镜像：本机有 rust-analyzer，远端也有，但 Python
/// 只有 ruff。所以这条同时验证「按主机问」与「strict（没装就不占位）」。
#[gpui::test]
fn not_started_placeholder_follows_the_target_host(_cx: &mut TestAppContext) {
    let Some(config) = remote_config() else {
        eprintln!("skipping: DEEPTERM_LSP_REMOTE_HOST not set");
        return;
    };
    let settings = crate::lsp::registry::LspContent {
        enabled: true,
        servers: Default::default(),
    };
    // 远端有 ruff → Python 该占位。
    assert!(
        crate::lsp::registry::server_for_language_strict(
            "python",
            Some(&settings),
            Some(&config),
            true,
        )
        .is_some(),
        "the test host has ruff, so python should be offered"
    );
    // 一台必然连不上的主机：探测不到任何候选 → 不占位。
    // （若不 strict，会退回第一个候选 pyright，让用户点一个注定失败的启动。）
    let unreachable =
        SshConnectionConfig::new("127.0.0.1", "nobody", crate::ssh::AuthMethod::Agent).with_port(1);
    assert!(
        crate::lsp::registry::server_for_language_strict(
            "python",
            Some(&settings),
            Some(&unreachable),
            true,
        )
        .is_none(),
        "a host with no reachable server must not show a NotStarted chip"
    );
    // 一门没有内置候选的语言：无论哪台主机都不占位。
    assert!(
        crate::lsp::registry::server_for_language_strict(
            "brainfuck",
            Some(&settings),
            Some(&config),
            true,
        )
        .is_none(),
        "unknown languages must never occupy a status-bar slot"
    );
}

/// 全局开关关闭时，状态栏/菜单也要能报出**该接哪个服务器**。
///
/// 回归用户实际看到的症状：远程打开 `.py`，chip 上只有「未启动」三个字。
/// 那是 `configured_command_on` 被默认关闭的全局开关挡回 `None` 的结果——
/// 而这个函数回答的是「这个文件归哪个进程管」，与「现在起没起」无关。
/// 挡掉它等于同时抹掉了用户唯一的线索：不知道该接 basedpyright，
/// 也看不出点「启动」会起什么。
#[gpui::test]
fn chip_names_the_server_even_when_globally_disabled(cx: &mut TestAppContext) {
    let Some(config) = remote_config() else {
        eprintln!("skipping: DEEPTERM_LSP_REMOTE_HOST not set");
        return;
    };
    app(cx);
    // `init_for_test` 的用户文档是空的 → 合并默认 → `lsp.enabled == false`。
    cx.update(|cx| {
        let settings = crate::settings::SettingsStore::global(cx)
            .content()
            .lsp
            .clone();
        assert!(
            !settings.enabled,
            "这条用例的前提就是默认关闭，前提变了它就没意义了"
        );
        let command = LspManager::configured_command_on(cx, "python", Some(&config));
        assert_eq!(
            command.as_deref(),
            Some("basedpyright-langserver"),
            "chip 要显示那台机器上真的会起的那个服务器，而不是「未启动」或语言名"
        );
    });
}

/// 菜单 Start 之后，**同项目的另一个文件**走自动路径也要拿到诊断。
///
/// 这是 P0 那组缺陷的收口用例。前面几条各测自己那一环——chip 点得开、
/// 状态迁移对、已跑起来的进程能被复用——但没有一条把链路走完，而用户看到
/// 的现象恰恰是「灯亮了、状态栏写着服务器名，可文件里没有一条波浪线」。
///
/// 它同时守两处口径必须一致的地方：
/// - 键的主机身份（`os` 是连上后才探测写回的，标签与服务器各自的配置可能
///   停在探测的前后两侧）；
/// - 默认关闭时，自动路径仍要能落到那个已被用户授权起来的进程上，
///   而不是回 `None`（回 `None` 的表现就是接不上、没有诊断）。
#[gpui::test]
fn after_menu_start_another_file_in_the_project_gets_diagnostics(cx: &mut TestAppContext) {
    let Some(config) = remote_config() else {
        eprintln!("skipping: DEEPTERM_LSP_REMOTE_HOST not set");
        return;
    };
    app(cx);

    // 远端临时工程：自带 Cargo.toml，让 rust-analyzer 认它是一个 workspace，
    // 也让项目根探测有确定的答案。
    let dir = format!("/tmp/deepterm-lsp-attach-{}", std::process::id());
    let setup = crate::ssh::sftp_fs::with_cached_session(&config, |session| {
        session.execute_command_bytes(
            &format!(
                "mkdir -p {d}/src && printf 'fn main() {{\\n    undefined_variable();\\n}}\\n' > {d}/src/main.rs && printf '[package]\\nname = \"attach\"\\nversion = \"0.1.0\"\\nedition = \"2021\"\\n' > {d}/Cargo.toml",
                d = dir
            ),
            30,
        )
    });
    // 失败一律记账、绝不中途 panic：真机用例在测试机上起了进程、写了目录，
    // 中途 panic 就跳过了下面的收尾，泄漏的 rust-analyzer 会拖垮同一批次里
    // 后面的每一条真机用例（实测发生过：本用例超时 panic 之后，批次末尾的
    // `remote_rust_analyzer_reports_diagnostics` 连带收不到诊断）。
    let mut problems: Vec<String> = Vec::new();
    let Ok(output) = setup else {
        eprintln!("skipping: cannot reach {dir}: {:?}", setup.err());
        return;
    };
    if !output.success() {
        // 建工程失败也走正常收尾路径：此时目录可能已经建了一半，
        // 直接 panic 会跳过下面的 `rm -rf`。
        problems.push(format!("remote setup failed: {}", output.stderr));
    } else {
        eprintln!("attach: remote project ready at {dir}");
    }

    let root = PathBuf::from(&dir);
    let key = ServerKey {
        root: root.clone(),
        language: "rust".to_string(),
        ssh_config: Some(config.clone()),
    };

    // 1) 用户点开 chip、按「Start」（与 `start_lsp_server_for_key` 同序）。
    //    后面的每一步都吃这一步的产物，所以前置已经出问题就整段跳过——
    //    但**收尾不跳过**。
    let mut started: Option<std::sync::Arc<crate::lsp::client::LspClient>> = None;
    if problems.is_empty() {
        let rx = cx.update(|cx| {
            LspManager::resume_server_for_key(cx, &key);
            LspManager::server_for_key_with_override(cx, &key, true)
        });
        match rx {
            Some(rx) => match wait_until_ready(rx, &key) {
                Ok(client) => {
                    eprintln!("attach: server ready");
                    started = Some(client);
                }
                Err(why) => problems.push(why),
            },
            // 这一条正是 P0 #1 的病根：菜单的 Start 被全局默认开关挡回，
            // 用户点开 chip 却什么都启动不了。
            None => problems.push(
                "menu Start must be accepted: `server_for_key_with_override(override_disabled = true)` \
                 returned None"
                    .to_string(),
            ),
        }
    }

    // 2) 用户在同项目里打开**第二个**文件：这条走的是不带 override 的自动
    //    路径，而此刻全局开关仍是关的（`init_for_test` 的默认）。
    //    配置刻意带上探测回来的 `os`——真机上标签拿到的就是这种版本。
    let mut reused_client = None;
    if let Some(started) = &started {
        let mut detected = config.clone();
        detected.os = Some("linux".to_string());
        let second = ServerKey {
            root: root.clone(),
            language: "rust".to_string(),
            ssh_config: Some(detected),
        };
        let rx2 = cx.update(|cx| LspManager::server_for_key(cx, &second));
        let reused = match rx2 {
            Some(rx2) => match rx2.try_recv() {
                Ok(Some(c)) => Some(c),
                // 表里已经有这个进程，`ready` 分支是同步投递的，拿不到就是没接上。
                _ => None,
            },
            None => None,
        };
        match reused {
            Some(client) => {
                assert_shared_process(started, &client, &mut problems);
                reused_client = Some(client);
            }
            None => problems.push(
                "the automatic path must hand back the running server \
                 (same host, key must survive `os`)"
                    .to_string(),
            ),
        }
        if reused_client.is_some() {
            eprintln!("attach: automatic path reused the running process");
        }
    }

    // 3) 接上之后真的有诊断：didOpen 那个含未定义变量的文件。
    //    文本与远端磁盘上的一致（第 1 步刚写过去），所以这一步只是「把已打开
    //    的文档交给服务器」，不改动测试机上的任何内容。
    let mut got = false;
    if let Some(second_client) = &reused_client {
        let file = format!("{dir}/src/main.rs");
        let uri = lsp_types::Url::from_file_path(&file).unwrap();
        let src = "fn main() {\n    undefined_variable();\n}\n";
        let sub = second_client.notifications();
        // didOpen 交给 smol 全局池发，**不在本线程 block_on**：这里是
        // `#[gpui::test]` 的主线程，装着 TestDispatcher，阻塞它等于阻塞整个测试
        // 执行器，卡住时看不出卡在哪一步（真机上跑过一次，九分钟无输出）。
        // 与 `menu_start_then_stop…` 同思路：投递之后用真睡眠轮询观测点。
        let open_client = std::sync::Arc::clone(second_client);
        let open_uri = uri.clone();
        let open_file = file.clone();
        smol::spawn(async move {
            match open_client.did_open(&open_uri, "rust", 1, src).await {
                Ok(()) => eprintln!("attach: didOpen sent for {}", open_file),
                Err(e) => eprintln!("attach: didOpen FAILED for {}: {e}", open_file),
            }
        })
        .detach();
        let deadline = std::time::Instant::now() + DIAGNOSTIC_BUDGET;
        while std::time::Instant::now() < deadline {
            if crate::lsp::remote_integration_tests::any_diagnostics_for(&sub, &uri) {
                got = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        eprintln!(
            "attach: diagnostics={got} closed={}",
            second_client.is_closed()
        );
        if !got {
            problems.push(format!(
                "no diagnostics reached the reused server for {dir}/src/main.rs \
                 within {}s:\n{}",
                DIAGNOSTIC_BUDGET.as_secs(),
                dump_log(&key)
            ));
        }
    }

    // 收尾：停服务器、清临时目录，别在测试机上留进程与文件。
    // **无条件执行**，且在判定之前——判定失败也要把机器恢复原样。
    cx.update(|cx| {
        LspManager::stop_server_for_key_and_purge_logs(cx, &key);
    });
    let _ = crate::ssh::sftp_fs::with_cached_session(&config, |session| {
        session.execute_command_bytes(&format!("rm -rf {dir}"), 30)
    });

    assert!(
        problems.is_empty(),
        "attach case failed:\n{}",
        problems.join("\n---\n")
    );
}

/// 第二个文件必须落到**同一个进程**上，而不是新起一个。
///
/// 起新进程的表现正是这次要修的：两个服务器各持一份文档，诊断与补全互相打脸。
fn assert_shared_process(
    started: &std::sync::Arc<crate::lsp::client::LspClient>,
    second: &std::sync::Arc<crate::lsp::client::LspClient>,
    problems: &mut Vec<String>,
) {
    if !std::sync::Arc::ptr_eq(started, second) {
        problems
            .push("the second file must attach to the *same* process, not a new one".to_string());
    }
}
