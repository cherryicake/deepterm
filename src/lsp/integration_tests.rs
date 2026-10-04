//! LSP 客户端的集成冒烟测试：用**真实的语言服务器**跑通握手与诊断。
//!
//! 这些测试需要本机装有对应的服务器（rust-analyzer / clangd）。缺失时
//! 测试会**跳过**而不是失败——CI 上没有这些二进制不该让构建变红。
//!
//! 覆盖的是「协议链路真的通」：进程能起、initialize 握手能完成、
//! didOpen 之后服务器真的会推诊断。单靠单元测试（mock 帧）验证不到这些。

use std::path::PathBuf;
use std::time::Duration;

use lsp_types::Url;
use ropey::Rope;

use crate::lsp::client::LspClient;
use crate::lsp::features;

/// 服务器可执行文件名 → 是否存在于 PATH。
fn has_server(name: &str) -> bool {
    std::process::Command::new(name)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

/// 临时目录里搭一个最小 Cargo 项目。
///
/// rust-analyzer 需要 `Cargo.toml` 才会真正分析；给个裸 `.rs` 文件它
/// 会当成游离文件，诊断行为不同（可能直接不给）。
fn rust_project(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("deepterm-lsp-it-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"it\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    dir
}

/// 等待条件成立，超时返回 false。
///
/// 语言服务器是异步的：didOpen 之后诊断不会立刻到（rust-analyzer 要先
/// 跑一遍 cargo metadata，冷启动可能好几秒）。
async fn wait_for<F>(mut cond: F, timeout: Duration) -> bool
where
    F: FnMut() -> bool,
{
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if cond() {
            return true;
        }
        smol::Timer::after(Duration::from_millis(100)).await;
    }
    cond()
}

/// rust-analyzer：握手 → didOpen → 收到诊断。
#[test]
fn rust_analyzer_reports_diagnostics() {
    if !has_server("rust-analyzer") {
        eprintln!("skipping: rust-analyzer not installed");
        return;
    }
    let dir = rust_project("ra-diag");
    // 这段代码有两个确定的错误：未定义变量 + 类型不匹配。
    let src = "fn main() {\n    let x: i32 = \"not a number\";\n    undefined_variable();\n}\n";
    let file = dir.join("src").join("main.rs");
    std::fs::write(&file, src).unwrap();

    smol::block_on(async {
        let client = LspClient::start("rust-analyzer", &[], dir.clone())
            .await
            .expect("rust-analyzer should start");

        assert!(!client.is_closed(), "server died right after handshake");
        // 握手结果里必须有服务器名——证明我们真的读到了它的能力。
        let name = client
            .capabilities()
            .server_info
            .as_ref()
            .map(|i| i.name.clone());
        assert_eq!(name.as_deref(), Some("rust-analyzer"), "got {name:?}");

        let uri = Url::from_file_path(&file).unwrap();
        client
            .did_open(&uri, "rust", 0, src)
            .await
            .expect("didOpen");

        // 等诊断。rust-analyzer 冷启动慢，给足 30s。
        let rx = client.notifications();
        let got = wait_for(
            || {
                while let Ok(msg) = rx.try_recv() {
                    if let crate::lsp::jsonrpc::Message::Notification { method, params } = msg {
                        if method == "textDocument/publishDiagnostics"
                            && let Some(p) = crate::lsp::client::as_publish_diagnostics(&params)
                            && p.uri == uri
                            && !p.diagnostics.is_empty()
                        {
                            return true;
                        }
                    }
                }
                false
            },
            Duration::from_secs(30),
        )
        .await;

        assert!(got, "rust-analyzer published no diagnostics for {file:?}");
        client.shutdown();
    });
}

/// clangd：握手 → didOpen → 收到诊断。
///
/// 与 rust-analyzer 分开测是为了确认客户端不依赖某个特定服务器的行为
/// （两个服务器在 didOpen 后推诊断的时机、能力声明都不一样）。
#[test]
fn clangd_reports_diagnostics() {
    if !has_server("clangd") {
        eprintln!("skipping: clangd not installed");
        return;
    }
    let dir = std::env::temp_dir().join("deepterm-lsp-it-clangd");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // 调用一个不存在的函数：clangd 一定会报错。
    let src = "int main() {\n    this_function_does_not_exist();\n    return 0;\n}\n";
    let file = dir.join("main.cpp");
    std::fs::write(&file, src).unwrap();

    smol::block_on(async {
        let client = LspClient::start("clangd", &[], dir.clone())
            .await
            .expect("clangd should start");

        let uri = Url::from_file_path(&file).unwrap();
        client.did_open(&uri, "cpp", 0, src).await.expect("didOpen");

        let rx = client.notifications();
        let got = wait_for(
            || {
                while let Ok(msg) = rx.try_recv() {
                    if let crate::lsp::jsonrpc::Message::Notification { method, params } = msg {
                        if method == "textDocument/publishDiagnostics"
                            && let Some(p) = crate::lsp::client::as_publish_diagnostics(&params)
                            && p.uri == uri
                            && !p.diagnostics.is_empty()
                        {
                            return true;
                        }
                    }
                }
                false
            },
            Duration::from_secs(30),
        )
        .await;

        assert!(got, "clangd published no diagnostics");
        client.shutdown();
    });
}

/// 诊断能换算成编辑器的字节区间，且落在文本范围内。
#[test]
fn real_diagnostics_convert_to_byte_ranges() {
    // 不依赖服务器：直接构造一条 rust-analyzer 风格的诊断，验证换算。
    let rope = Rope::from("fn main() {\n    let x = 1;\n}\n");
    let d = lsp_types::Diagnostic {
        range: lsp_types::Range::new(
            lsp_types::Position::new(1, 8),
            lsp_types::Position::new(1, 9),
        ),
        message: "unused variable: `x`".into(),
        severity: Some(lsp_types::DiagnosticSeverity::WARNING),
        source: Some("rust-analyzer".into()),
        ..Default::default()
    };
    let out = features::to_diagnostics(&rope, vec![d]);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].severity, crate::editor::syntax::DiagnosticSeverity::Warning);
    // 区间必须落在文本内且在字符边界上——`DiagnosticSet::set` 依赖这一点。
    assert!(out[0].range.end <= rope.len());
    assert!(rope.is_char_boundary(out[0].range.start));
    assert!(rope.is_char_boundary(out[0].range.end));
    // (1,8) 是第二行第 8 个字符 → "    let x" 里的 'x'。
    assert_eq!(&rope.slice(out[0].range.clone()).to_string(), "x");
}

/// 服务器不存在时启动失败，且失败是可恢复的 `Err`（不 panic）。
#[test]
fn missing_server_fails_gracefully() {
    smol::block_on(async {
        let result = LspClient::start(
            "definitely-not-a-real-language-server-xyz",
            &[],
            std::env::temp_dir(),
        )
        .await;
        assert!(result.is_err(), "spawning a missing binary must fail");
    });
}
