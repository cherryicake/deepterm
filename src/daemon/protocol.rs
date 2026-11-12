//! 终端守护进程的 IPC 协议。
//!
//! 守护进程（同可执行文件的隐藏子命令 `deepterm __daemon`）持有所有 PTY，
//! UI 进程通过 Unix socket / Named Pipe 挂接上去。窗口关掉时守护进程继续跑，
//! 终端里跑的程序因此不受影响；重开 deepterm 时挂接回去即可。
//!
//! # 帧格式
//!
//! 每条帧是 **9 字节头 + 负载**，与 herdr-core 的 framed 模式同构：
//!
//! ```text
//! ┌────────────┬──────────┬────────────┬──────────┐
//! │ channel u32│ kind u8  │ len u32    │ payload  │
//! │ 小端 4 字节 │ 1 字节   │ 小端 4 字节 │ len 字节 │
//! └────────────┴──────────┴────────────┴──────────┘
//! ```
//!
//! 分 channel 的意义：一个连接上既走控制请求（channel 0 的 JSON），
//! 又走各终端的字节流（每个终端自己的 channel）。终端输出是二进制，
//! 塞进 JSON 要么 base64 膨胀 33%，要么每帧都做一次转义；分帧后
//! 输出路径上没有任何编解码。
//!
//! # 为什么不用 JSON Lines 一条流
//!
//! herdr 自己就是这么分的（`src/hc.rs` 的 `DATA` / `CREDIT` / `CLOSE` /
//! `RESIZE`）。终端输出是热路径（一个 `yes` 就能打满 CPU），
//! 结构化的控制面与二进制的数据面分开，各自都能保持简单。

use std::io::{self, Read, Write};

/// 帧头长度：channel(4) + kind(1) + len(4)。
pub const HEADER_LEN: usize = 9;

/// 单帧负载上限（16 MiB）。
///
/// 超过它说明对面的帧头已经乱了（或者有人在喂垃圾），继续按照长度
/// 去分配内存只会把进程撑爆。终端一次输出远达不到这个量级：
/// kitty 图形的单条命令上限是 [`crate::terminal::kitty::command::MAX_COMMAND_BYTES`]，
/// 而 PTY 读缓冲是 64 KiB 一块。
pub const MAX_PAYLOAD: usize = 16 * 1024 * 1024;

/// 尺寸帧的负载长度：cols(2) + rows(2) + cell_w(2) + cell_h(2)。
///
/// cell 尺寸用 `u16` 而不是 `u32`：`tty::WindowSize` 的
/// `cell_width` / `cell_height` 就是 `u16`（它最终落到 `winsize` 的
/// `ws_xpixel` / `ws_ypixel`，内核里也是 16 位），协议照抄即可，
/// 不必自己宽一号再窄回来。
pub const RESIZE_PAYLOAD_LEN: usize = 8;

/// 控制请求/回复固定走 channel 0。
pub const CONTROL: u32 = 0;

/// 帧类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// 负载是 JSON 文本（控制面）。只在 channel 0 上出现。
    Json = 0,
    /// 负载是原始字节（终端输入/输出）。
    Data = 1,
    /// 通道关闭，负载是 JSON 描述原因。
    Close = 2,
    /// 尺寸变更，负载是 `cols u16 || rows u16 || cell_w u16 || cell_h u16`（全小端）。
    Resize = 3,
    /// 流控信用，负载是 `u32` 小端：又允许多少字节可以发过来。
    Credit = 4,
    /// 心跳，无负载。用于让客户端尽早发现守护进程已经没了。
    Ping = 5,
}

impl Kind {
    fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::Json,
            1 => Self::Data,
            2 => Self::Close,
            3 => Self::Resize,
            4 => Self::Credit,
            5 => Self::Ping,
            _ => return None,
        })
    }
}

/// 一帧。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub channel: u32,
    pub kind: Kind,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(channel: u32, kind: Kind, payload: impl Into<Vec<u8>>) -> Self {
        Self {
            channel,
            kind,
            payload: payload.into(),
        }
    }

    /// 控制面的 JSON 请求。
    pub fn json(payload: impl Into<Vec<u8>>) -> Self {
        Self::new(CONTROL, Kind::Json, payload)
    }

    /// 终端数据。
    pub fn data(channel: u32, payload: impl Into<Vec<u8>>) -> Self {
        Self::new(channel, Kind::Data, payload)
    }

    /// 尺寸帧的负载编码。
    pub fn encode_resize(cols: u16, rows: u16, cell_width: u16, cell_height: u16) -> Vec<u8> {
        let mut out = Vec::with_capacity(RESIZE_PAYLOAD_LEN);
        out.extend_from_slice(&cols.to_le_bytes());
        out.extend_from_slice(&rows.to_le_bytes());
        out.extend_from_slice(&cell_width.to_le_bytes());
        out.extend_from_slice(&cell_height.to_le_bytes());
        out
    }

    /// 解析尺寸帧；负载长度不对时返回 `None`。
    pub fn decode_resize(payload: &[u8]) -> Option<(u16, u16, u16, u16)> {
        if payload.len() != RESIZE_PAYLOAD_LEN {
            return None;
        }
        let cols = u16::from_le_bytes(payload[0..2].try_into().ok()?);
        let rows = u16::from_le_bytes(payload[2..4].try_into().ok()?);
        let w = u16::from_le_bytes(payload[4..6].try_into().ok()?);
        let h = u16::from_le_bytes(payload[6..8].try_into().ok()?);
        Some((cols, rows, w, h))
    }

    /// 写成一帧。帧头与负载分两次写：调用方通常持有 `BufWriter`，
    /// 合并不了两次 `write_all` 的开销，但能少一次拷贝。
    pub fn write_to(&self, w: &mut impl Write) -> io::Result<()> {
        let mut header = [0u8; HEADER_LEN];
        header[0..4].copy_from_slice(&self.channel.to_le_bytes());
        header[4] = self.kind as u8;
        header[5..9].copy_from_slice(&(self.payload.len() as u32).to_le_bytes());
        w.write_all(&header)?;
        w.write_all(&self.payload)?;
        w.flush()
    }
}

/// 从 `r` 读一帧；对面正常关闭时返回 `Ok(None)`。
///
/// 帧头完整但负载被截断时报 `UnexpectedEof`——那是真的坏了，
/// 不能当成"对面干净地关了"。
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<Frame>> {
    let mut header = [0u8; HEADER_LEN];
    match r.read_exact(&mut header) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let channel = u32::from_le_bytes(header[0..4].try_into().expect("4 字节"));
    let kind_raw = header[4];
    let len = u32::from_le_bytes(header[5..9].try_into().expect("4 字节")) as usize;

    if len > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("帧负载 {len} 字节超过上限 {MAX_PAYLOAD}"),
        ));
    }
    let kind = Kind::from_u8(kind_raw).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("未知的帧类型 {kind_raw}"),
        )
    })?;

    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok(Some(Frame {
        channel,
        kind,
        payload,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// 帧必须原样往返：channel / kind / payload 一个都不能丢。
    ///
    /// 回归价值：`read_frame` 里 channel 与 len 的偏移量只差一个字节，
    /// 写错了会读写自洽而跨进程不自洽——单元测试用同一套读写函数时
    /// 看不出来，只有断言具体的字节布局才抓得住。
    #[test]
    fn frames_round_trip() {
        let original = Frame::new(7, Kind::Data, b"hello".to_vec());
        let mut buf = Vec::new();
        original.write_to(&mut buf).unwrap();

        assert_eq!(buf.len(), HEADER_LEN + 5);
        assert_eq!(&buf[0..4], &7u32.to_le_bytes());
        assert_eq!(buf[4], Kind::Data as u8);
        assert_eq!(&buf[5..9], &5u32.to_le_bytes());
        assert_eq!(&buf[9..], b"hello");

        let frame = read_frame(&mut Cursor::new(buf)).unwrap().unwrap();
        assert_eq!(frame, original);
    }

    /// channel 0 上的 JSON 控制帧可往返。
    #[test]
    fn control_frames_round_trip() {
        let original = Frame::json(b"{\"op\":\"list\"}".to_vec());
        let mut buf = Vec::new();
        original.write_to(&mut buf).unwrap();
        assert_eq!(original.channel, CONTROL);

        let frame = read_frame(&mut Cursor::new(buf)).unwrap().unwrap();
        assert_eq!(frame.channel, CONTROL);
        assert_eq!(frame.kind, Kind::Json);
        assert_eq!(frame.payload, b"{\"op\":\"list\"}");
    }

    /// 空负载合法（心跳）。
    #[test]
    fn empty_payload_is_valid() {
        let mut buf = Vec::new();
        Frame::new(3, Kind::Ping, Vec::new())
            .write_to(&mut buf)
            .unwrap();
        let frame = read_frame(&mut Cursor::new(buf)).unwrap().unwrap();
        assert_eq!(frame.kind, Kind::Ping);
        assert!(frame.payload.is_empty());
    }

    /// 对面在帧边界关闭是正常结束，不是错误。
    #[test]
    fn clean_eof_at_a_frame_boundary_is_none() {
        assert!(read_frame(&mut Cursor::new(Vec::new())).unwrap().is_none());
    }

    /// 帧头说完长度却没有负载：这是截断，必须报错。
    ///
    /// 若这里返回 `None`，客户端会把「守护进程写到一半崩了」当成
    /// 「守护进程正常退出」，从而放弃重连。
    #[test]
    fn truncated_payload_is_an_error_not_a_clean_close() {
        let mut buf = Vec::new();
        let mut header = [0u8; HEADER_LEN];
        header[5..9].copy_from_slice(&100u32.to_le_bytes());
        buf.extend_from_slice(&header);
        buf.extend_from_slice(b"short");

        let err = read_frame(&mut Cursor::new(buf)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    /// 超过上限的长度必须拒绝，且**不能**去分配那块内存。
    ///
    /// 回归价值：没有这道闸，一个被写坏的帧头（比如把 0x10000000 当成
    /// 长度）会让进程直接 `vec![0; 256MB]` 然后 OOM。
    #[test]
    fn absurd_payload_length_is_rejected() {
        let mut header = [0u8; HEADER_LEN];
        header[5..9].copy_from_slice(&((MAX_PAYLOAD + 1) as u32).to_le_bytes());

        let err = read_frame(&mut Cursor::new(header.to_vec())).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(
            err.to_string().contains("超过上限"),
            "错误信息要说清是长度超限: {err}"
        );
    }

    /// 未知的帧类型必须报错而不是静默丢弃。
    ///
    /// 静默丢弃会让协议版本漂移变成「界面偶尔不动」，极难排查。
    #[test]
    fn unknown_frame_kind_is_rejected() {
        let mut buf = vec![0u8; HEADER_LEN];
        buf[4] = 200;
        let err = read_frame(&mut Cursor::new(buf)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    /// 尺寸帧的编码与解码必须对称。
    #[test]
    fn resize_payload_round_trips() {
        let payload = Frame::encode_resize(174, 66, 9, 18);
        assert_eq!(payload.len(), RESIZE_PAYLOAD_LEN);
        assert_eq!(Frame::decode_resize(&payload), Some((174, 66, 9, 18)));
    }

    /// 尺寸帧长度不对时解码返回 `None`，不 panic。
    #[test]
    fn malformed_resize_payload_is_none() {
        assert_eq!(Frame::decode_resize(&[]), None);
        assert_eq!(Frame::decode_resize(&[0u8; 7]), None);
        assert_eq!(Frame::decode_resize(&[0u8; 9]), None);
    }
}
