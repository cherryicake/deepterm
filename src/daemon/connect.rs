//! `deepcore connect`：把 stdin/stdout 与 core socket 桥接。
//!
//! app 不再直接持有 Unix socket / Named Pipe，而是启动本子进程。这样本地
//! 与远端（未来通过 SSH channel 执行 `deepcore connect`）走的是同一条
//! 传输路径，app 侧只看见两根管道。
//!
//! 首帧 `hello` 由本进程补上**本 socket** 的 token；之后所有字节原样转发。
//! 这样同一个桥既能服务本地 app（它本来就会带 token），也能服务通过 SSH
//! 启动本进程的本地 core（它不可能知道远端 core 的随机 token）。
//!
//! # 约束
//!
//! stdout **只能**出现帧字节。所有日志都走 stderr 或日志文件；这里刻意
//! 不写任何提示文本，否则 app 的帧解析会在第一帧之前就错位。

use std::io::{BufReader, Read, Write};

use super::protocol::{CONTROL, Frame, Kind, read_frame};

/// 子命令名。
pub const CONNECT_COMMAND: &str = "connect";

/// 兼容显式开关写法。
const CONNECT_FLAG: &str = "--connect";

/// 这个进程是不是被要求做 stdio 桥接。
pub fn is_connect_invocation(args: &[String]) -> bool {
    args.get(1).is_some_and(|arg| arg == CONNECT_COMMAND)
        || args.iter().any(|arg| arg == CONNECT_FLAG)
}

/// 运行桥接直到 core 关闭 stdout 或本进程被结束。
///
/// 本函数不代替调用方完成握手：`hello` 的协议版本、方法与能力仍由调用方
/// 决定，只把鉴权 token 换成当前 core 的 token。
pub fn run() -> std::io::Result<()> {
    let stream = super::bootstrap::connect_to_core()?;
    // core 的 token 在 bind 之前写入；`connect_to_core` 已经能连上 socket，
    // 因此这里读不到 token 是真正的状态损坏，而不是冷启动竞态。
    let token = super::read_token_for(&super::socket_path()).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("读不到 core token: {e}"),
        )
    })?;
    let mut write_stream = super::transport::try_clone(&stream)?;

    std::thread::Builder::new()
        .name("deepcore-connect-in".into())
        .spawn(move || {
            let mut stdin = BufReader::new(std::io::stdin().lock());
            if let Err(e) = forward_with_token(&mut stdin, &mut write_stream, &token) {
                log::debug!("connect: stdin → core 结束: {e}");
            }
            // 客户端关掉写端后要通知 core 没有更多帧了。Unix 上半关闭写端，
            // 让 core 的读线程正常退出，而不是一直等到本进程被杀死。
            let _ = super::transport::shutdown_write(&write_stream);
        })?;

    let mut read_stream = stream;
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    pump(&mut read_stream, &mut stdout, true)
}

/// 读第一帧，若是 `hello` 就替换 token，然后继续原样泵送。
///
/// 使用 `BufReader` 很重要：`read_frame` 可能一次从管道读出不止一帧，直接
/// 丢掉 reader 会把后续帧一起吞掉。
fn forward_with_token(
    from: &mut (impl Read + ?Sized),
    to: &mut (impl Write + Sized),
    token: &str,
) -> std::io::Result<()> {
    let mut from = BufReader::new(from);
    if let Some(frame) = read_frame(&mut from)? {
        let frame = with_token(frame, token)?;
        frame.write_to(to)?;
    } else {
        return Ok(());
    }
    pump(&mut from, to, true)
}

/// 只改写控制面的 `hello`；其它帧保持原样，包括二进制负载。
fn with_token(frame: Frame, token: &str) -> std::io::Result<Frame> {
    if frame.channel != CONTROL || frame.kind != Kind::Json {
        return Ok(frame);
    }
    let Ok(mut request) = serde_json::from_slice::<serde_json::Value>(&frame.payload) else {
        return Ok(frame);
    };
    if request.get("op").and_then(serde_json::Value::as_str) != Some("hello") {
        return Ok(frame);
    }
    let Some(object) = request.as_object_mut() else {
        return Ok(frame);
    };
    object.insert("token".into(), serde_json::Value::String(token.into()));
    Ok(Frame::json(serde_json::to_vec(&request)?))
}

/// 读一块、写一块、立刻 flush。
///
/// 不能用 `std::io::copy`：stdout 连接管道时可能带块缓冲，握手回复只有
/// 一两百字节，不 flush 就会一直卡到缓冲区满。
fn pump(
    from: &mut (impl Read + ?Sized),
    to: &mut (impl Write + ?Sized),
    flush: bool,
) -> std::io::Result<()> {
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        to.write_all(&buf[..n])?;
        if flush {
            to.flush()?;
        }
    }
    to.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_invocation_is_recognised_without_matching_other_words() {
        assert!(is_connect_invocation(&[
            "deepcore".into(),
            "connect".into()
        ]));
        assert!(is_connect_invocation(&[
            "deepcore".into(),
            "--connect".into()
        ]));
        assert!(!is_connect_invocation(&["deepcore".into()]));
        assert!(!is_connect_invocation(&[
            "deepcore".into(),
            "disconnect".into()
        ]));
    }

    #[test]
    fn pump_preserves_binary_bytes() {
        let input = b"\x00\x01hello\n\xff".to_vec();
        let mut out = Vec::new();
        pump(&mut input.as_slice(), &mut out, true).expect("pump");
        assert_eq!(out, input);
    }

    #[test]
    fn the_first_hello_gets_the_core_token_and_later_bytes_are_untouched() {
        let hello = Frame::json(
            serde_json::to_vec(&serde_json::json!({
                "id": 7, "op": "hello", "protocol": 2, "token": "from-caller"
            }))
            .unwrap(),
        );
        let data = Frame::data(3, b"raw\x00bytes".to_vec());
        let index = {
            let mut first = Frame::json(
                serde_json::to_vec(&serde_json::json!({"id": 8, "op": "list"})).unwrap(),
            );
            first.payload[0] = b'{';
            first
        };
        let mut input = Vec::new();
        hello.write_to(&mut input).unwrap();
        data.write_to(&mut input).unwrap();
        index.write_to(&mut input).unwrap();

        let mut out = Vec::new();
        forward_with_token(&mut input.as_slice(), &mut out, "remote-token").unwrap();

        let mut cursor = std::io::Cursor::new(out);
        let parsed = read_frame(&mut cursor).unwrap().unwrap();
        let value: serde_json::Value = serde_json::from_slice(&parsed.payload).unwrap();
        assert_eq!(value["token"], "remote-token");
        assert_eq!(value["protocol"], 2);
        assert_eq!(read_frame(&mut cursor).unwrap(), Some(data));
        assert_eq!(read_frame(&mut cursor).unwrap(), Some(index));
        assert!(read_frame(&mut cursor).unwrap().is_none());
    }
}
