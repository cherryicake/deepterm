//! 远端（SSH）LSP 的真机冒烟测试。
//!
//! 这些测试要连一台真实的 SSH 主机，**默认跳过**。设两个环境变量才跑：
//!
//! ```text
//! DEEPTERM_LSP_REMOTE_HOST=192.168.194.4
//! DEEPTERM_LSP_REMOTE_USER=zhousong
//! ```
//!
//! 认证走与终端连接相同的路径（`russh_terminal::connect_and_authenticate`），
//! 所以主机必须已经在本机的 SSH 配置里（密钥或 agent 可用）。缺失时静默
//! 跳过——CI 上没有这台机器不该让构建变红。
//!
//! 覆盖的是「传输层真的能把服务器跑起来」：PATH 探测、channel exec、
//! 两套 trait 家族的桥接、initialize 握手、didOpen 之后服务器推诊断。
//! 单测里的替身传输验证不了这些。

use std::path::PathBuf;
use std::time::Duration;

use lsp_types::Url;

use crate::lsp::client::LspClient;

/// 测试主机配置；未配置环境变量时为 `None`（测试跳过）。
///
/// `pub(crate)`：`remote_editor_tests` 的另一组真机用例共用这一份口径
/// （含 `DEEPTERM_LSP_REMOTE_PORT`）。
pub(crate) fn remote_config() -> Option<crate::ssh::SshConnectionConfig> {
    let host = std::env::var("DEEPTERM_LSP_REMOTE_HOST").ok()?;
    let username = std::env::var("DEEPTERM_LSP_REMOTE_USER").unwrap_or_else(|_| "root".into());
    Some(
        crate::ssh::SshConnectionConfig::new(host, username, crate::ssh::AuthMethod::Auto)
            .with_port(
                std::env::var("DEEPTERM_LSP_REMOTE_PORT")
                    .ok()
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(22),
            ),
    )
}

/// 等待条件成立，超时返回 false。
async fn wait_for<F>(mut cond: F, timeout: Duration) -> bool
where
    F: FnMut() -> bool,
{
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if cond() {
            return true;
        }
        smol::Timer::after(Duration::from_millis(200)).await;
    }
    cond()
}

/// 排空通知，找针对 `uri` 的非空 `publishDiagnostics`。
///
/// 两条端到端用例（rust-analyzer / ruff）共用同一个判定，只差超时。
/// `pub(crate)`：`remote_editor_tests` 里「菜单 Start 之后标签拿到诊断」
/// 那条走的是管理器的复用路径，判据与这里完全相同。
pub(crate) fn any_diagnostics_for(rx: &crate::lsp::jsonrpc::NotificationSub, uri: &Url) -> bool {
    while let Ok(msg) = rx.try_recv() {
        if let crate::lsp::jsonrpc::Message::Notification { method, params } = msg
            && method == "textDocument/publishDiagnostics"
            && let Some(p) = crate::lsp::client::as_publish_diagnostics(&params)
            && crate::lsp::client::uri_eq(&p.uri, uri)
            && !p.diagnostics.is_empty()
        {
            return true;
        }
    }
    false
}

/// 远端 PATH 探测：rust-analyzer 在测试机上只存在于登录 shell 的 PATH
/// （`~/.cargo/bin`），非登录 shell 里按名字找不到。这条用例守的就是
/// 「两级探测」里的第二级真的有效。
#[test]
fn resolves_binary_only_visible_in_login_shell() {
    let Some(config) = remote_config() else {
        eprintln!("skipping: DEEPTERM_LSP_REMOTE_HOST not set");
        return;
    };
    let resolved = crate::ssh::lsp_channel::resolve_remote_binary(&config, "rust-analyzer")
        .expect("rust-analyzer should resolve on the test host");
    assert!(
        resolved.is_absolute(),
        "resolved path must be absolute, got {resolved:?}"
    );
}

/// 找不到的命令必须给出一条能指出病因的错误，而不是笼统的「启动失败」。
#[test]
fn missing_binary_names_the_host() {
    let Some(config) = remote_config() else {
        eprintln!("skipping: DEEPTERM_LSP_REMOTE_HOST not set");
        return;
    };
    let err = crate::ssh::lsp_channel::resolve_remote_binary(
        &config,
        "deepterm-definitely-not-a-real-server",
    )
    .expect_err("a nonexistent command must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains(&config.username) && msg.contains(&config.host),
        "error must name user@host so the user knows which machine, got: {msg}"
    );
}

/// 远端 rust-analyzer：握手 → didOpen → 收到诊断。
///
/// 这条是端到端的核心断言：它同时验证 PATH 探测、channel exec、
/// trait 家族桥接与协议链路。任何一环断掉都到不了「收到诊断」。
#[test]
fn remote_rust_analyzer_reports_diagnostics() {
    let Some(config) = remote_config() else {
        eprintln!("skipping: DEEPTERM_LSP_REMOTE_HOST not set");
        return;
    };
    // 远端临时目录：不能用本地的，服务器的 rootUri 指向的是远端路径。
    let remote_dir = format!("/tmp/deepterm-lsp-remote-it-{}", std::process::id());
    smol::block_on(async {
        // 建目录 + 写一个有确定错误的文件（未定义变量）。
        let setup = crate::ssh::sftp_fs::with_cached_session(&config, |session| {
            session.execute_command_bytes(
                &format!(
                    "mkdir -p {d}/src && printf 'fn main() {{\\n    undefined_variable();\\n}}\\n' > {d}/src/main.rs && printf '[package]\\nname = \"it\"\\nversion = \"0.1.0\"\\nedition = \"2021\"\\n' > {d}/Cargo.toml",
                    d = remote_dir
                ),
                30,
            )
        })
        .expect("setup commands should run");
        assert!(setup.success(), "remote setup failed: {}", setup.stderr);

        let root = PathBuf::from(&remote_dir);
        let client = LspClient::start_remote(&config, "rust-analyzer", &[], root, None)
            .await
            .expect("remote rust-analyzer should start");

        assert!(!client.is_closed(), "server died right after handshake");
        let name = client
            .capabilities()
            .server_info
            .as_ref()
            .map(|i| i.name.clone());
        assert_eq!(name.as_deref(), Some("rust-analyzer"), "got {name:?}");

        let file = format!("{remote_dir}/src/main.rs");
        let uri = Url::from_file_path(&file).unwrap();
        let src = "fn main() {\n    undefined_variable();\n}\n";
        client
            .did_open(&uri, "rust", 0, src)
            .await
            .expect("didOpen over the channel");

        // rust-analyzer 冷启动要跑 cargo metadata，给足 60s（远端更慢）。
        let rx = client.notifications();
        let got = wait_for(|| any_diagnostics_for(&rx, &uri), Duration::from_secs(60)).await;

        client.shutdown();
        // 清理远端临时目录：失败也无所谓，别让清理错误盖掉真正的断言。
        let _ = crate::ssh::sftp_fs::with_cached_session(&config, |session| {
            session.execute_command_bytes(&format!("rm -rf {remote_dir}"), 30)
        });

        assert!(
            got,
            "remote rust-analyzer published no diagnostics for {file}"
        );
    });
}

/// ruff 的 LSP：握手 → didOpen → 收到诊断。
///
/// Python 这条单独测，因为它的服务器形态与 rust-analyzer / clangd 都不同：
/// ruff 是单文件二进制，**不需要项目根**，也不需要跑 `cargo metadata` 之类的
/// 外部工具链。这正好覆盖「服务器本身轻量、依赖少」那一类。
///
/// **这里把命令写死成 `ruff`**，所以它验的是「ruff 能被远端跑起来」，
/// 不是「候选链会选中 ruff」——链选错命令时这条用例照样绿。选命令的口径
/// 由 `remote_python_chain_resolves_to_something_installed` 守。
#[test]
fn remote_ruff_reports_diagnostics() {
    let Some(config) = remote_config() else {
        eprintln!("skipping: DEEPTERM_LSP_REMOTE_HOST not set");
        return;
    };
    // ruff 不要求项目结构：一个裸 .py 文件就够。
    let remote_dir = format!("/tmp/deepterm-lsp-ruff-it-{}", std::process::id());
    // 未使用的 import（F401）+ 未定义的变量（F821）。
    let src = "import os\nimport sys\n\n\ndef f():\n    return undefined_name\n";
    smol::block_on(async {
        // 文件内容经 stdin 交给 cat：`execute_command_bytes` 不发 stdin，
        // 所以不能用 printf/heredoc 那套，改用「命令本身带内容」的形式。
        // 内容里只有换行与 ASCII，单引号包裹即可，无需转义。
        let setup = crate::ssh::sftp_fs::with_cached_session(&config, |session| {
            session.execute_command_bytes(
                &format!(
                    "mkdir -p {d} && printf '%s' '{src}' > {d}/m.py",
                    d = remote_dir
                ),
                30,
            )
        })
        .expect("setup commands should run");
        assert!(setup.success(), "remote setup failed: {}", setup.stderr);

        let client = LspClient::start_remote(
            &config,
            "ruff",
            &["server".to_string()],
            PathBuf::from(&remote_dir),
            None,
        )
        .await
        .expect("remote ruff should start");

        assert!(!client.is_closed(), "server died right after handshake");
        let name = client
            .capabilities()
            .server_info
            .as_ref()
            .map(|i| i.name.clone());
        assert_eq!(name.as_deref(), Some("ruff"), "got {name:?}");

        let file = format!("{remote_dir}/m.py");
        let uri = Url::from_file_path(&file).unwrap();
        client
            .did_open(&uri, "python", 0, src)
            .await
            .expect("didOpen over the channel");

        let rx = client.notifications();
        let got = wait_for(|| any_diagnostics_for(&rx, &uri), Duration::from_secs(30)).await;

        client.shutdown();
        let _ = crate::ssh::sftp_fs::with_cached_session(&config, |session| {
            session.execute_command_bytes(&format!("rm -rf {remote_dir}"), 30)
        });

        assert!(got, "remote ruff published no diagnostics for {file}");
    });
}

/// 候选链在这台主机上必须挑出一个**真的装了的** Python 服务器。
///
/// 守的是实际故障：链里写了 `python-lsp-server`，那是 pip 包名，入口其实叫
/// `pylsp`；于是这台装了 pylsp 的主机被判定「没有 python-lsp-server」，链
/// 静默退到 ruff——用户拿到只有 lint、没有类型检查的降级结果，而 ruff 那条
/// 用例（写死命令名）完全看不出来。basedpyright 同理：包名与入口不同名。
#[test]
fn remote_python_chain_resolves_to_something_installed() {
    let Some(config) = remote_config() else {
        eprintln!("skipping: DEEPTERM_LSP_REMOTE_HOST not set");
        return;
    };
    // 只问「有没有」，不看全局开关，所以 enabled 在这里不影响结果；
    // 传 true 是为了与状态栏/菜单同一个入口口径。
    let settings = crate::lsp::registry::LspContent {
        enabled: true,
        servers: Default::default(),
    };
    let picked = crate::lsp::registry::server_for_language_on(
        "python",
        Some(&settings),
        Some(&config),
        true,
    )
    .expect("python must have a server config");

    // 挑中的那个必须真能在那台机器上解析出来。「一个都没装时退回链首」
    // 是设计行为（为了让失败信息指名道姓），所以只有解析成功才算「装了的」。
    assert!(
        crate::ssh::lsp_channel::resolve_remote_binary(&config, &picked.command).is_ok(),
        "候选链挑中的 `{}` 在 {}@{} 上不存在——说明链里的名字与远端入口对不上",
        picked.command,
        config.username,
        config.host
    );

    // 逐个点名：这些可执行文件名必须能在这台机器上探到。
    // 名字写回成包名（`python-lsp-server`）时这里会失败，而链那条断言不会——
    // 它会退回下一个候选照样「成功」。
    for name in ["pylsp", "basedpyright-langserver"] {
        assert!(
            crate::ssh::lsp_channel::resolve_remote_binary(&config, name).is_ok(),
            "`{name}` 探不到：候选链里的可执行文件名写错了（包名 ≠ 入口名）"
        );
    }
}

/// basedpyright（pyright 分支）经远端 channel 跑通：握手 → didOpen → 诊断。
///
/// 有类型检查的 Python 服务器与 ruff 的差别在于它**重**：要起 node 运行时的
/// 等价物（这里是 uv tool 的独立 venv python）、要读项目结构、冷启动慢。
/// 只测 ruff 覆盖不到这一类，所以候选链把类型检查服务器排到前面之后，
/// 必须有这条用例证明远端真能把更重的那个跑起来。
#[test]
fn remote_basedpyright_reports_diagnostics() {
    let Some(config) = remote_config() else {
        eprintln!("skipping: DEEPTERM_LSP_REMOTE_HOST not set");
        return;
    };
    // `start_remote` 自己会解析路径，这里探测只是为了「没装就跳过」。
    if crate::ssh::lsp_channel::resolve_remote_binary(&config, "basedpyright-langserver").is_err() {
        eprintln!("skipping: basedpyright-langserver not installed on the test host");
        return;
    }
    let remote_dir = format!("/tmp/deepterm-lsp-pyright-it-{}", std::process::id());
    // 未定义变量：pyright 的 reportUndefinedVariable。
    let src = "def f():\n    return undefined_name\n";
    smol::block_on(async {
        let setup = crate::ssh::sftp_fs::with_cached_session(&config, |session| {
            session.execute_command_bytes(
                &format!(
                    "mkdir -p {d} && printf '%s' '{src}' > {d}/m.py",
                    d = remote_dir
                ),
                30,
            )
        })
        .expect("setup commands should run");
        assert!(setup.success(), "remote setup failed: {}", setup.stderr);

        let client = LspClient::start_remote(
            &config,
            "basedpyright-langserver",
            &["--stdio".to_string()],
            PathBuf::from(&remote_dir),
            None,
        )
        .await
        .expect("remote basedpyright should start");

        assert!(!client.is_closed(), "server died right after handshake");

        let file = format!("{remote_dir}/m.py");
        let uri = Url::from_file_path(&file).unwrap();
        client
            .did_open(&uri, "python", 0, src)
            .await
            .expect("didOpen over the channel");

        let rx = client.notifications();
        // 冷启动比 ruff 慢得多（要建分析员、扫库），给足余量。
        let got = wait_for(|| any_diagnostics_for(&rx, &uri), Duration::from_secs(90)).await;

        client.shutdown();
        let _ = crate::ssh::sftp_fs::with_cached_session(&config, |session| {
            session.execute_command_bytes(&format!("rm -rf {remote_dir}"), 30)
        });

        assert!(
            got,
            "remote basedpyright published no diagnostics for {file}"
        );
    });
}
