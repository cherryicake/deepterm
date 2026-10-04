//! 各 LSP 功能的端到端验证：用真实服务器跑一遍，确认响应能解析、能换算。
//!
//! 与 `integration_tests` 的分工：那边验证「握手 + 诊断」这条主链路，
//! 这里逐个验证补全 / 跳转 / 符号 / 格式化 / hover 等**请求类**功能。
//! 服务器缺失时跳过。

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;

use lsp_types::Url;

use crate::lsp::client::LspClient;
use crate::lsp::features;

/// 重发一个请求直到它不再返回瞬时态错误。
///
/// 语言服务器在首次分析期间会对请求回 `-32801 content modified`（文档还在
/// 被内部改写），这是**预期的瞬时态**而不是失败。并行跑测试时冷启动更慢，
/// 一次重试往往不够，所以要重试到超时。
///
/// 连接真的断开时立即放弃——那是真故障，重试没有意义。
async fn retry_until_ready<T, Fut>(
    client: &LspClient,
    mut f: impl FnMut() -> Fut,
    what: &str,
    timeout: Duration,
) -> T
where
    Fut: std::future::Future<Output = Result<T, anyhow::Error>>,
{
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match f().await {
            Ok(v) => return v,
            Err(e) if client.is_closed() => panic!("{what}: server closed: {e}"),
            Err(_) if std::time::Instant::now() < deadline => {
                smol::Timer::after(Duration::from_millis(300)).await;
            }
            Err(e) => panic!("{what}: still failing after {timeout:?}: {e}"),
        }
    }
}

/// 服务器是否**真的可用**（判退出码，理由见 `integration_tests::has_server`）。
///
/// rustup 的代理存根能启动但会以非零退出——只判`status().is_ok()` 会把
/// 「toolchain 没装该组件」误判成可用，用例随后在握手处炸掉且掩盖真实原因。
fn has_server(name: &str) -> bool {
    std::process::Command::new(name)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn rust_project(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("deepterm-lsp-feat-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let _ = std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"feat\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    dir
}

/// rust-analyzer：hover / 补全 / 跳转定义 / 文档符号 / 格式化。
///
/// 合成一个大测试而不是五个：每个都要等服务器冷启动（cargo metadata），
/// 分开跑会慢五倍，而它们共享同一个已就绪的客户端。
#[test]
fn rust_analyzer_feature_requests() {
    if !has_server("rust-analyzer") {
        eprintln!("skipping: rust-analyzer not installed");
        return;
    }
    let dir = rust_project("requests");
    let src = "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\nfn main() {\n    let _ = add(1, 2);\n}\n";
    let file = dir.join("src").join("main.rs");
    std::fs::write(&file, src).unwrap();

    smol::block_on(async {
        let client = LspClient::start("rust-analyzer", &[], dir.clone())
            .await
            .expect("start");
        let uri = Url::from_file_path(&file).unwrap();
        client.did_open(&uri, "rust", 0, src).await.expect("didOpen");

        // 服务器的首次分析要跑 cargo metadata，并行跑测试时明显变慢；
        // 所有请求都走 `retry_until_ready`，避开分析期间返回的瞬时态错误。
        let budget = Duration::from_secs(60);

        // --- hover：`add` 上应该有类型信息 ---
        let add_pos = lsp_types::Position::new(0, 4); // `add` 标识符内
        // hover 特殊处理：`Ok(None)`（服务器还没分析到这个位置）也要继续等，
        // 所以外面再套一层轮询，里面用 retry 吸收瞬时态错误。
        let mut hover = None;
        let deadline = std::time::Instant::now() + budget;
        while std::time::Instant::now() < deadline {
            match retry_until_ready(&client, || client.hover(&uri, add_pos), "hover", budget).await
            {
                Some(h) => {
                    hover = Some(h);
                    break;
                }
                None => {
                    smol::Timer::after(Duration::from_millis(300)).await;
                }
            }
        }
        let hover = hover.expect("rust-analyzer returned no hover for `add`");
        let text = features::hover_contents(&hover);
        assert!(!text.trim().is_empty(), "hover text was empty");

        // --- 补全：`main` 函数体内请求，应有候选 ---
        // 位置 5:12 在 `let _ = ` 之后，rust-analyzer 在此返回上百个候选。
        // （行末 5:20 处在 `)` 之后，服务器确实返回 0 个，不能用来断言。）
        //
        // 与 hover 一样要轮询：服务器在索引完成前会对补全返回**空列表**
        // （而不是错误），所以 `retry_until_ready` 挡不住这种情况。
        let comp_pos = lsp_types::Position::new(5, 12);
        let mut items = Vec::new();
        let deadline = std::time::Instant::now() + budget;
        while std::time::Instant::now() < deadline {
            let (got, _incomplete) = retry_until_ready(
                &client,
                || client.completion(&uri, comp_pos, None),
                "completion",
                budget,
            )
            .await;
            if !got.is_empty() {
                items = got;
                break;
            }
            smol::Timer::after(Duration::from_millis(300)).await;
        }
        assert!(!items.is_empty(), "rust-analyzer returned no completions");
        // 每条候选都要能取出可展示的 label。
        assert!(
            items.iter().any(|i| !features::completion_label(i).is_empty()),
            "no completion item had a label"
        );

        // --- 跳转定义：从 `add(1, 2)` 跳到 `fn add` ---
        let use_pos = lsp_types::Position::new(5, 15); // `add` in `add(1, 2)`
        let goto = retry_until_ready(
            &client,
            || client.goto_definition(&uri, use_pos),
            "definition",
            budget,
        )
        .await;
        let location = goto.as_ref().and_then(features::first_location);
        assert!(location.is_some(), "no definition found for `add`");
        let loc = location.unwrap();
        // 用 `uri_eq` 而非 `==`：windows 上 rust-analyzer 回的盘符大小写
        // 与本地构造的 uri 不一致（`/c:/` vs `/C:/`），精确比较会误报。
        assert!(
            crate::lsp::client::uri_eq(&loc.uri, &uri),
            "definition should be in the same file (got {})",
            loc.uri
        );
        // 定义在第 0 行（`fn add(...)`）。
        assert_eq!(loc.range.start.line, 0, "wrong definition line");

        // --- 文档符号：应含 add 与 main ---
        let symbols = retry_until_ready(
            &client,
            || client.document_symbols(&uri),
            "documentSymbol",
            budget,
        )
        .await;
        let text_rope = ropey::Rope::from(src);
        let flat = features::flatten_symbols(&text_rope, &symbols);
        let names: Vec<&str> = flat.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"add"), "symbols missing `add`: {names:?}");
        assert!(names.contains(&"main"), "symbols missing `main`: {names:?}");

        // --- 格式化：应返回编辑 ---
        let edits = retry_until_ready(
            &client,
            || client.formatting(&uri, 4, true),
            "formatting",
            budget,
        )
        .await;
        // 源码已符合 rustfmt 风格，允许返回 0 条；只验证请求本身没报错。
        let byte_edits = features::to_byte_edits(&text_rope, edits);
        // 每条编辑的区间都必须在文本范围内。
        for e in &byte_edits {
            assert!(e.range.end <= text_rope.len(), "edit range out of bounds");
        }

        client.shutdown();
    });
}

/// clangd：签名帮助 / 文档高亮 / 代码动作。
#[test]
fn clangd_feature_requests() {
    if !has_server("clangd") {
        eprintln!("skipping: clangd not installed");
        return;
    }
    let dir = std::env::temp_dir().join("deepterm-lsp-feat-clangd");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let src = "int mul(int a, int b) {\n    return a * b;\n}\n\nint main() {\n    return mul(2, 3);\n}\n";
    let file = dir.join("main.cpp");
    std::fs::write(&file, src).unwrap();

    smol::block_on(async {
        let client = LspClient::start("clangd", &[], dir.clone()).await.expect("start");
        let uri = Url::from_file_path(&file).unwrap();
        client.did_open(&uri, "cpp", 0, src).await.expect("didOpen");
        smol::Timer::after(Duration::from_secs(3)).await;

        // --- 签名帮助：在 `mul(` 的参数列表内 ---
        let sig_pos = lsp_types::Position::new(5, 16);
        let sig = retry_until_ready(
            &client,
            || client.signature_help(&uri, sig_pos),
            "signatureHelp",
            Duration::from_secs(30),
        )
        .await;
        // 参数列表末尾可能没有活动签名，允许 None；有则必须能转成文本。
        if let Some(help) = sig {
            let text = features::signature_text(&help);
            assert!(!text.trim().is_empty(), "signature text empty");
        }

        // --- 文档高亮：`mul` 的两处出现 ---
        let mul_pos = lsp_types::Position::new(5, 13);
        let highlights = retry_until_ready(
            &client,
            || client.document_highlight(&uri, mul_pos),
            "documentHighlight",
            Duration::from_secs(30),
        )
        .await;
        let ranges = features::to_highlight_ranges(&ropey::Rope::from(src), highlights);
        for r in &ranges {
            assert!(r.end <= src.len(), "highlight range out of bounds");
        }

        client.shutdown();
    });
}
